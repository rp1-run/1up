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

/// A scanned file entry with its path and detected extension.
#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub extension: String,
}

/// Scan a directory for source files, respecting .gitignore and default ignores.
///
/// Returns a list of files with their extensions, skipping binary files,
/// hidden directories, and common build artifact directories.
pub fn scan_directory(root: &Path) -> Result<Vec<ScannedFile>, OneupError> {
    let mut overrides = OverrideBuilder::new(root);
    for pattern in DEFAULT_IGNORE_DIRS {
        overrides.add(pattern).map_err(|e| {
            IndexingError::Scan(format!("invalid ignore pattern '{}': {e}", pattern))
        })?;
    }
    let overrides = overrides
        .build()
        .map_err(|e| IndexingError::Scan(format!("failed to build overrides: {e}")))?;

    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .overrides(overrides)
        .build();

    let mut files = Vec::new();

    for entry in walker {
        let entry = entry.map_err(|e| IndexingError::Scan(format!("directory walk error: {e}")))?;

        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let path = entry.path().to_path_buf();

        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        if extension.is_empty() {
            continue;
        }

        if BINARY_EXTENSIONS.contains(&extension.as_str()) {
            continue;
        }

        files.push(ScannedFile { path, extension });
    }

    Ok(files)
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

        let files = scan_directory(tmp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].extension, "go");
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
}
