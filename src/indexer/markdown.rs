use tree_sitter::{Language, Node, Parser};

use crate::indexer::chunker::{chunk_file, chunk_file_default};
use crate::shared::constants::{CHUNK_OVERLAP, CHUNK_WINDOW_SIZE};
use crate::shared::symbols::EDGE_IDENTITY_DOC_MENTION;
use crate::shared::types::{ParsedRelation, ParsedSegment, SegmentRole};

/// Block type for heading-scoped markdown documentation segments.
#[allow(dead_code)]
pub const DOC_SECTION_BLOCK_TYPE: &str = "doc_section";

/// Per-segment cap on unique doc-to-code mentions; dedupe happens first, so
/// the cap keeps the first 64 unique tokens in document order.
const MAX_DOC_MENTIONS_PER_SEGMENT: usize = 64;

/// Minimum raw-token length for a mention candidate.
const MIN_MENTION_TOKEN_LEN: usize = 3;

/// Keywords and common words rejected in the strict mention tier. Matched
/// case-insensitively so mixed-case variants (`True`, `None`, `Self`) that
/// pass the beyond-plain-lowercase shape check are still rejected.
const MENTION_STOPLIST: &[&str] = &[
    "let", "fn", "pub", "use", "return", "true", "false", "none", "null", "self", "this", "const",
    "mut", "var", "def", "class", "impl", "struct", "enum", "trait", "mod", "type", "match",
    "else", "for", "while", "loop", "break", "continue", "async", "await", "import", "from",
    "export", "function", "void", "new", "static", "where",
];

/// A code identifier mentioned in documentation, with the 1-based source line
/// it appears on. The raw token is preserved (qualified chains stay whole);
/// downstream `normalize_symbolish` produces the canonical form.
struct Mention {
    token: String,
    line: usize,
}

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

    let mentions = extract_mentions(document, content.as_bytes());
    attach_mentions(&mut segments, mentions);
    Some(segments)
}

/// Collects doc-to-code mentions from inline code spans and fenced code
/// blocks across the whole block tree, in document order.
///
/// Inline code spans come from a second parse of each block-grammar `inline`
/// node region with `tree_sitter_md::INLINE_LANGUAGE`; if that two-phase parse
/// is unavailable the fallback is a lexical backtick scan over the `inline`
/// node text only. Fenced regions are never scanned via the inline path: they
/// are handled as a lexical identifier-token scan over `code_fence_content`
/// text and never parsed with per-language tree-sitter grammars.
fn extract_mentions(document: Node, source: &[u8]) -> Vec<Mention> {
    let mut inline_parser = inline_parser();
    let mut mentions = Vec::new();
    collect_mentions(document, source, &mut inline_parser, &mut mentions);
    mentions
}

fn inline_parser() -> Option<Parser> {
    let language = Language::new(tree_sitter_md::INLINE_LANGUAGE);
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    Some(parser)
}

fn collect_mentions(
    node: Node,
    source: &[u8],
    inline_parser: &mut Option<Parser>,
    out: &mut Vec<Mention>,
) {
    match node.kind() {
        "inline" => {
            let parsed = inline_parser
                .as_mut()
                .and_then(|parser| inline_code_span_mentions(parser, &node, source));
            match parsed {
                Some(mentions) => out.extend(mentions),
                None => {
                    let text = node.utf8_text(source).unwrap_or("");
                    out.extend(lexical_backtick_mentions(
                        text,
                        node.start_position().row + 1,
                    ));
                }
            }
        }
        "code_fence_content" => out.extend(fenced_identifier_mentions(&node, source)),
        _ => {
            let child_count = node.named_child_count();
            for i in 0..child_count {
                let child = node.named_child(i as u32).unwrap();
                collect_mentions(child, source, inline_parser, out);
            }
        }
    }
}

/// Parses one `inline` node region with the inline grammar and collects
/// `code_span` mentions. Included ranges exclude the inline node's named
/// children (block-continuation markers), mirroring the grammar's reference
/// two-phase strategy, so positions stay absolute to the document.
fn inline_code_span_mentions(
    parser: &mut Parser,
    inline_node: &Node,
    source: &[u8],
) -> Option<Vec<Mention>> {
    parser
        .set_included_ranges(&inline_content_ranges(inline_node))
        .ok()?;
    let tree = parser.parse(source, None)?;
    let mut mentions = Vec::new();
    collect_code_span_mentions(tree.root_node(), source, &mut mentions);
    Some(mentions)
}

fn inline_content_ranges(inline_node: &Node) -> Vec<tree_sitter::Range> {
    let mut range = inline_node.range();
    let mut ranges = Vec::new();
    let child_count = inline_node.named_child_count();
    for i in 0..child_count {
        let child_range = inline_node.named_child(i as u32).unwrap().range();
        ranges.push(tree_sitter::Range {
            start_byte: range.start_byte,
            start_point: range.start_point,
            end_byte: child_range.start_byte,
            end_point: child_range.start_point,
        });
        range.start_byte = child_range.end_byte;
        range.start_point = child_range.end_point;
    }
    ranges.push(range);
    ranges
}

fn collect_code_span_mentions(node: Node, source: &[u8], out: &mut Vec<Mention>) {
    if node.kind() == "code_span" {
        if let Ok(text) = node.utf8_text(source) {
            let content = text.trim_matches('`').trim();
            out.extend(span_content_mentions(
                content,
                node.start_position().row + 1,
            ));
        }
        return;
    }
    let child_count = node.named_child_count();
    for i in 0..child_count {
        let child = node.named_child(i as u32).unwrap();
        collect_code_span_mentions(child, source, out);
    }
}

/// Lexical fallback for inline code spans when the two-phase inline parse is
/// unavailable: matches backtick delimiter runs of equal length over the
/// `inline` node text only. Fenced regions are excluded by construction
/// because they are never part of an `inline` node.
fn lexical_backtick_mentions(text: &str, start_line: usize) -> Vec<Mention> {
    let chars: Vec<char> = text.chars().collect();
    let mut mentions = Vec::new();
    let mut line = start_line;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\n' => {
                line += 1;
                i += 1;
            }
            '`' => {
                let run_len = backtick_run_len(&chars, i);
                match find_closing_run(&chars, i + run_len, run_len) {
                    Some(close_start) => {
                        let content: String = chars[i + run_len..close_start].iter().collect();
                        mentions.extend(span_content_mentions(content.trim(), line));
                        line += content.matches('\n').count();
                        i = close_start + run_len;
                    }
                    None => i += run_len,
                }
            }
            _ => i += 1,
        }
    }
    mentions
}

fn backtick_run_len(chars: &[char], start: usize) -> usize {
    chars[start..].iter().take_while(|c| **c == '`').count()
}

fn find_closing_run(chars: &[char], from: usize, run_len: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '`' {
            let len = backtick_run_len(chars, i);
            if len == run_len {
                return Some(i);
            }
            i += len;
        } else {
            i += 1;
        }
    }
    None
}

/// Applies the two-tier identifier-likeness filter to one code span.
///
/// A span whose whole content is a single identifier-shaped token qualifies
/// as-is (the author deliberately marked it as code, so plain lowercase
/// tokens like `reindex` pass). Multi-token span content uses the strict
/// tier, like fenced tokens.
fn span_content_mentions(content: &str, line: usize) -> Vec<Mention> {
    if is_single_identifier_token(content) {
        return vec![Mention {
            token: content.to_string(),
            line,
        }];
    }
    scan_identifier_tokens(content)
        .into_iter()
        .filter(|token| passes_strict_tier(token))
        .map(|token| Mention { token, line })
        .collect()
}

fn fenced_identifier_mentions(node: &Node, source: &[u8]) -> Vec<Mention> {
    let Ok(text) = node.utf8_text(source) else {
        return Vec::new();
    };
    let start_line = node.start_position().row + 1;
    let mut mentions = Vec::new();
    for (offset, line_text) in text.lines().enumerate() {
        for token in scan_identifier_tokens(line_text) {
            if passes_strict_tier(&token) {
                mentions.push(Mention {
                    token,
                    line: start_line + offset,
                });
            }
        }
    }
    mentions
}

/// Strict tier: the token must show identifier shape beyond a plain lowercase
/// word (underscore, mixed case, `::`/`.`/`#` qualification, or a call/macro
/// suffix) and must not be a stoplisted keyword/common word.
fn passes_strict_tier(token: &str) -> bool {
    beyond_plain_lowercase(token) && !MENTION_STOPLIST.contains(&token.to_lowercase().as_str())
}

fn beyond_plain_lowercase(token: &str) -> bool {
    token.contains(['_', '.', '#'])
        || token.contains("::")
        || token.ends_with("()")
        || token.ends_with('!')
        || token.chars().any(|c| c.is_uppercase())
}

fn is_single_identifier_token(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if !is_token_start(chars.first()) {
        return false;
    }
    let (_, end) = consume_identifier_token(&chars, 0);
    end == chars.len() && chars.len() >= MIN_MENTION_TOKEN_LEN
}

/// Scans text for maximal identifier-shaped tokens: letter/underscore start,
/// word characters plus `::`/`.`/`#` qualifiers, an optional trailing `()` or
/// `!`, minimum length 3. Digit-led word runs cannot start a token.
fn scan_identifier_tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_alphabetic() || c == '_' {
            let (token, next) = consume_identifier_token(&chars, i);
            if next - i >= MIN_MENTION_TOKEN_LEN {
                tokens.push(token);
            }
            i = next;
        } else if c.is_alphanumeric() {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    tokens
}

fn consume_identifier_token(chars: &[char], start: usize) -> (String, usize) {
    let mut i = start;
    loop {
        while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        if i < chars.len()
            && chars[i] == ':'
            && chars.get(i + 1) == Some(&':')
            && is_token_start(chars.get(i + 2))
        {
            i += 2;
        } else if i < chars.len()
            && (chars[i] == '.' || chars[i] == '#')
            && is_token_start(chars.get(i + 1))
        {
            i += 1;
        } else {
            break;
        }
    }
    if i < chars.len() && chars[i] == '!' && chars.get(i + 1) != Some(&'=') {
        i += 1;
    } else if chars.get(i) == Some(&'(') && chars.get(i + 1) == Some(&')') {
        i += 2;
    }
    (chars[start..i].iter().collect(), i)
}

fn is_token_start(c: Option<&char>) -> bool {
    c.is_some_and(|c| c.is_alphabetic() || *c == '_')
}

/// Attaches each mention to the first final segment covering its line, then
/// populates `referenced_symbols` (first 64 unique raw tokens in document
/// order) and matching `doc_mention` relation rows on every segment.
///
/// Relations are built from the same token list as the symbols, so
/// `referenced_relations` is non-empty whenever `referenced_symbols` is:
/// storage otherwise synthesizes `bare_identifier` fallback relations that
/// would be indistinguishable from code references.
fn attach_mentions(segments: &mut [ParsedSegment], mentions: Vec<Mention>) {
    for mention in mentions {
        if let Some(segment) = segments
            .iter_mut()
            .find(|s| s.line_start <= mention.line && mention.line <= s.line_end)
        {
            if segment.referenced_symbols.len() < MAX_DOC_MENTIONS_PER_SEGMENT
                && !segment.referenced_symbols.contains(&mention.token)
            {
                segment.referenced_symbols.push(mention.token);
            }
        }
    }
    for segment in segments {
        segment.referenced_relations = segment
            .referenced_symbols
            .iter()
            .map(|symbol| ParsedRelation {
                symbol: symbol.clone(),
                edge_identity_kind: EDGE_IDENTITY_DOC_MENTION.to_string(),
                kind: None,
            })
            .collect();
    }
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

    fn assert_doc_mention_rows(segment: &ParsedSegment) {
        assert_eq!(
            segment.referenced_relations.len(),
            segment.referenced_symbols.len()
        );
        for (relation, symbol) in segment
            .referenced_relations
            .iter()
            .zip(&segment.referenced_symbols)
        {
            assert_eq!(relation.symbol, *symbol);
            assert_eq!(relation.edge_identity_kind, EDGE_IDENTITY_DOC_MENTION);
            assert_eq!(relation.kind, None);
        }
    }

    #[test]
    fn single_token_code_span_emits_mention() {
        let content = "# Guide\n\nintro prose only\n\n## Usage\n\nCall `generate_segment_id` first, then run `reindex` when stale.\n";
        let segments = parse_markdown_file(content, "README");

        let guide = segment_by_breadcrumb(&segments, "README > Guide");
        assert!(guide.referenced_symbols.is_empty());
        assert!(guide.referenced_relations.is_empty());

        let usage = segment_by_breadcrumb(&segments, "README > Guide > Usage");
        assert_eq!(
            usage.referenced_symbols,
            vec!["generate_segment_id", "reindex"]
        );
        assert_doc_mention_rows(usage);
    }

    #[test]
    fn qualified_chain_emits_full_raw_token() {
        let content =
            "# API\n\nSee `segments::generate_segment_id` and `Engine.search()` and `println!`.\n";
        let segments = parse_markdown_file(content, "DOC");

        let api = segment_by_breadcrumb(&segments, "DOC > API");
        assert_eq!(
            api.referenced_symbols,
            vec![
                "segments::generate_segment_id",
                "Engine.search()",
                "println!"
            ]
        );
        assert_doc_mention_rows(api);
    }

    #[test]
    fn fenced_tokens_use_strict_tier_and_stoplist() {
        let content = "# Example\n\n```rust\nlet total = compute_total(items);\npub fn helper() { return True; }\nstorage::replace_rows(batch)\n```\n";
        let segments = parse_markdown_file(content, "DOC");

        let example = segment_by_breadcrumb(&segments, "DOC > Example");
        assert_eq!(
            example.referenced_symbols,
            vec!["compute_total", "helper()", "storage::replace_rows"]
        );
        assert_doc_mention_rows(example);
    }

    #[test]
    fn prose_and_stoplist_words_rejected() {
        let content = "# Notes\n\nPlain prose mentioning segments and storage stays unscanned.\n\nThe span `use the helper` only has plain words.\n\n```text\nlet use return true false\nplain words and digits 123 here\n```\n";
        let segments = parse_markdown_file(content, "NOTES");

        for segment in &segments {
            assert!(
                segment.referenced_symbols.is_empty(),
                "expected no mentions, found {:?}",
                segment.referenced_symbols
            );
            assert!(segment.referenced_relations.is_empty());
        }
    }

    #[test]
    fn mention_cap_of_64_enforced() {
        let mut content = String::from("# Dump\n\n```\n");
        for i in 0..40 {
            content.push_str(&format!("alpha_{i:03} beta_{i:03} alpha_{i:03}\n"));
        }
        content.push_str("```\n");
        let segments = parse_markdown_file(&content, "BIG");

        assert_eq!(segments.len(), 1, "fixture section must not split");
        let dump = &segments[0];
        assert_eq!(dump.referenced_symbols.len(), MAX_DOC_MENTIONS_PER_SEGMENT);
        let unique: std::collections::BTreeSet<_> = dump.referenced_symbols.iter().collect();
        assert_eq!(unique.len(), MAX_DOC_MENTIONS_PER_SEGMENT);
        assert_eq!(dump.referenced_symbols[0], "alpha_000");
        assert_eq!(
            dump.referenced_symbols.last().map(String::as_str),
            Some("beta_031"),
            "cap keeps the first 64 unique tokens in scan order"
        );
        assert_doc_mention_rows(dump);
    }

    #[test]
    fn relations_nonempty_whenever_symbols_nonempty() {
        let content = "# Top\n\nUses `chunk_file` internally.\n\n## Fence\n\n```rust\nsegments::replace_for_context(batch)\n```\n\n## Empty\n\nno code here\n";
        let segments = parse_markdown_file(content, "README");

        let mut nonempty = 0;
        for segment in &segments {
            if !segment.referenced_symbols.is_empty() {
                nonempty += 1;
                assert!(
                    !segment.referenced_relations.is_empty(),
                    "symbols without relations would synthesize bare_identifier fallback rows"
                );
            }
            assert_doc_mention_rows(segment);
        }
        assert_eq!(nonempty, 2);
    }

    #[test]
    fn mention_attaches_to_covering_split_piece() {
        let mut content = String::from("# Big\n\n");
        for i in 1..=68 {
            content.push_str(&format!("filler line {i}\n"));
        }
        content.push_str("Ends by calling `final_helper_token` here.\n");
        let segments = parse_markdown_file(&content, "BIG");

        assert!(segments.len() > 1, "section must split");
        let mention_line = 71;
        let carrier = segments
            .iter()
            .find(|s| s.referenced_symbols == vec!["final_helper_token"])
            .expect("mention attaches to one split piece");
        assert!(
            carrier.line_start <= mention_line && mention_line <= carrier.line_end,
            "mention line {mention_line} outside carrier span {}-{}",
            carrier.line_start,
            carrier.line_end
        );
        assert_doc_mention_rows(carrier);
        assert_eq!(
            segments
                .iter()
                .filter(|s| !s.referenced_symbols.is_empty())
                .count(),
            1,
            "overlapping pieces must not duplicate the mention"
        );
    }

    #[test]
    fn lexical_inline_fallback_extracts_code_span_tokens() {
        let mentions = lexical_backtick_mentions(
            "Call `normalize_symbolish` or ``chunk_file`` but skip `the plain words`.",
            5,
        );

        let tokens: Vec<&str> = mentions.iter().map(|m| m.token.as_str()).collect();
        assert_eq!(tokens, vec!["normalize_symbolish", "chunk_file"]);
        assert!(mentions.iter().all(|m| m.line == 5));
    }
}
