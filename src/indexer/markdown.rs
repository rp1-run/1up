use tree_sitter::{Language, Node, Parser};

use crate::indexer::chunker::{chunk_file, chunk_file_default};
use crate::shared::constants::{CHUNK_OVERLAP, CHUNK_WINDOW_SIZE};
use crate::shared::types::{ParsedSegment, SegmentRole};

/// Block type for heading-scoped markdown documentation segments.
#[allow(dead_code)]
pub const DOC_SECTION_BLOCK_TYPE: &str = "doc_section";

/// Parses a markdown file into heading-scoped documentation segments.
///
/// Each heading section becomes one `ParsedSegment` with a document-rooted
/// breadcrumb (`{file_stem} > {heading path...}`), `block_type: "doc_section"`,
/// and `role: Docs`. Oversized sections split through the existing chunk
/// mechanism with the section breadcrumb preserved on every piece. If the
/// tree-sitter parse fails, falls back to plain doc chunks so coverage is
/// never lost.
#[allow(dead_code)]
pub fn parse_markdown_file(content: &str, file_stem: &str) -> Vec<ParsedSegment> {
    if content.is_empty() {
        return Vec::new();
    }
    match structural_doc_segments(content, file_stem) {
        Some(segments) => segments,
        None => fallback_doc_chunks(content),
    }
}

fn structural_doc_segments(content: &str, file_stem: &str) -> Option<Vec<ParsedSegment>> {
    let language = Language::new(tree_sitter_md::LANGUAGE);
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(content, None)?;

    let document = tree.root_node();
    let lines: Vec<&str> = content.lines().collect();

    let mut preamble_span: Option<(usize, usize)> = None;
    let mut heading_sections = Vec::new();
    let child_count = document.named_child_count();
    for i in 0..child_count {
        let child = document.named_child(i as u32).unwrap();
        match child.kind() {
            "minus_metadata" | "plus_metadata" => {
                merge_span(&mut preamble_span, node_line_range(&child));
            }
            "section" => {
                if section_heading_node(&child).is_some() {
                    heading_sections.push(child);
                } else {
                    merge_span(&mut preamble_span, node_line_range(&child));
                }
            }
            _ => {}
        }
    }

    let mut segments = Vec::new();
    let mut path = vec![file_stem.to_string()];
    if let Some((start, end)) = preamble_span {
        emit_span(start, end, &breadcrumb_of(&path), &lines, &mut segments);
    }
    for section in heading_sections {
        walk_section(
            section,
            &lines,
            content.as_bytes(),
            &mut path,
            &mut segments,
        );
    }
    Some(segments)
}

fn walk_section(
    section: Node,
    lines: &[&str],
    source: &[u8],
    path: &mut Vec<String>,
    out: &mut Vec<ParsedSegment>,
) {
    let pushed = match heading_text(&section, source) {
        Some(text) => {
            path.push(text);
            true
        }
        None => false,
    };

    let child_sections = child_sections(&section);
    if let Some((start, end)) = node_line_range(&section) {
        let own_end = child_sections
            .first()
            .map(|child| child.start_position().row)
            .unwrap_or(end)
            .min(end);
        if own_end >= start {
            emit_span(start, own_end, &breadcrumb_of(path), lines, out);
        }
    }
    for child in child_sections {
        walk_section(child, lines, source, path, out);
    }

    if pushed {
        path.pop();
    }
}

/// Emits one doc segment for the line span, splitting oversized spans through
/// the existing chunk mechanism with the breadcrumb preserved on every piece.
fn emit_span(
    line_start: usize,
    line_end: usize,
    breadcrumb: &Option<String>,
    lines: &[&str],
    out: &mut Vec<ParsedSegment>,
) {
    let line_end = line_end.min(lines.len());
    if line_start > line_end {
        return;
    }
    let span_content = lines[line_start - 1..line_end].join("\n");
    let line_count = line_end - line_start + 1;

    if line_count > CHUNK_WINDOW_SIZE {
        for piece in chunk_file(&span_content, "md", CHUNK_WINDOW_SIZE, CHUNK_OVERLAP) {
            out.push(doc_segment(
                piece.content,
                piece.line_start + line_start - 1,
                piece.line_end + line_start - 1,
                breadcrumb.clone(),
            ));
        }
    } else {
        out.push(doc_segment(
            span_content,
            line_start,
            line_end,
            breadcrumb.clone(),
        ));
    }
}

fn doc_segment(
    content: String,
    line_start: usize,
    line_end: usize,
    breadcrumb: Option<String>,
) -> ParsedSegment {
    ParsedSegment {
        content,
        block_type: DOC_SECTION_BLOCK_TYPE.into(),
        line_start,
        line_end,
        language: "markdown".into(),
        breadcrumb,
        complexity: 0,
        role: SegmentRole::Docs,
        defined_symbols: Vec::new(),
        referenced_symbols: Vec::new(),
        referenced_relations: Vec::new(),
        called_symbols: Vec::new(),
        called_relations: Vec::new(),
    }
}

fn section_heading_node<'a>(section: &Node<'a>) -> Option<Node<'a>> {
    let first = section.named_child(0)?;
    matches!(first.kind(), "atx_heading" | "setext_heading").then_some(first)
}

fn heading_text(section: &Node, source: &[u8]) -> Option<String> {
    let heading = section_heading_node(section)?;
    let content_node = heading.child_by_field_name("heading_content")?;
    let text = content_node.utf8_text(source).ok()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn child_sections<'a>(section: &Node<'a>) -> Vec<Node<'a>> {
    let mut children = Vec::new();
    let child_count = section.named_child_count();
    for i in 0..child_count {
        let child = section.named_child(i as u32).unwrap();
        if child.kind() == "section" {
            children.push(child);
        }
    }
    children
}

/// Converts a node span to an inclusive 1-based line range. An end position at
/// column 0 means the node ends at the start of that row, so the last content
/// line is the previous one.
fn node_line_range(node: &Node) -> Option<(usize, usize)> {
    let start = node.start_position().row + 1;
    let end_pos = node.end_position();
    let end = if end_pos.column == 0 {
        end_pos.row
    } else {
        end_pos.row + 1
    };
    (end >= start).then_some((start, end))
}

fn merge_span(target: &mut Option<(usize, usize)>, span: Option<(usize, usize)>) {
    if let Some((start, end)) = span {
        *target = Some(match *target {
            Some((existing_start, existing_end)) => {
                (existing_start.min(start), existing_end.max(end))
            }
            None => (start, end),
        });
    }
}

fn breadcrumb_of(path: &[String]) -> Option<String> {
    let components: Vec<&str> = path
        .iter()
        .map(|component| component.as_str())
        .filter(|component| !component.is_empty())
        .collect();
    (!components.is_empty()).then(|| components.join(" > "))
}

fn fallback_doc_chunks(content: &str) -> Vec<ParsedSegment> {
    let mut segments = chunk_file_default(content, "md");
    for segment in &mut segments {
        segment.role = SegmentRole::Docs;
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::constants::CHUNK_WINDOW_SIZE;
    use crate::shared::types::SegmentRole;

    const NESTED: &str = "# Title\n\nintro text\n\n## Install\n\nsteps here\n\n### macOS\n\nbrew stuff\n\n## Usage\n\nrun it\n";

    fn segment_by_breadcrumb<'a>(
        segments: &'a [ParsedSegment],
        breadcrumb: &str,
    ) -> &'a ParsedSegment {
        segments
            .iter()
            .find(|s| s.breadcrumb.as_deref() == Some(breadcrumb))
            .unwrap_or_else(|| {
                let found: Vec<_> = segments.iter().map(|s| s.breadcrumb.clone()).collect();
                panic!("no segment with breadcrumb {breadcrumb:?}; found {found:?}")
            })
    }

    fn assert_doc_section_shape(segment: &ParsedSegment) {
        assert_eq!(segment.block_type, DOC_SECTION_BLOCK_TYPE);
        assert_eq!(segment.role, SegmentRole::Docs);
        assert_eq!(segment.language, "markdown");
        assert_eq!(segment.complexity, 0);
    }

    #[test]
    fn nested_headings_produce_per_section_segments_with_full_breadcrumb() {
        let segments = parse_markdown_file(NESTED, "README");

        assert_eq!(segments.len(), 4);
        for segment in &segments {
            assert_doc_section_shape(segment);
        }

        let title = segment_by_breadcrumb(&segments, "README > Title");
        assert_eq!((title.line_start, title.line_end), (1, 4));
        assert!(title.content.contains("intro text"));

        let install = segment_by_breadcrumb(&segments, "README > Title > Install");
        assert_eq!((install.line_start, install.line_end), (5, 8));
        assert!(install.content.contains("steps here"));

        let macos = segment_by_breadcrumb(&segments, "README > Title > Install > macOS");
        assert_eq!((macos.line_start, macos.line_end), (9, 12));
        assert!(macos.content.contains("brew stuff"));

        let usage = segment_by_breadcrumb(&segments, "README > Title > Usage");
        assert_eq!((usage.line_start, usage.line_end), (13, 15));
        assert!(usage.content.contains("run it"));
    }

    #[test]
    fn section_span_ends_before_next_same_or_higher_heading() {
        let content =
            "## First\n\nalpha\n\n### Sub\n\ndeep\n\n## Second\n\nbeta\n\n# Top\n\ngamma\n";
        let segments = parse_markdown_file(content, "GUIDE");

        let first = segment_by_breadcrumb(&segments, "GUIDE > First");
        assert_eq!((first.line_start, first.line_end), (1, 4));

        let sub = segment_by_breadcrumb(&segments, "GUIDE > First > Sub");
        assert_eq!((sub.line_start, sub.line_end), (5, 8));
        assert!(!sub.content.contains("beta"));

        let second = segment_by_breadcrumb(&segments, "GUIDE > Second");
        assert_eq!((second.line_start, second.line_end), (9, 12));
        assert!(!second.content.contains("gamma"));

        let top = segment_by_breadcrumb(&segments, "GUIDE > Top");
        assert_eq!((top.line_start, top.line_end), (13, 15));
    }

    #[test]
    fn headingless_file_is_single_file_level_segment_with_stem_breadcrumb() {
        let content = "just some text\n\nmore text\n";
        let segments = parse_markdown_file(content, "NOTES");

        assert_eq!(segments.len(), 1);
        assert_doc_section_shape(&segments[0]);
        assert_eq!(segments[0].breadcrumb.as_deref(), Some("NOTES"));
        assert_eq!((segments[0].line_start, segments[0].line_end), (1, 3));
        assert!(segments[0].content.contains("just some text"));
        assert!(segments[0].content.contains("more text"));
    }

    #[test]
    fn preamble_and_frontmatter_fold_into_file_level_segment() {
        let content = "---\ntitle: x\n---\n\npreamble here\n\n# First\n\nbody\n";
        let segments = parse_markdown_file(content, "README");

        assert_eq!(segments.len(), 2);

        let preamble = segment_by_breadcrumb(&segments, "README");
        assert_doc_section_shape(preamble);
        assert_eq!((preamble.line_start, preamble.line_end), (1, 6));
        assert!(preamble.content.contains("title: x"));
        assert!(preamble.content.contains("preamble here"));

        let first = segment_by_breadcrumb(&segments, "README > First");
        assert_eq!((first.line_start, first.line_end), (7, 9));
        assert!(first.content.contains("body"));
    }

    #[test]
    fn oversized_section_splits_cover_full_span_with_identical_breadcrumb() {
        let mut content = String::from("# Big\n\n");
        for i in 1..=70 {
            content.push_str(&format!("line {i}\n"));
        }
        let segments = parse_markdown_file(&content, "BIG");

        assert!(
            segments.len() > 1,
            "section over {CHUNK_WINDOW_SIZE} lines must split"
        );
        for segment in &segments {
            assert_doc_section_shape(segment);
            assert_eq!(segment.breadcrumb.as_deref(), Some("BIG > Big"));
            assert!(
                segment.line_end - segment.line_start < CHUNK_WINDOW_SIZE,
                "no split piece may span the whole section"
            );
        }

        assert_eq!(segments[0].line_start, 1);
        assert!(segments[0].content.starts_with("# Big"));
        assert_eq!(segments.last().unwrap().line_end, 72);
        for pair in segments.windows(2) {
            assert!(
                pair[1].line_start <= pair[0].line_end + 1,
                "split pieces must cover the section without gaps"
            );
        }
    }

    #[test]
    fn setext_heading_text_extracted_from_paragraph_content() {
        let content = "My Title\n========\n\nbody\n";
        let segments = parse_markdown_file(content, "DOC");

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].breadcrumb.as_deref(), Some("DOC > My Title"));
        assert_eq!((segments[0].line_start, segments[0].line_end), (1, 4));
    }

    #[test]
    fn empty_file_yields_no_segments() {
        assert!(parse_markdown_file("", "EMPTY").is_empty());
    }

    #[test]
    fn parse_failure_falls_back_to_docs_role_chunks() {
        let lines: Vec<String> = (1..=70).map(|i| format!("doc line {i}")).collect();
        let content = lines.join("\n");
        let segments = fallback_doc_chunks(&content);

        assert!(segments.len() > 1);
        assert_eq!(segments[0].line_start, 1);
        assert_eq!(segments.last().unwrap().line_end, 70);
        for segment in &segments {
            assert_eq!(segment.block_type, "chunk");
            assert_eq!(segment.role, SegmentRole::Docs);
            assert_eq!(segment.language, "markdown");
        }
    }
}
