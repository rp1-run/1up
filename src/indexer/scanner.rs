use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

use crate::shared::errors::{IndexingError, OneupError};

const BINARY_EXTENSIONS: &[&str] = &[
    // Images
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "svg", "webp", "tiff", "tif", "psd", "pxd", "ai",
    "sketch", "fig", "xcf", "raw", "cr2", "nef", "heic", "heif", "avif", // Audio/video
    "mp3", "mp4", "avi", "mov", "wav", "flac", "ogg", "mkv", "wmv", "webm", "aac", "m4a",
    // Archives
    "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "zst", "lz4", "zstd",
    // Compiled/binary
    "exe", "dll", "so", "dylib", "bin", "obj", "o", "a", "lib", "wasm", "pyc", "pyo", "class",
    // Documents
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", // Fonts
    "ttf", "otf", "woff", "woff2", "eot", // Databases/lockfiles
    "db", "sqlite", "sqlite3", "lock", // Data/serialized
    "parquet", "arrow", "pkl", "pickle", "npy", "npz", "h5", "hdf5",
    // Disk images / packages
    "dmg", "iso", "deb", "rpm", "msi", "apk", "ipa",
];

const DEFAULT_IGNORE_DIRS: &[&str] = &[
    "!node_modules/",
    "!.git/",
    "!vendor/",
    "!target/",
    "!build/",
    "!dist/",
    "!out/",
    "!.next/",
    "!.nuxt/",
    "!__pycache__/",
    "!.venv/",
    "!venv/",
    "!.tox/",
    "!.mypy_cache/",
    "!.pytest_cache/",
    "!.cargo/",
    "!.gradle/",
    "!.idea/",
    "!.vscode/",
    "!.1up/",
    "!.rp1/",
    "!coverage/",
];

/// A scanned file entry with its path, detected extension, and filesystem metadata.
#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub extension: String,
    pub file_size: u64,
    pub modified_ns: i64,
}

fn detect_file_type(path: &Path) -> Option<String> {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_lowercase();
        if !ext.is_empty() {
            return Some(ext);
        }
    }

    let name = path.file_name()?.to_str()?.to_lowercase();
    match name.as_str() {
        "dockerfile" | "makefile" | "justfile" => Some(name),
        _ => None,
    }
}

fn is_binary_extension(extension: &str) -> bool {
    BINARY_EXTENSIONS.contains(&extension)
}

fn indexable_extension(path: &Path) -> Option<String> {
    let extension = detect_file_type(path)?;
    if is_binary_extension(extension.as_str()) {
        return None;
    }
    Some(extension)
}

pub fn is_scannable_file(path: &Path) -> bool {
    indexable_extension(path).is_some()
}

fn build_walker(root: &Path, path: &Path) -> Result<WalkBuilder, OneupError> {
    let mut overrides = OverrideBuilder::new(root);
    for pattern in DEFAULT_IGNORE_DIRS {
        overrides.add(pattern).map_err(|e| {
            IndexingError::Scan(format!("invalid ignore pattern '{}': {e}", pattern))
        })?;
    }
    let overrides = overrides
        .build()
        .map_err(|e| IndexingError::Scan(format!("failed to build overrides: {e}")))?;

    let mut builder = WalkBuilder::new(path);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .overrides(overrides);
    Ok(builder)
}

fn file_modified_ns(metadata: &std::fs::Metadata) -> i64 {
    use std::time::UNIX_EPOCH;
    metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|dur| dur.as_nanos() as i64)
        .unwrap_or(0)
}

fn collect_scanned_files(walker: ignore::Walk) -> Result<Vec<ScannedFile>, OneupError> {
    let mut files = Vec::new();

    for entry in walker {
        let entry = entry.map_err(|e| IndexingError::Scan(format!("directory walk error: {e}")))?;

        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let path = entry.path().to_path_buf();
        let Some(extension) = indexable_extension(&path) else {
            continue;
        };

        let (file_size, modified_ns) = match std::fs::metadata(&path) {
            Ok(meta) => (meta.len(), file_modified_ns(&meta)),
            Err(_) => (0, 0),
        };

        files.push(ScannedFile {
            path,
            extension,
            file_size,
            modified_ns,
        });
    }

    Ok(files)
}

/// Scan a directory for source files, respecting .gitignore and default ignores.
///
/// Returns a list of files with their extensions, skipping binary files,
/// hidden directories, and common build artifact directories.
pub fn scan_directory(root: &Path) -> Result<Vec<ScannedFile>, OneupError> {
    collect_scanned_files(build_walker(root, root)?.build())
}

pub fn scan_paths(
    root: &Path,
    relative_paths: &BTreeSet<PathBuf>,
) -> Result<Vec<ScannedFile>, OneupError> {
    if relative_paths.is_empty() {
        return Ok(Vec::new());
    }

    let root = root.to_path_buf();
    let target_paths = relative_paths.clone();
    let mut walker = build_walker(&root, &root)?;
    walker.filter_entry(move |entry| {
        let Ok(relative_path) = entry.path().strip_prefix(&root) else {
            return false;
        };

        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            target_paths
                .iter()
                .any(|target_path| target_path.starts_with(relative_path))
        } else {
            target_paths.contains(relative_path)
        }
    });

    collect_scanned_files(walker.build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_finds_source_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.path().join("lib.py"), "def foo(): pass").unwrap();
        fs::write(tmp.path().join("readme.md"), "# Readme").unwrap();

        let files = scan_directory(tmp.path()).unwrap();
        assert_eq!(files.len(), 3);

        let extensions: Vec<&str> = files.iter().map(|f| f.extension.as_str()).collect();
        assert!(extensions.contains(&"rs"));
        assert!(extensions.contains(&"py"));
        assert!(extensions.contains(&"md"));
    }

    #[test]
    fn scan_skips_binary_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.path().join("image.png"), [0u8; 100]).unwrap();
        fs::write(tmp.path().join("archive.zip"), [0u8; 50]).unwrap();

        let files = scan_directory(tmp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].extension, "rs");
    }

    #[test]
    fn scan_skips_node_modules() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("index.js"), "module.exports = {}").unwrap();
        let nm = tmp.path().join("node_modules").join("pkg");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("index.js"), "// dep").unwrap();

        let files = scan_directory(tmp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, tmp.path().join("index.js"));
    }

    #[test]
    fn scan_skips_files_without_extension() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Makefile"), "all:").unwrap();
        fs::write(tmp.path().join("main.go"), "package main").unwrap();
        fs::write(tmp.path().join("notes"), "plain text").unwrap();

        let files = scan_directory(tmp.path()).unwrap();
        assert_eq!(files.len(), 2);

        let extensions: Vec<&str> = files.iter().map(|f| f.extension.as_str()).collect();
        assert!(extensions.contains(&"go"));
        assert!(extensions.contains(&"makefile"));
        assert!(!extensions.contains(&"notes"));
    }

    #[test]
    fn scan_respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        // The ignore crate requires a git repo for .gitignore to take effect
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.path().join("ignored.rs"), "fn ignored() {}").unwrap();

        let files = scan_directory(tmp.path()).unwrap();
        let names: Vec<&str> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(names.contains(&"main.rs"));
        assert!(!names.contains(&"ignored.rs"));
    }

    #[test]
    fn scan_paths_respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.path().join("ignored.rs"), "fn ignored() {}").unwrap();

        let paths = BTreeSet::from([PathBuf::from("main.rs"), PathBuf::from("ignored.rs")]);

        let files = scan_paths(tmp.path(), &paths).unwrap();
        let names: Vec<&str> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(names.contains(&"main.rs"));
        assert!(!names.contains(&"ignored.rs"));
    }

    #[test]
    fn scan_paths_respects_hidden_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.path().join(".hidden.rs"), "fn hidden() {}").unwrap();

        let paths = BTreeSet::from([PathBuf::from("main.rs"), PathBuf::from(".hidden.rs")]);

        let files = scan_paths(tmp.path(), &paths).unwrap();
        let names: Vec<&str> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(names.contains(&"main.rs"));
        assert!(!names.contains(&".hidden.rs"));
    }

    #[test]
    fn scan_paths_respects_git_exclude() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".git").join("info")).unwrap();
        fs::write(
            tmp.path().join(".git").join("info").join("exclude"),
            "ignored.rs\n",
        )
        .unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.path().join("ignored.rs"), "fn ignored() {}").unwrap();

        let paths = BTreeSet::from([PathBuf::from("main.rs"), PathBuf::from("ignored.rs")]);

        let files = scan_paths(tmp.path(), &paths).unwrap();
        let names: Vec<&str> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(names.contains(&"main.rs"));
        assert!(!names.contains(&"ignored.rs"));
    }

    #[test]
    fn scan_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let files = scan_directory(tmp.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn scan_skips_target_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
        let target = tmp.path().join("target").join("debug");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("build.rs"), "// build script").unwrap();

        let files = scan_directory(tmp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].extension, "rs");
    }

    #[test]
    fn scan_recognizes_special_filenames_without_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Dockerfile"), "FROM rust:1.0").unwrap();
        fs::write(tmp.path().join("justfile"), "fmt:\n  cargo fmt").unwrap();

        let files = scan_directory(tmp.path()).unwrap();
        let extensions: Vec<&str> = files.iter().map(|f| f.extension.as_str()).collect();
        assert!(extensions.contains(&"dockerfile"));
        assert!(extensions.contains(&"justfile"));
    }
}
