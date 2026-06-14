//! Pure, filesystem-free classification of legacy 1up code-discovery hints in a
//! user instruction file's content.
//!
//! Per binding requirement EDIT-001, the destructive (auto-remove) path is
//! restricted to a span 1up can deterministically prove it owns: a matched
//! `<!-- 1up:hint:begin ... -->` / `<!-- 1up:hint:end -->` HTML-comment fence
//! pair. Every other legacy signal -- specifically a stale `oneup_*` token that
//! is not inside such a fence -- is reported as a detect-and-advise finding and
//! never edited. A token is "stale" iff it has the `oneup_` prefix but is absent
//! from the live [`RETAINED_PUBLIC_TOOLS`], which is the single source of truth
//! for what a real tool is (today the stale set is `oneup_prepare`/`oneup_read`).
//!
//! This module is intentionally free of any filesystem, CLI, or async
//! dependency so the classification and the byte-exact fence-removal transform
//! can be unit-tested in isolation. The `doctor` command layer is responsible
//! for reading files, rendering reports, and performing the gated write.

// The `doctor` command (built in a later task) is the only non-test consumer of
// `classify`. Until that wiring lands, the bin target sees this leaf module as
// unused; the unit tests below exercise the full surface. Remove once `doctor`
// calls `classify`.
#![allow(dead_code)]

use std::ops::Range;

use crate::mcp::types::RETAINED_PUBLIC_TOOLS;

/// Begin marker prefix for a 1up-owned hint fence. Versioned attributes (e.g.
/// `v=1`) may follow before the closing `-->`, so recognition is prefix-based
/// to stay forward-compatible across fence versions.
const FENCE_BEGIN_PREFIX: &str = "<!-- 1up:hint:begin";
/// End marker prefix for a 1up-owned hint fence.
const FENCE_END_PREFIX: &str = "<!-- 1up:hint:end";
/// Common closing of an HTML-comment marker line.
const COMMENT_CLOSE: &str = "-->";
/// Prefix that identifies a candidate MCP tool token.
const TOKEN_PREFIX: &str = "oneup_";

/// A single stale-token occurrence found outside any 1up-owned fence. Reported
/// for detect-and-advise; it is never auto-edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The full stale token, e.g. `oneup_prepare`.
    pub token: String,
    /// 1-based line number where the token occurs.
    pub line: usize,
}

/// Outcome of classifying one instruction file's content.
///
/// `changed` is true only when an owned fence was removed (the sole destructive
/// path). Unfenced findings alone never set `changed`: they are advisory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReport {
    /// Half-open line-index range `[begin, end)` of the matched 1up-owned fence
    /// (begin marker line through end marker line, inclusive), when one exists.
    /// `None` means no deterministically-owned fence was found.
    pub fenced_span: Option<Range<usize>>,
    /// Stale `oneup_*` tokens located outside any owned fence, for advisory
    /// reporting. Order follows their appearance in the content.
    pub unfenced_findings: Vec<Finding>,
    /// The content after removing the owned fence (and one immediately trailing
    /// blank line, to avoid leaving a double blank). Byte-for-byte identical to
    /// the input outside the removed span. Equals the input when nothing was
    /// removed.
    pub cleaned: String,
    /// True iff `cleaned` differs from the input, i.e. an owned fence was
    /// removed. Advisory unfenced findings do not set this.
    pub changed: bool,
}

/// Classify one instruction file's `content`.
///
/// Removes at most the first matched 1up-owned fence pair (1up writes at most
/// one such fence), preserving every other byte exactly, and reports any stale
/// `oneup_*` tokens that live outside that fence as advisory findings. Running
/// `classify` again over the returned `cleaned` string is a no-op
/// (`changed == false`).
pub fn classify(content: &str) -> FileReport {
    let lines = split_keep_endings(content);
    let fence = find_owned_fence(&lines);

    let (cleaned, removed_bytes) = match &fence {
        Some(span) => remove_fence(&lines, span),
        None => (content.to_string(), None),
    };

    let unfenced_findings = collect_unfenced_findings(content, removed_bytes);
    let changed = cleaned != content;

    FileReport {
        fenced_span: fence,
        unfenced_findings,
        cleaned,
        changed,
    }
}

/// Split `content` into physical lines whose slices, concatenated in order,
/// reproduce `content` exactly. Each returned slice includes its trailing
/// newline when present; the final slice omits one only if `content` did not end
/// with a newline. This preserves `\r\n` endings and a missing final newline so
/// reconstruction stays byte-exact.
fn split_keep_endings(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = content.as_bytes();
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            lines.push(&content[start..=idx]);
            start = idx + 1;
        }
    }
    if start < content.len() {
        lines.push(&content[start..]);
    }
    lines
}

/// Return the line content with a single trailing `\n` and optional preceding
/// `\r` removed, leaving any other interior bytes untouched.
fn line_body(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .unwrap_or(line)
}

/// A line is a 1up-owned fence begin marker when, ignoring surrounding
/// whitespace, it starts with the begin prefix and closes the HTML comment.
fn is_fence_begin(line: &str) -> bool {
    let body = line_body(line).trim();
    body.starts_with(FENCE_BEGIN_PREFIX) && body.ends_with(COMMENT_CLOSE)
}

/// A line is a 1up-owned fence end marker when, ignoring surrounding whitespace,
/// it starts with the end prefix and closes the HTML comment.
fn is_fence_end(line: &str) -> bool {
    let body = line_body(line).trim();
    body.starts_with(FENCE_END_PREFIX) && body.ends_with(COMMENT_CLOSE)
}

/// A line is blank when its body (excluding the line ending) is empty or only
/// whitespace.
fn is_blank(line: &str) -> bool {
    line_body(line).trim().is_empty()
}

/// Locate the first deterministically-owned fence: a begin marker line matched
/// by a later end marker line. Returns the half-open line-index range
/// `[begin, end)` covering the begin marker through the end marker inclusive.
/// Unmatched or partial markers yield `None` (not owned).
fn find_owned_fence(lines: &[&str]) -> Option<Range<usize>> {
    let begin = lines.iter().position(|l| is_fence_begin(l))?;
    let end_rel = lines[begin + 1..].iter().position(|l| is_fence_end(l))?;
    let end = begin + 1 + end_rel;
    Some(begin..end + 1)
}

/// Rebuild content with the owned fence span removed, plus one immediately
/// trailing blank line if present (to avoid leaving a double blank). Returns the
/// cleaned string and the byte range of the original content that was removed, so
/// findings inside the fence can be excluded from advisories.
fn remove_fence(lines: &[&str], span: &Range<usize>) -> (String, Option<Range<usize>>) {
    let mut drop_end = span.end;
    if lines.get(drop_end).is_some_and(|l| is_blank(l)) {
        drop_end += 1;
    }

    let removed_byte_start = byte_offset_of_line(lines, span.start);
    let removed_byte_end = byte_offset_of_line(lines, drop_end);

    let mut cleaned = String::new();
    for (idx, line) in lines.iter().enumerate() {
        if idx < span.start || idx >= drop_end {
            cleaned.push_str(line);
        }
    }

    (cleaned, Some(removed_byte_start..removed_byte_end))
}

/// Byte offset in the original content where the line at `line_idx` begins. An
/// index at or past the end maps to the total content length.
fn byte_offset_of_line(lines: &[&str], line_idx: usize) -> usize {
    lines.iter().take(line_idx).map(|l| l.len()).sum()
}

/// Collect stale `oneup_*` tokens in `content` whose byte offset is not inside
/// `excluded` (the removed fence span). Mirrors `extract_oneup_tokens`
/// (`tests/release_assets_tests.rs`): a literal `oneup_` prefix followed by a
/// maximal run of `[a-z_]`, scanned left-to-right with non-overlapping matches.
fn collect_unfenced_findings(content: &str, excluded: Option<Range<usize>>) -> Vec<Finding> {
    let bytes = content.as_bytes();
    let mut findings = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = content[search_from..].find(TOKEN_PREFIX) {
        let start = search_from + rel;
        let mut end = start + TOKEN_PREFIX.len();
        while end < bytes.len() && (bytes[end].is_ascii_lowercase() || bytes[end] == b'_') {
            end += 1;
        }
        if end > start + TOKEN_PREFIX.len() {
            let token = &content[start..end];
            let inside_fence = excluded.as_ref().is_some_and(|r| r.contains(&start));
            if !inside_fence && is_stale_token(token) {
                findings.push(Finding {
                    token: token.to_string(),
                    line: line_number_at(content, start),
                });
            }
            search_from = end;
        } else {
            search_from = start + TOKEN_PREFIX.len();
        }
    }
    findings
}

/// A token is stale iff it has the `oneup_` prefix but is not a currently
/// retained public tool. `RETAINED_PUBLIC_TOOLS` is the authority, so adding or
/// removing a real tool keeps this correct without a hardcoded stale list.
fn is_stale_token(token: &str) -> bool {
    token.starts_with(TOKEN_PREFIX) && !RETAINED_PUBLIC_TOOLS.contains(&token)
}

/// 1-based line number of the byte at `offset` in `content`.
fn line_number_at(content: &str, offset: usize) -> usize {
    content[..offset].bytes().filter(|&b| b == b'\n').count() + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fenced(body: &str) -> String {
        format!("<!-- 1up:hint:begin v=1 -->\n{body}\n<!-- 1up:hint:end -->\n")
    }

    #[test]
    fn clean_file_is_a_noop() {
        let content = "# My project\n\nSome notes about the build.\n";
        let report = classify(content);

        assert!(!report.changed);
        assert_eq!(report.cleaned, content);
        assert_eq!(report.fenced_span, None);
        assert!(report.unfenced_findings.is_empty());
    }

    #[test]
    fn fence_removal_preserves_surrounding_bytes() {
        let before = "# Title\n\nUser line one.\n";
        let after = "More user content.\n\nFinal line.\n";
        let content = format!(
            "{before}{}{after}",
            fenced("use oneup_prepare and oneup_read")
        );

        let report = classify(&content);

        assert!(report.changed);
        assert!(report.fenced_span.is_some());
        // Surrounding content is byte-for-byte preserved; only the fence (and
        // its single trailing blank) is gone.
        assert_eq!(report.cleaned, format!("{before}{after}"));
        // Stale tokens lived inside the fence, so they are removed, not advised.
        assert!(report.unfenced_findings.is_empty());
    }

    #[test]
    fn second_classify_over_cleaned_is_idempotent() {
        let content = format!(
            "# Title\n\n{}\nTrailing user note.\n",
            fenced("oneup_prepare hint")
        );

        let first = classify(&content);
        assert!(first.changed);

        let second = classify(&first.cleaned);
        assert!(!second.changed);
        assert_eq!(second.cleaned, first.cleaned);
        assert_eq!(second.fenced_span, None);
    }

    #[test]
    fn already_clean_file_with_no_hints_reports_no_change() {
        let content = "# Docs\n\nNothing to clean here.\n";
        let report = classify(content);

        assert!(!report.changed);
        assert_eq!(report.cleaned, content);
        assert!(report.fenced_span.is_none());
        assert!(report.unfenced_findings.is_empty());
    }

    #[test]
    fn unfenced_stale_token_is_advised_not_edited() {
        let content = "# Notes\n\nCall `oneup_prepare` then `oneup_read` to begin.\n";
        let report = classify(content);

        assert!(
            !report.changed,
            "unfenced content must never be auto-edited"
        );
        assert_eq!(report.cleaned, content);
        assert_eq!(report.fenced_span, None);

        let tokens: Vec<&str> = report
            .unfenced_findings
            .iter()
            .map(|f| f.token.as_str())
            .collect();
        assert_eq!(tokens, vec!["oneup_prepare", "oneup_read"]);
        assert_eq!(report.unfenced_findings[0].line, 3);
        assert_eq!(report.unfenced_findings[1].line, 3);
    }

    #[test]
    fn retained_tools_are_never_flagged_but_stale_ones_are() {
        for retained in RETAINED_PUBLIC_TOOLS {
            let content = format!("Use `{retained}` for discovery.\n");
            let report = classify(&content);
            assert!(
                report.unfenced_findings.is_empty(),
                "retained tool {retained} must not be flagged as stale"
            );
            assert!(!report.changed);
        }

        for stale in ["oneup_prepare", "oneup_read"] {
            let content = format!("Use `{stale}` first.\n");
            let report = classify(&content);
            let tokens: Vec<&str> = report
                .unfenced_findings
                .iter()
                .map(|f| f.token.as_str())
                .collect();
            assert_eq!(tokens, vec![stale], "{stale} must be flagged as stale");
        }
    }

    #[test]
    fn unmatched_begin_marker_is_not_owned() {
        // A begin marker with no closing end is a partial marker: not owned, so
        // nothing is removed and any stale token routes to advisories.
        let content = "<!-- 1up:hint:begin v=1 -->\nuse oneup_prepare\n";
        let report = classify(content);

        assert!(!report.changed);
        assert_eq!(report.cleaned, content);
        assert_eq!(report.fenced_span, None);
        let tokens: Vec<&str> = report
            .unfenced_findings
            .iter()
            .map(|f| f.token.as_str())
            .collect();
        assert_eq!(tokens, vec!["oneup_prepare"]);
    }

    #[test]
    fn fence_removal_collapses_one_trailing_blank_line() {
        let content = format!("# Title\n\n{}\nKeep me.\n", fenced("oneup_read"));
        let report = classify(&content);

        assert!(report.changed);
        // The blank line between the fence and "Keep me." is collapsed so no
        // double blank is left behind.
        assert_eq!(report.cleaned, "# Title\n\nKeep me.\n");
    }

    #[test]
    fn preserves_crlf_endings_and_missing_final_newline() {
        // No trailing newline on the final line, CRLF endings throughout.
        let content =
            "# Title\r\n\r\n<!-- 1up:hint:begin v=1 -->\r\noneup_prepare\r\n<!-- 1up:hint:end -->\r\nLast line no newline";
        let report = classify(content);

        assert!(report.changed);
        assert_eq!(report.cleaned, "# Title\r\n\r\nLast line no newline");
    }
}
