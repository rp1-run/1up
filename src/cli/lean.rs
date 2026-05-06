//! Lean row renderer for plain discovery output.
//!
//! The module owns the single machine-parseable shape emitted by `search`,
//! `symbol --plain`, `impact --plain`, `context --plain`, `structural`, and `get --plain`. Each `render_*`
//! function writes directly to a `Write` sink and borrows its inputs, so
//! callers never pay for an intermediate `String` buffer.
//!
//! Row grammar (discovery commands):
//!
//! ```text
//! <score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>[  ~<channel>]
//! ```
//!
//! Fields are separated by two ASCII spaces (design D2). The `:<segment_id>`
//! suffix is the 12-char display handle (design D3). `~P`/`~C` channel
//! suffixes are exclusive to `impact` rows (design D5).

use std::io::Write;

use crate::search::impact::{
    ImpactCandidate, ImpactHint, ImpactResultEnvelope, ImpactStatus, ResolvedImpactAnchor,
};
use crate::shared::types::{ContextResult, SearchResult, StructuralResult, SymbolResult};
use crate::storage::segments::StoredSegment;

/// Number of hex characters rendered as the `:<segment_id>` display handle.
const SEGMENT_HANDLE_LEN: usize = 12;

/// Field separator for the lean row grammar (two ASCII spaces, D2).
const FIELD_SEP: &str = "  ";

/// Placeholder emitted when a row field is structurally required but absent
/// (e.g. a segment without a breadcrumb or defined symbol).
const PLACEHOLDER: &str = "-";

/// Render a slice of search hits as one lean row per hit.
pub fn render_search<W: Write>(sink: &mut W, results: &[SearchResult]) -> anyhow::Result<()> {
    for r in results {
        write_discovery_row(
            sink,
            r.score,
            &r.file_path,
            r.line_number,
            r.line_end,
            &r.block_type,
            r.breadcrumb.as_deref(),
            defined_symbol(r.defined_symbols.as_deref()),
            &r.segment_id,
            None,
        )?;
    }
    Ok(())
}

/// Render a slice of symbol matches as one lean row per match.
///
/// The `<kind>` field is the `reference_kind:kind` composite (e.g. `def:function`,
/// `usage:struct`) so an agent can tell definitions from usages without parsing
/// headers.
pub fn render_symbol<W: Write>(sink: &mut W, results: &[SymbolResult]) -> anyhow::Result<()> {
    for r in results {
        let ref_kind_tag = reference_kind_tag(&r.reference_kind);
        let composite = format!("{ref_kind_tag}:{}", r.kind);
        write_discovery_row(
            sink,
            0,
            &r.file_path,
            r.line_start,
            r.line_end,
            &composite,
            r.breadcrumb.as_deref(),
            Some(r.name.as_str()),
            &r.segment_id,
            None,
        )?;
    }
    Ok(())
}

/// Render an impact envelope. Primary rows are written before contextual rows
/// and carry ` ~P` / ` ~C` channel suffixes respectively. `refused`, `empty`,
/// and `empty_scoped` envelopes emit a single terminal line (no row grammar).
pub fn render_impact<W: Write>(
    sink: &mut W,
    envelope: &ImpactResultEnvelope,
) -> anyhow::Result<()> {
    match envelope.status {
        ImpactStatus::Refused => {
            let (reason, message) = envelope
                .refusal
                .as_ref()
                .map(|r| (r.reason.as_str(), r.message.as_str()))
                .unwrap_or(("refused", ""));
            writeln!(sink, "refused{FIELD_SEP}{reason}{FIELD_SEP}{message}")?;
            write_hint_line(
                sink,
                envelope.hint.as_ref(),
                envelope.resolved_anchor.as_ref(),
            )?;
            return Ok(());
        }
        ImpactStatus::Empty | ImpactStatus::EmptyScoped => {
            writeln!(sink, "{}", impact_status_label(envelope.status))?;
            write_hint_line(
                sink,
                envelope.hint.as_ref(),
                envelope.resolved_anchor.as_ref(),
            )?;
            return Ok(());
        }
        ImpactStatus::Expanded | ImpactStatus::ExpandedScoped => {}
    }

    for candidate in &envelope.results {
        write_impact_row(sink, candidate, ImpactChannel::Primary)?;
    }
    if let Some(contextual) = envelope.contextual_results.as_ref() {
        for candidate in contextual {
            write_impact_row(sink, candidate, ImpactChannel::Contextual)?;
        }
    }
    Ok(())
}

/// Render a context read as an `<path>:<l1>-<l2>  context  <scope_type>` header
/// followed by a blank line and a gutter-prefixed body (`NNNN| <content>`).
pub fn render_context<W: Write>(sink: &mut W, result: &ContextResult) -> anyhow::Result<()> {
    writeln!(
        sink,
        "{}:{}-{}{FIELD_SEP}context{FIELD_SEP}{}",
        result.file_path, result.line_start, result.line_end, result.scope_type,
    )?;
    writeln!(sink)?;
    for (offset, line) in result.content.lines().enumerate() {
        let lineno = result.line_start + offset;
        writeln!(sink, "{:>4}| {}", lineno, line)?;
    }
    Ok(())
}

/// Render structural hits as a per-match header
/// `<path>:<l1>-<l2>  structural  <language>::<pattern_name>` followed by the
/// matched snippet indented two spaces.
pub fn render_structural<W: Write>(
    sink: &mut W,
    results: &[StructuralResult],
) -> anyhow::Result<()> {
    for (idx, r) in results.iter().enumerate() {
        if idx > 0 {
            writeln!(sink)?;
        }
        let pattern = r.pattern_name.as_deref().unwrap_or("match");
        writeln!(
            sink,
            "{}:{}-{}{FIELD_SEP}structural{FIELD_SEP}{}::{}",
            r.file_path, r.line_start, r.line_end, r.language, pattern,
        )?;
        for line in r.content.lines() {
            writeln!(sink, "  {line}")?;
        }
    }
    Ok(())
}

/// Render a resolved `get` outcome as a `segment <id>\n<tab-metadata>\n\n<body>\n\n---`
/// record.
///
/// Unresolved handles are rendered as `not_found\t<handle>\n---\n` on the same
/// sink so that a state-machine parser sees one record per input handle in
/// request order.
pub fn render_get_found<W: Write>(sink: &mut W, segment: &StoredSegment) -> anyhow::Result<()> {
    writeln!(sink, "segment {}", segment.id)?;
    let defines = segment.parsed_defined_symbols().join(",");
    let references = segment.parsed_referenced_symbols().join(",");
    let calls = segment.parsed_called_symbols().join(",");
    let breadcrumb = segment.breadcrumb.as_deref().unwrap_or(PLACEHOLDER);
    writeln!(
        sink,
        "path\t{}\tlines\t{}-{}\tkind\t{}\tlanguage\t{}\tbreadcrumb\t{}\trole\t{}\tcomplexity\t{}\tdefines\t{}\treferences\t{}\tcalls\t{}",
        segment.file_path,
        segment.line_start,
        segment.line_end,
        segment.block_type,
        segment.language,
        breadcrumb,
        segment.role.to_ascii_lowercase(),
        segment.complexity,
        defines,
        references,
        calls,
    )?;
    writeln!(sink)?;
    writeln!(sink, "{}", segment.content)?;
    writeln!(sink)?;
    writeln!(sink, "---")?;
    Ok(())
}

/// Render a `not_found` record for a handle that did not resolve to a segment.
///
/// `raw_handle` is echoed verbatim (including any leading `:`) so an agent can
/// correlate the failure with its original input.
pub fn render_get_not_found<W: Write>(sink: &mut W, raw_handle: &str) -> anyhow::Result<()> {
    writeln!(sink, "not_found\t{raw_handle}")?;
    writeln!(sink, "---")?;
    Ok(())
}

/// Format a full segment id as the lean `:<12-char-id>` trailing token.
///
/// Exposed so call sites that render suggestions (`impact` hints, stderr
/// disambiguation) can stay consistent with the row grammar without duplicating
/// the truncation rule.
pub fn segment_suffix(segment_id: &str) -> String {
    let short: String = segment_id.chars().take(SEGMENT_HANDLE_LEN).collect();
    format!(":{short}")
}

/// Which impact bucket a row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImpactChannel {
    Primary,
    Contextual,
}

impl ImpactChannel {
    fn suffix(self) -> &'static str {
        match self {
            ImpactChannel::Primary => "~P",
            ImpactChannel::Contextual => "~C",
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_discovery_row<W: Write>(
    sink: &mut W,
    score: u32,
    path: &str,
    l1: usize,
    l2: usize,
    kind: &str,
    breadcrumb: Option<&str>,
    symbol: Option<&str>,
    segment_id: &str,
    channel: Option<ImpactChannel>,
) -> anyhow::Result<()> {
    let breadcrumb_symbol = render_breadcrumb_symbol(breadcrumb, symbol);
    let suffix = segment_suffix(segment_id);
    write!(
        sink,
        "{score}{FIELD_SEP}{path}:{l1}-{l2}{FIELD_SEP}{kind}{FIELD_SEP}{breadcrumb_symbol}{FIELD_SEP}{suffix}",
    )?;
    if let Some(channel) = channel {
        write!(sink, "{FIELD_SEP}{}", channel.suffix())?;
    }
    writeln!(sink)?;
    Ok(())
}

fn write_impact_row<W: Write>(
    sink: &mut W,
    candidate: &ImpactCandidate,
    channel: ImpactChannel,
) -> anyhow::Result<()> {
    let score = ((candidate.score * 100.0).round().clamp(0.0, 100.0)) as u32;
    write_discovery_row(
        sink,
        score,
        &candidate.file_path,
        candidate.line_start,
        candidate.line_end,
        &candidate.block_type,
        candidate.breadcrumb.as_deref(),
        defined_symbol(candidate.defined_symbols.as_deref()),
        &candidate.segment_id,
        Some(channel),
    )
}

fn write_hint_line<W: Write>(
    sink: &mut W,
    hint: Option<&ImpactHint>,
    anchor: Option<&ResolvedImpactAnchor>,
) -> anyhow::Result<()> {
    let Some(hint) = hint else {
        return Ok(());
    };
    let mut line = format!("hint{FIELD_SEP}{}{FIELD_SEP}{}", hint.code, hint.message);
    if let Some(scope) = &hint.suggested_scope {
        line.push_str(FIELD_SEP);
        line.push_str("scope=");
        line.push_str(scope);
    } else if let Some(scope) = anchor.and_then(|a| a.scope.as_deref()) {
        line.push_str(FIELD_SEP);
        line.push_str("scope=");
        line.push_str(scope);
    }
    if let Some(segment_id) = &hint.suggested_segment_id {
        line.push_str(FIELD_SEP);
        line.push_str(&segment_suffix(segment_id));
    }
    writeln!(sink, "{line}")?;
    Ok(())
}

fn render_breadcrumb_symbol(breadcrumb: Option<&str>, symbol: Option<&str>) -> String {
    let breadcrumb = breadcrumb.filter(|s| !s.is_empty()).unwrap_or(PLACEHOLDER);
    let symbol = symbol.filter(|s| !s.is_empty()).unwrap_or(PLACEHOLDER);
    format!("{breadcrumb}::{symbol}")
}

fn defined_symbol(defined: Option<&[String]>) -> Option<&str> {
    defined.and_then(|syms| syms.first().map(String::as_str))
}

fn reference_kind_tag(kind: &crate::shared::types::ReferenceKind) -> &'static str {
    match kind {
        crate::shared::types::ReferenceKind::Definition => "def",
        crate::shared::types::ReferenceKind::Usage => "usage",
    }
}

fn impact_status_label(status: ImpactStatus) -> &'static str {
    match status {
        ImpactStatus::Expanded => "expanded",
        ImpactStatus::ExpandedScoped => "expanded_scoped",
        ImpactStatus::Empty => "empty",
        ImpactStatus::EmptyScoped => "empty_scoped",
        ImpactStatus::Refused => "refused",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::impact::{ImpactCandidate, ImpactRefusal};
    use crate::shared::types::{ReferenceKind, SegmentRole};

    fn sample_search_result() -> SearchResult {
        SearchResult {
            segment_id: "abcdef0123456789".to_string(),
            file_path: "src/auth/builder.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: String::new(),
            score: 95,
            line_number: 21,
            line_end: 38,
            breadcrumb: Some("AuthConfig::build".to_string()),
            defined_symbols: Some(vec!["build_auth".to_string()]),
        }
    }

    fn sample_impact_candidate(segment_id: &str, score: f64) -> ImpactCandidate {
        ImpactCandidate {
            segment_id: segment_id.to_string(),
            file_path: "src/auth/builder.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            line_start: 21,
            line_end: 38,
            score,
            hop: 1,
            reasons: Vec::new(),
            breadcrumb: Some("AuthConfig::build".to_string()),
            complexity: Some(4),
            role: Some(SegmentRole::Orchestration),
            defined_symbols: Some(vec!["build_auth".to_string()]),
        }
    }

    fn capture<F: FnOnce(&mut Vec<u8>) -> anyhow::Result<()>>(render: F) -> String {
        let mut buf = Vec::new();
        render(&mut buf).expect("render succeeds");
        String::from_utf8(buf).expect("utf-8 output")
    }

    #[test]
    fn search_row_matches_grammar() {
        let out = capture(|sink| render_search(sink, &[sample_search_result()]));
        let expected =
            "95  src/auth/builder.rs:21-38  function  AuthConfig::build::build_auth  :abcdef012345\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn search_row_uses_placeholders_when_breadcrumb_and_symbol_missing() {
        let mut r = sample_search_result();
        r.breadcrumb = None;
        r.defined_symbols = None;
        let out = capture(|sink| render_search(sink, &[r]));
        assert!(
            out.contains("  -::-  :"),
            "expected placeholder breadcrumb+symbol, got: {out}"
        );
    }

    #[test]
    fn segment_suffix_truncates_to_twelve_hex() {
        assert_eq!(segment_suffix("abcdef0123456789fedcba"), ":abcdef012345");
        assert_eq!(segment_suffix("short"), ":short");
    }

    #[test]
    fn symbol_row_uses_reference_kind_prefix() {
        let sym = SymbolResult {
            segment_id: "abcdef0123456789".to_string(),
            name: "Config".to_string(),
            kind: "struct".to_string(),
            file_path: "src/config.rs".to_string(),
            language: "rust".to_string(),
            line_start: 10,
            line_end: 20,
            content: String::new(),
            reference_kind: ReferenceKind::Definition,
            breadcrumb: Some("mod config".to_string()),
        };
        let out = capture(|sink| render_symbol(sink, &[sym]));
        assert!(out.contains("  def:struct  "), "got: {out}");
        assert!(out.contains("mod config::Config"), "got: {out}");
        assert!(
            out.contains(":abcdef012345"),
            "row must carry real 12-char segment handle, got: {out}"
        );
    }

    #[test]
    fn impact_primary_precedes_contextual() {
        let envelope = ImpactResultEnvelope {
            status: ImpactStatus::Expanded,
            resolved_anchor: None,
            results: vec![sample_impact_candidate("primary000000001", 0.9)],
            contextual_results: Some(vec![sample_impact_candidate("context000000001", 0.3)]),
            hint: None,
            refusal: None,
        };
        let out = capture(|sink| render_impact(sink, &envelope));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 rows, got: {out}");
        assert!(lines[0].ends_with("  ~P"), "primary first: {}", lines[0]);
        assert!(
            lines[1].ends_with("  ~C"),
            "contextual second: {}",
            lines[1]
        );
        let primary_idx = out.find("~P").expect("has ~P");
        let contextual_idx = out.find("~C").expect("has ~C");
        assert!(primary_idx < contextual_idx);
    }

    #[test]
    fn impact_refusal_emits_terminal_line() {
        let envelope = ImpactResultEnvelope {
            status: ImpactStatus::Refused,
            resolved_anchor: None,
            results: Vec::new(),
            contextual_results: None,
            hint: None,
            refusal: Some(ImpactRefusal {
                reason: "symbol_too_broad".to_string(),
                message: "Symbol matched too many definitions.".to_string(),
            }),
        };
        let out = capture(|sink| render_impact(sink, &envelope));
        assert_eq!(
            out,
            "refused  symbol_too_broad  Symbol matched too many definitions.\n"
        );
    }

    #[test]
    fn impact_empty_scoped_emits_status_label() {
        let envelope = ImpactResultEnvelope {
            status: ImpactStatus::EmptyScoped,
            resolved_anchor: None,
            results: Vec::new(),
            contextual_results: None,
            hint: None,
            refusal: None,
        };
        let out = capture(|sink| render_impact(sink, &envelope));
        assert_eq!(out, "empty_scoped\n");
    }

    #[test]
    fn context_row_uses_gutter_prefixed_body() {
        let ctx = ContextResult {
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            content: "fn a() {}\nfn b() {}".to_string(),
            line_start: 12,
            line_end: 13,
            scope_type: "function".to_string(),
            access_scope: None,
        };
        let out = capture(|sink| render_context(sink, &ctx));
        let expected = "src/lib.rs:12-13  context  function\n\n  12| fn a() {}\n  13| fn b() {}\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn structural_row_emits_indented_snippet() {
        let r = StructuralResult {
            file_path: "src/parse.rs".to_string(),
            language: "rust".to_string(),
            pattern_name: Some("fn_decl".to_string()),
            content: "fn parse() {\n    todo!()\n}".to_string(),
            line_start: 30,
            line_end: 32,
        };
        let out = capture(|sink| render_structural(sink, &[r]));
        let expected =
            "src/parse.rs:30-32  structural  rust::fn_decl\n  fn parse() {\n      todo!()\n  }\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn get_not_found_emits_sentinel() {
        let out = capture(|sink| render_get_not_found(sink, ":deadbeefcafe"));
        assert_eq!(out, "not_found\t:deadbeefcafe\n---\n");
    }

    fn sample_stored_segment() -> StoredSegment {
        StoredSegment {
            id: "abcdef0123456789".to_string(),
            file_path: "src/auth/builder.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "fn build() {}".to_string(),
            line_start: 21,
            line_end: 38,
            breadcrumb: Some("AuthConfig::build".to_string()),
            complexity: 4,
            role: "ORCHESTRATION".to_string(),
            defined_symbols: "[\"build_auth\"]".to_string(),
            referenced_symbols: "[]".to_string(),
            called_symbols: "[]".to_string(),
            file_hash: "h".to_string(),
            created_at: "t".to_string(),
            updated_at: "t".to_string(),
        }
    }

    #[test]
    fn lean_row_no_segment_prefix_literal() {
        // Capture every lean renderer's output in a single buffer and assert the
        // literal `segment=` substring never appears. This is the T9 grep-style
        // guard that pins the grammar's suffix convention (`:<id>`) across
        // search, symbol, impact (both buckets, empty, refused), context,
        // structural, and get rendering paths.
        let search_out = capture(|sink| render_search(sink, &[sample_search_result()]));

        let sym = SymbolResult {
            segment_id: "abcdef0123456789".to_string(),
            name: "Config".to_string(),
            kind: "struct".to_string(),
            file_path: "src/config.rs".to_string(),
            language: "rust".to_string(),
            line_start: 10,
            line_end: 20,
            content: String::new(),
            reference_kind: ReferenceKind::Definition,
            breadcrumb: Some("mod config".to_string()),
        };
        let symbol_out = capture(|sink| render_symbol(sink, &[sym]));

        let expanded_envelope = ImpactResultEnvelope {
            status: ImpactStatus::Expanded,
            resolved_anchor: None,
            results: vec![sample_impact_candidate("primary000000001", 0.9)],
            contextual_results: Some(vec![sample_impact_candidate("context000000001", 0.3)]),
            hint: None,
            refusal: None,
        };
        let impact_expanded_out = capture(|sink| render_impact(sink, &expanded_envelope));

        let refused_envelope = ImpactResultEnvelope {
            status: ImpactStatus::Refused,
            resolved_anchor: None,
            results: Vec::new(),
            contextual_results: None,
            hint: None,
            refusal: Some(ImpactRefusal {
                reason: "symbol_too_broad".to_string(),
                message: "m".to_string(),
            }),
        };
        let impact_refused_out = capture(|sink| render_impact(sink, &refused_envelope));

        let ctx = ContextResult {
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            content: "fn a() {}".to_string(),
            line_start: 12,
            line_end: 12,
            scope_type: "function".to_string(),
            access_scope: None,
        };
        let context_out = capture(|sink| render_context(sink, &ctx));

        let struc = StructuralResult {
            file_path: "src/parse.rs".to_string(),
            language: "rust".to_string(),
            pattern_name: Some("fn_decl".to_string()),
            content: "fn parse() {}".to_string(),
            line_start: 30,
            line_end: 30,
        };
        let structural_out = capture(|sink| render_structural(sink, &[struc]));

        let get_found_out = capture(|sink| render_get_found(sink, &sample_stored_segment()));
        let get_not_found_out = capture(|sink| render_get_not_found(sink, ":deadbeefcafe"));

        let combined = [
            search_out,
            symbol_out,
            impact_expanded_out,
            impact_refused_out,
            context_out,
            structural_out,
            get_found_out,
            get_not_found_out,
        ]
        .concat();

        assert!(
            !combined.contains("segment="),
            "lean output must not contain `segment=`: {combined}"
        );
    }

    #[test]
    fn get_handle_tolerates_leading_colon() {
        // Mirrors the lean grammar: agents paste `:<id>` tokens straight out of
        // search rows into `1up get`. This pin keeps the suffix helper and the
        // get-side normalization on the same shape.
        assert_eq!(segment_suffix("abcdef0123456789"), ":abcdef012345");
        // segment_suffix produces the exact token `get` must accept back.
        let suffix = segment_suffix("abcdef0123456789");
        assert!(suffix.starts_with(':'));
        assert_eq!(&suffix[1..], "abcdef012345");
    }
}
