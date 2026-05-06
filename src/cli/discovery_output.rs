use std::io::Write;

use crate::cli::lean;
use crate::search::impact::{
    ImpactCandidate, ImpactHint, ImpactReason, ImpactRefusal, ImpactResultEnvelope, ImpactStatus,
    ResolvedImpactAnchor,
};
use crate::shared::types::{ContextResult, ReferenceKind, SymbolResult};
use crate::storage::segments::StoredSegment;

pub fn render_get_found<W: Write>(sink: &mut W, segment: &StoredSegment) -> anyhow::Result<()> {
    writeln!(sink, "Segment {}", lean::segment_suffix(&segment.id))?;
    writeln!(sink, "ID: {}", segment.id)?;
    writeln!(sink, "Path: {}", segment.file_path)?;
    writeln!(sink, "Lines: {}-{}", segment.line_start, segment.line_end)?;
    writeln!(sink, "Kind: {}", segment.block_type)?;
    writeln!(sink, "Language: {}", segment.language)?;
    if let Some(breadcrumb) = segment
        .breadcrumb
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        writeln!(sink, "Breadcrumb: {breadcrumb}")?;
    }
    writeln!(sink, "Role: {}", humanize_wire_value(&segment.role))?;
    writeln!(sink, "Complexity: {}", segment.complexity)?;
    render_symbol_list(sink, "Defines", &segment.parsed_defined_symbols())?;
    render_symbol_list(sink, "References", &segment.parsed_referenced_symbols())?;
    render_symbol_list(sink, "Calls", &segment.parsed_called_symbols())?;
    writeln!(sink)?;
    writeln!(sink, "{}", segment.content)?;
    writeln!(sink)?;
    writeln!(sink, "---")?;
    Ok(())
}

pub fn render_get_not_found<W: Write>(sink: &mut W, raw_handle: &str) -> anyhow::Result<()> {
    writeln!(sink, "No segment found for `{raw_handle}`.")?;
    writeln!(sink, "---")?;
    Ok(())
}

pub fn render_symbol<W: Write>(
    sink: &mut W,
    query: &str,
    include_references: bool,
    fuzzy: bool,
    results: &[SymbolResult],
) -> anyhow::Result<()> {
    writeln!(sink, "Symbols for `{query}`")?;
    let mode = if include_references {
        "definitions and references"
    } else {
        "definitions"
    };
    writeln!(sink, "Mode: {mode}")?;
    writeln!(sink, "Matching: {}", if fuzzy { "fuzzy" } else { "exact" })?;
    writeln!(sink, "Matches: {}", results.len())?;

    if results.is_empty() {
        writeln!(sink)?;
        writeln!(sink, "No symbol matches found.")?;
        return Ok(());
    }

    for (index, result) in results.iter().enumerate() {
        writeln!(sink)?;
        writeln!(
            sink,
            "{}. {} {} `{}`",
            index + 1,
            reference_label(result.reference_kind),
            result.kind,
            result.name
        )?;
        writeln!(
            sink,
            "   Location: {}:{}-{}",
            result.file_path, result.line_start, result.line_end
        )?;
        writeln!(sink, "   Language: {}", result.language)?;
        if let Some(breadcrumb) = result
            .breadcrumb
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            writeln!(sink, "   Breadcrumb: {breadcrumb}")?;
        }
        writeln!(
            sink,
            "   Handle: {}",
            lean::segment_suffix(&result.segment_id)
        )?;
    }

    Ok(())
}

pub fn render_context<W: Write>(sink: &mut W, result: &ContextResult) -> anyhow::Result<()> {
    writeln!(
        sink,
        "Context {}:{}-{}",
        result.file_path, result.line_start, result.line_end
    )?;
    writeln!(sink, "Scope: {}", result.scope_type)?;
    writeln!(sink, "Language: {}", result.language)?;
    if let Some(access_scope) = result.access_scope {
        writeln!(sink, "Access: {access_scope:?}")?;
    }
    writeln!(sink)?;
    for (offset, line) in result.content.lines().enumerate() {
        let lineno = result.line_start + offset;
        writeln!(sink, "{lineno:>4} | {line}")?;
    }
    Ok(())
}

pub fn render_impact<W: Write>(
    sink: &mut W,
    envelope: &ImpactResultEnvelope,
) -> anyhow::Result<()> {
    writeln!(sink, "Likely impact (advisory)")?;
    writeln!(sink, "Status: {}", impact_status_label(envelope.status))?;

    if let Some(anchor) = envelope.resolved_anchor.as_ref() {
        render_anchor(sink, anchor)?;
    }

    if let Some(refusal) = envelope.refusal.as_ref() {
        render_refusal(sink, refusal)?;
    }

    if let Some(hint) = envelope.hint.as_ref() {
        render_hint(sink, hint)?;
    }

    if envelope.results.is_empty() {
        writeln!(sink)?;
        writeln!(sink, "Primary results: none")?;
    } else {
        writeln!(sink)?;
        writeln!(sink, "Primary results: {}", envelope.results.len())?;
        render_impact_candidates(sink, &envelope.results)?;
    }

    if let Some(contextual) = envelope.contextual_results.as_deref() {
        if !contextual.is_empty() {
            writeln!(sink)?;
            writeln!(sink, "Contextual results: {}", contextual.len())?;
            render_impact_candidates(sink, contextual)?;
        }
    }

    Ok(())
}

fn render_symbol_list<W: Write>(
    sink: &mut W,
    label: &str,
    values: &[String],
) -> anyhow::Result<()> {
    if values.is_empty() {
        writeln!(sink, "{label}: none")?;
    } else {
        writeln!(sink, "{label}: {}", values.join(", "))?;
    }
    Ok(())
}

fn render_anchor<W: Write>(sink: &mut W, anchor: &ResolvedImpactAnchor) -> anyhow::Result<()> {
    writeln!(sink, "Anchor: {} `{}`", anchor.kind, anchor.value)?;
    if let Some(line) = anchor.line {
        writeln!(sink, "Anchor line: {line}")?;
    }
    if let Some(scope) = anchor.scope.as_deref() {
        writeln!(sink, "Scope: {scope}")?;
    }
    if !anchor.matched_files.is_empty() {
        writeln!(sink, "Matched files: {}", anchor.matched_files.join(", "))?;
    }
    Ok(())
}

fn render_refusal<W: Write>(sink: &mut W, refusal: &ImpactRefusal) -> anyhow::Result<()> {
    writeln!(sink, "Refusal: {}", refusal.reason)?;
    writeln!(sink, "Message: {}", refusal.message)?;
    Ok(())
}

fn render_hint<W: Write>(sink: &mut W, hint: &ImpactHint) -> anyhow::Result<()> {
    writeln!(sink, "Hint: {} - {}", hint.code, hint.message)?;
    if let Some(scope) = hint.suggested_scope.as_deref() {
        writeln!(sink, "Suggested scope: {scope}")?;
    }
    if let Some(segment_id) = hint.suggested_segment_id.as_deref() {
        writeln!(
            sink,
            "Suggested handle: {}",
            lean::segment_suffix(segment_id)
        )?;
    }
    Ok(())
}

fn render_impact_candidates<W: Write>(
    sink: &mut W,
    candidates: &[ImpactCandidate],
) -> anyhow::Result<()> {
    for (index, candidate) in candidates.iter().enumerate() {
        let score = ((candidate.score * 100.0).round().clamp(0.0, 100.0)) as u32;
        writeln!(
            sink,
            "{}. {}:{}-{} {} (score {score})",
            index + 1,
            candidate.file_path,
            candidate.line_start,
            candidate.line_end,
            candidate.block_type
        )?;
        if let Some(breadcrumb) = candidate
            .breadcrumb
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            writeln!(sink, "   Breadcrumb: {breadcrumb}")?;
        }
        if let Some(symbols) = candidate
            .defined_symbols
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            writeln!(sink, "   Defines: {}", symbols.join(", "))?;
        }
        if let Some(role) = candidate.role {
            writeln!(sink, "   Role: {role:?}")?;
        }
        if let Some(complexity) = candidate.complexity {
            writeln!(sink, "   Complexity: {complexity}")?;
        }
        writeln!(sink, "   Hop: {}", candidate.hop)?;
        writeln!(
            sink,
            "   Handle: {}",
            lean::segment_suffix(&candidate.segment_id)
        )?;
        let reasons = format_reasons(&candidate.reasons);
        if !reasons.is_empty() {
            writeln!(sink, "   Reasons: {reasons}")?;
        }
    }
    Ok(())
}

fn format_reasons(reasons: &[ImpactReason]) -> String {
    reasons
        .iter()
        .map(|reason| {
            let mut parts = vec![reason.kind.clone()];
            if let Some(symbol) = reason.symbol.as_deref() {
                parts.push(format!("symbol={symbol}"));
            }
            if let Some(segment_id) = reason.from_segment_id.as_deref() {
                parts.push(format!("from={}", lean::segment_suffix(segment_id)));
            }
            parts.join(" ")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn reference_label(kind: ReferenceKind) -> &'static str {
    match kind {
        ReferenceKind::Definition => "definition",
        ReferenceKind::Usage => "usage",
    }
}

fn impact_status_label(status: ImpactStatus) -> &'static str {
    match status {
        ImpactStatus::Expanded => "expanded",
        ImpactStatus::ExpandedScoped => "expanded scoped",
        ImpactStatus::Empty => "empty",
        ImpactStatus::EmptyScoped => "empty scoped",
        ImpactStatus::Refused => "refused",
    }
}

fn humanize_wire_value(value: &str) -> String {
    value.replace('_', " ").to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::SegmentRole;

    fn capture<F: FnOnce(&mut Vec<u8>) -> anyhow::Result<()>>(render: F) -> String {
        let mut buf = Vec::new();
        render(&mut buf).expect("render succeeds");
        String::from_utf8(buf).expect("utf-8 output")
    }

    #[test]
    fn impact_output_labels_advisory_status() {
        let envelope = ImpactResultEnvelope {
            status: ImpactStatus::Expanded,
            resolved_anchor: None,
            results: vec![ImpactCandidate {
                segment_id: "abcdef0123456789".to_string(),
                file_path: "src/lib.rs".to_string(),
                language: "rust".to_string(),
                block_type: "function".to_string(),
                line_start: 10,
                line_end: 12,
                score: 0.9,
                hop: 1,
                reasons: Vec::new(),
                breadcrumb: None,
                complexity: None,
                role: Some(SegmentRole::Definition),
                defined_symbols: Some(vec!["load_config".to_string()]),
            }],
            contextual_results: None,
            hint: None,
            refusal: None,
        };

        let out = capture(|sink| render_impact(sink, &envelope));
        assert!(out.contains("Likely impact (advisory)"));
        assert!(out.contains("Primary results: 1"));
        assert!(out.contains("Handle: :abcdef012345"));
    }

    #[test]
    fn symbol_output_reports_empty_matches() {
        let out = capture(|sink| render_symbol(sink, "Missing", false, false, &[]));
        assert!(out.contains("Symbols for `Missing`"));
        assert!(out.contains("Matches: 0"));
        assert!(out.contains("No symbol matches found."));
    }
}
