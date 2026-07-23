//! Lint-hygiene guard: forbid module-level dead-code suppression in `src/`.
//!
//! Whole-module `#![allow(dead_code)]`/`#![allow(unused…)]`/`#![allow(warnings)]`
//! blankets erase the compiler's ability to distinguish live from dead code
//! inside live files and let new rot accumulate silently. Item-level
//! `#[allow(dead_code)]` with a stated reason remains fine (this crate compiles
//! its modules both as a lib and as the `1up` bin, so items consumed only via
//! the lib target by `tests/`/`benches/` legitimately need targeted allows).
//! Platform-conditional blankets like `#![cfg_attr(not(unix), allow(dead_code))]`
//! are also fine: they scope the suppression to a configuration where the
//! module is a stub.

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

/// Find inner (`#![…]`) attributes anywhere in `content`, including ones whose
/// argument list spans multiple lines, and return `(line_number, normalized)`
/// for each. Matching runs on raw text, so a violation quoted inside a string
/// literal would also be flagged — acceptable for a guard (fails closed).
fn inner_attributes(content: &str) -> Vec<(usize, String)> {
    let bytes = content.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while let Some(off) = content[i..].find("#!") {
        let start = i + off;
        let mut j = start + 2;
        while j < bytes.len() && (bytes[j] as char).is_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'[' {
            i = start + 2;
            continue;
        }
        // Capture to the matching close bracket, tracking nesting.
        let mut depth = 0usize;
        let mut end = None;
        for (k, &b) in bytes.iter().enumerate().skip(j) {
            match b {
                b'[' | b'(' => depth += 1,
                b']' | b')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = Some(k + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        let line = content[..start].matches('\n').count() + 1;
        let normalized: String = content[j + 1..end - 1]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        found.push((line, normalized));
        i = end;
    }
    found
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
        for (line, attr) in inner_attributes(&content) {
            // Unconditional inner allow attributes only; `#![cfg_attr(...)]`
            // (platform-conditional) and item-level `#[allow(...)]` do not match.
            if let Some(args) = attr.strip_prefix("allow(") {
                if args.contains("dead_code")
                    || args.contains("unused")
                    || args.contains("warnings")
                {
                    violations.push(format!("{}:{}: #![{}]", path.display(), line, attr));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "module-level dead-code/unused/warnings suppression is forbidden in src/; \
         use item-level #[allow(dead_code, reason = \"...\")] or delete the dead code:\n{}",
        violations.join("\n")
    );
}

#[test]
fn inner_attribute_scanner_catches_evasions() {
    // Multiline argument list.
    let multiline = "#![allow(\n    dead_code\n)]\nfn f() {}\n";
    let attrs = inner_attributes(multiline);
    assert_eq!(attrs, vec![(1, "allow(dead_code)".to_string())]);

    // Whitespace between `#!` and `[`, and the `warnings` group.
    let spaced = "#!\n[allow(warnings)]\n";
    assert_eq!(
        inner_attributes(spaced),
        vec![(1, "allow(warnings)".to_string())]
    );

    // Conditional cfg_attr form is captured but not an `allow(` prefix match.
    let conditional = "#![cfg_attr(not(unix), allow(dead_code))]\n";
    let attrs = inner_attributes(conditional);
    assert_eq!(attrs.len(), 1);
    assert!(!attrs[0].1.starts_with("allow("));

    // Item-level (outer) attributes are ignored entirely.
    assert!(inner_attributes("#[allow(dead_code)]\nfn f() {}\n").is_empty());
}
