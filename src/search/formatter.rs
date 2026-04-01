use crate::shared::types::SearchResult;

pub fn truncate_content(content: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= max_lines {
        content.to_string()
    } else {
        let mut result: String = lines[..max_lines].join("\n");
        result.push_str("\n...");
        result
    }
}

pub fn format_score(score: f64) -> String {
    format!("{:.4}", score)
}

pub fn summarize_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "No results found.".to_string();
    }
    let files: std::collections::HashSet<&str> =
        results.iter().map(|r| r.file_path.as_str()).collect();
    format!("{} results across {} files", results.len(), files.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_content() {
        let content = "line1\nline2";
        assert_eq!(truncate_content(content, 5), content);
    }

    #[test]
    fn truncate_long_content() {
        let content = "a\nb\nc\nd\ne\nf\ng";
        let truncated = truncate_content(content, 3);
        assert!(truncated.ends_with("..."));
        assert_eq!(truncated.lines().count(), 4);
    }

    #[test]
    fn score_formatting() {
        assert_eq!(format_score(0.123456), "0.1235");
        assert_eq!(format_score(1.0), "1.0000");
    }
}
