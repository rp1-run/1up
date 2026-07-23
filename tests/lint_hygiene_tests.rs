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

/// Replace comments and the *contents* of string/char literals with spaces,
/// preserving newlines so downstream line numbers stay correct. This removes
/// the lexer trivia Rust permits between attribute tokens (so
/// `#! /* gap */ [allow /* gap */ (dead_code)]` cannot dodge the scanner) and
/// blanks `reason = "…"` text (so prose containing "warnings" cannot falsely
/// flag an unrelated allow).
fn strip_comments_and_string_contents(content: &str) -> String {
    let b = content.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;

    let blank = |out: &mut Vec<u8>, slice: &[u8]| {
        out.extend(slice.iter().map(|&c| if c == b'\n' { b'\n' } else { b' ' }));
    };

    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let end = content[i..].find('\n').map_or(b.len(), |off| i + off);
                blank(&mut out, &b[i..end]);
                i = end;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // Rust block comments nest.
                let mut depth = 1usize;
                let mut j = i + 2;
                while j < b.len() && depth > 0 {
                    if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                        depth += 1;
                        j += 2;
                    } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                        depth -= 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                blank(&mut out, &b[i..j]);
                i = j;
            }
            b'"' => {
                out.push(b'"');
                let mut j = i + 1;
                while j < b.len() {
                    match b[j] {
                        b'\\' => j += 2,
                        b'"' => break,
                        _ => j += 1,
                    }
                }
                let end = j.min(b.len());
                blank(&mut out, &b[i + 1..end]);
                if end < b.len() {
                    out.push(b'"');
                    i = end + 1;
                } else {
                    i = end;
                }
            }
            b'r' if matches!(b.get(i + 1), Some(&b'"') | Some(&b'#')) => {
                // Raw string r"…" / r#"…"# (any number of #s).
                let mut hashes = 0;
                let mut j = i + 1;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) == Some(&b'"') {
                    let open_end = j + 1;
                    let closer: Vec<u8> = std::iter::once(b'"')
                        .chain(std::iter::repeat_n(b'#', hashes))
                        .collect();
                    let close = content[open_end..]
                        .find(std::str::from_utf8(&closer).unwrap())
                        .map_or(b.len(), |off| open_end + off);
                    out.extend_from_slice(&b[i..open_end]);
                    blank(&mut out, &b[open_end..close]);
                    let end = (close + closer.len()).min(b.len());
                    out.extend_from_slice(&b[close.min(b.len())..end]);
                    i = end;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            b'\'' => {
                // Char literal ('x', '\n', '\u{…}') vs lifetime ('a). Treat as a
                // char literal only when it demonstrably closes.
                let is_escape = b.get(i + 1) == Some(&b'\\');
                let closes_short = b.get(i + 2) == Some(&b'\'');
                if is_escape || closes_short {
                    let mut j = i + 1;
                    if is_escape {
                        j += 2; // skip backslash + escaped char
                        while j < b.len() && b[j] != b'\'' {
                            j += 1; // \u{…} style escapes
                        }
                    } else {
                        j += 1;
                    }
                    let end = (j + 1).min(b.len());
                    out.push(b'\'');
                    blank(&mut out, &b[i + 1..end.saturating_sub(1)]);
                    if end > i + 1 {
                        out.push(b'\'');
                    }
                    i = end;
                } else {
                    out.push(b'\'');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Find inner (`#![…]`) attributes in comment-stripped text, including ones
/// whose tokens are separated by newlines or (former) comment gaps, and return
/// `(line_number, normalized)` for each.
fn inner_attributes(content: &str) -> Vec<(usize, String)> {
    let content = strip_comments_and_string_contents(content);
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

fn blanket_violations(content: &str) -> Vec<(usize, String)> {
    inner_attributes(content)
        .into_iter()
        .filter(|(_, attr)| {
            // Unconditional inner allow attributes only; `#![cfg_attr(...)]`
            // (platform-conditional) and item-level `#[allow(...)]` do not match.
            attr.strip_prefix("allow(").is_some_and(|args| {
                args.contains("dead_code") || args.contains("unused") || args.contains("warnings")
            })
        })
        .collect()
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
        for (line, attr) in blanket_violations(&content) {
            violations.push(format!("{}:{}: #![{}]", path.display(), line, attr));
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
    assert_eq!(
        blanket_violations("#![allow(\n    dead_code\n)]\nfn f() {}\n"),
        vec![(1, "allow(dead_code)".to_string())]
    );

    // Whitespace between `#!` and `[`, and the `warnings` group.
    assert_eq!(
        blanket_violations("#!\n[allow(warnings)]\n"),
        vec![(1, "allow(warnings)".to_string())]
    );

    // Block-comment trivia between tokens (valid Rust) cannot dodge the scan.
    assert_eq!(
        blanket_violations("#! /* gap */ [allow(dead_code)]\n"),
        vec![(1, "allow(dead_code)".to_string())]
    );
    assert_eq!(
        blanket_violations("#![allow /* gap */ (dead_code)]\n"),
        vec![(1, "allow(dead_code)".to_string())]
    );
    assert_eq!(
        blanket_violations("#![allow(/* nested /* comment */ */ unused_imports)]\n"),
        vec![(1, "allow(unused_imports)".to_string())]
    );

    // Line-comment trivia inside a multiline attribute.
    assert_eq!(
        blanket_violations("#![allow( // why not\n    dead_code\n)]\n"),
        vec![(1, "allow(dead_code)".to_string())]
    );
}

#[test]
fn inner_attribute_scanner_permits_legitimate_forms() {
    // Conditional cfg_attr blankets are permitted.
    assert!(blanket_violations("#![cfg_attr(not(unix), allow(dead_code))]\n").is_empty());

    // Item-level (outer) attributes are ignored entirely.
    assert!(blanket_violations("#[allow(dead_code)]\nfn f() {}\n").is_empty());

    // A banned lint name inside reason prose must not flag an unrelated allow.
    assert!(blanket_violations(
        "#![allow(clippy::print_stdout, reason = \"CLI prints warnings to users\")]\n"
    )
    .is_empty());

    // Banned lint names inside comments or string literals must not flag.
    assert!(blanket_violations("// #![allow(dead_code)]\nfn f() {}\n").is_empty());
    assert!(blanket_violations("const S: &str = \"#![allow(dead_code)]\";\n").is_empty());
    assert!(blanket_violations("const S: &str = r#\"#![allow(dead_code)]\"#;\n").is_empty());

    // Char literals (including an escaped quote) must not derail string tracking.
    assert!(blanket_violations("let q = '\"'; let e = '\\n';\n").is_empty());
}
