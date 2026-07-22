//! Lint-hygiene guard: forbid module-level dead-code suppression in `src/`.
//!
//! Whole-module `#![allow(dead_code)]`/`#![allow(unused…)]` blankets erase the
//! compiler's ability to distinguish live from dead code inside live files and
//! let new rot accumulate silently. Item-level `#[allow(dead_code)]` with a
//! stated reason remains fine (this crate compiles its modules both as a lib
//! and as the `1up` bin, so items consumed only via the lib target by
//! `tests/`/`benches/` legitimately need targeted allows). Platform-conditional
//! blankets like `#![cfg_attr(not(unix), allow(dead_code))]` are also fine:
//! they scope the suppression to a configuration where the module is a stub.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read_dir failed") {
        let path = entry.expect("dir entry failed").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_module_level_dead_code_blankets_in_src() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    assert!(!files.is_empty(), "no Rust sources found under src/");

    let mut violations = Vec::new();
    for path in files {
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        for (idx, line) in content.lines().enumerate() {
            let trimmed = line.trim_start();
            // Unconditional inner (module-level) allow attributes only;
            // `#![cfg_attr(...)]` and item-level `#[allow(...)]` do not match.
            if let Some(rest) = trimmed.strip_prefix("#![allow(") {
                if rest.contains("dead_code") || rest.contains("unused") {
                    violations.push(format!("{}:{}: {}", path.display(), idx + 1, trimmed));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "module-level dead-code/unused suppression is forbidden in src/; \
         use item-level #[allow(dead_code, reason = \"...\")] or delete the dead code:\n{}",
        violations.join("\n")
    );
}
