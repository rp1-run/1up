use crate::shared::errors::FenceError;

/// Condensed agent reminder, embedded at compile time from src/reminder.md.
pub const CONDENSED_REMINDER: &str = include_str!("../reminder.md");

/// 1up version from Cargo.toml, embedded at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Fence marker prefix used to identify 1up-managed sections.
pub const FENCE_PREFIX: &str = "<!-- 1up:start:";

/// Fence marker suffix used to identify 1up-managed section ends.
pub const FENCE_SUFFIX: &str = "<!-- 1up:end:";

const MARKER_CLOSE: &str = " -->";

/// Parsed representation of an existing 1up fence in file content.
pub struct ExistingFence {
    pub version: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, PartialEq)]
pub enum FenceAction {
    Created,
    Updated { old_version: String },
    AlreadyCurrent,
}

/// Returns the full fenced reminder block with versioned markers.
pub fn fenced_reminder() -> String {
    format!(
        "{}{}{}\n{}\n{}{}{}",
        FENCE_PREFIX,
        VERSION,
        MARKER_CLOSE,
        CONDENSED_REMINDER.trim_end(),
        FENCE_SUFFIX,
        VERSION,
        MARKER_CLOSE,
    )
}

/// Scans file content for an existing 1up fence.
///
/// Returns `Ok(None)` if no fence is found.
/// Returns `Ok(Some(ExistingFence))` if a well-formed fence is found.
/// Returns `Err(FenceError)` if the fence markers are malformed
/// (e.g., start without end, end without start, or end before start).
pub fn find_fence(content: &str) -> Result<Option<ExistingFence>, FenceError> {
    let lines: Vec<&str> = content.lines().collect();
    let mut start_info: Option<(usize, String)> = None;
    let mut end_line: Option<usize> = None;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(FENCE_PREFIX) {
            if let Some(version) = rest.strip_suffix(MARKER_CLOSE) {
                start_info = Some((i, version.to_string()));
            }
        } else if let Some(rest) = trimmed.strip_prefix(FENCE_SUFFIX) {
            if rest.strip_suffix(MARKER_CLOSE).is_some() {
                end_line = Some(i);
            }
        }
    }

    match (start_info, end_line) {
        (Some((start, version)), Some(end)) => {
            if end <= start {
                return Err(FenceError::Malformed(
                    "end marker appears before or at start marker".to_string(),
                ));
            }
            Ok(Some(ExistingFence {
                version,
                start_line: start,
                end_line: end,
            }))
        }
        (Some(_), None) => Err(FenceError::Malformed(
            "start marker without matching end marker".to_string(),
        )),
        (None, Some(_)) => Err(FenceError::Malformed(
            "end marker without matching start marker".to_string(),
        )),
        (None, None) => Ok(None),
    }
}

/// Applies the fenced reminder to file content.
///
/// Handles three cases:
/// - `None` existing content: returns the fenced block as the entire file.
/// - Content without a 1up fence: appends the fence after a blank line separator.
/// - Content with a 1up fence: replaces the fenced section, preserving surrounding content.
///
/// Returns the resulting content and a [`FenceAction`] describing what changed.
/// When the existing fence matches the current version and content,
/// returns `FenceAction::AlreadyCurrent` with the original content unchanged.
pub fn apply_fence(existing_content: Option<&str>) -> (String, FenceAction) {
    let new_fence = fenced_reminder();

    match existing_content {
        None => (format!("{new_fence}\n"), FenceAction::Created),
        Some(content) => match find_fence(content) {
            Ok(Some(existing)) => {
                let lines: Vec<&str> = content.lines().collect();
                let existing_fenced: String =
                    lines[existing.start_line..=existing.end_line].join("\n");

                if existing.version == VERSION && existing_fenced == new_fence {
                    return (content.to_string(), FenceAction::AlreadyCurrent);
                }

                let mut result = String::new();
                for line in &lines[..existing.start_line] {
                    result.push_str(line);
                    result.push('\n');
                }
                result.push_str(&new_fence);
                result.push('\n');
                for line in lines.iter().skip(existing.end_line + 1) {
                    result.push_str(line);
                    result.push('\n');
                }

                let old_version = existing.version;
                (result, FenceAction::Updated { old_version })
            }
            Ok(None) => {
                let mut result = content.to_string();
                if !result.ends_with('\n') {
                    result.push('\n');
                }
                result.push('\n');
                result.push_str(&new_fence);
                result.push('\n');
                (result, FenceAction::Created)
            }
            Err(_) => {
                let mut result = content.to_string();
                if !result.ends_with('\n') {
                    result.push('\n');
                }
                result.push('\n');
                result.push_str(&new_fence);
                result.push('\n');
                (result, FenceAction::Created)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_reminder_contains_version_markers() {
        let fenced = fenced_reminder();
        let expected_start = format!("{FENCE_PREFIX}{VERSION}{MARKER_CLOSE}");
        let expected_end = format!("{FENCE_SUFFIX}{VERSION}{MARKER_CLOSE}");

        assert!(
            fenced.starts_with(&expected_start),
            "fenced reminder should start with versioned marker, got: {}",
            fenced.lines().next().unwrap_or("")
        );
        assert!(
            fenced.ends_with(&expected_end),
            "fenced reminder should end with versioned marker, got: {}",
            fenced.lines().last().unwrap_or("")
        );
    }

    #[test]
    fn fenced_reminder_contains_condensed_content() {
        let fenced = fenced_reminder();
        let trimmed = CONDENSED_REMINDER.trim_end();
        assert!(
            fenced.contains(trimmed),
            "fenced reminder should contain the condensed reminder content"
        );
    }

    #[test]
    fn find_fence_detects_existing_fence() {
        let content = format!(
            "Some preamble\n\n{FENCE_PREFIX}0.0.9{MARKER_CLOSE}\nold content\n{FENCE_SUFFIX}0.0.9{MARKER_CLOSE}\n\nSome postamble\n"
        );
        let result = find_fence(&content).unwrap();
        let fence = result.expect("should detect existing fence");
        assert_eq!(fence.version, "0.0.9");
        assert_eq!(fence.start_line, 2);
        assert_eq!(fence.end_line, 4);
    }

    #[test]
    fn find_fence_returns_none_when_no_fence() {
        let content = "# My Project\n\nSome instructions for agents.\n";
        let result = find_fence(content).unwrap();
        assert!(result.is_none(), "should return None when no fence present");
    }

    #[test]
    fn find_fence_errors_on_malformed_fence() {
        let content = format!("{FENCE_PREFIX}0.1.0{MARKER_CLOSE}\nsome content\n");
        let result = find_fence(&content);
        assert!(result.is_err(), "should error on start marker without end");

        let content = format!("some content\n{FENCE_SUFFIX}0.1.0{MARKER_CLOSE}\n");
        let result = find_fence(&content);
        assert!(result.is_err(), "should error on end marker without start");
    }

    #[test]
    fn find_fence_ignores_other_tool_fences() {
        let content = "<!-- rp1:start:v0.7.1 -->\nrp1 managed content\n<!-- rp1:end:v0.7.1 -->\n";
        let result = find_fence(content).unwrap();
        assert!(
            result.is_none(),
            "should not match rp1 fence markers as 1up fences"
        );
    }

    #[test]
    fn apply_fence_creates_new_content() {
        let (content, action) = apply_fence(None);
        let expected_start = format!("{FENCE_PREFIX}{VERSION}{MARKER_CLOSE}");

        assert_eq!(action, FenceAction::Created);
        assert!(content.contains(&expected_start));
        assert!(content.contains(CONDENSED_REMINDER.trim_end()));
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn apply_fence_appends_to_existing_content() {
        let existing = "# My Project\n\nCustom agent instructions here.\n";
        let (content, action) = apply_fence(Some(existing));

        assert_eq!(action, FenceAction::Created);
        assert!(
            content.starts_with("# My Project\n"),
            "should preserve existing content at the start"
        );
        assert!(
            content.contains(CONDENSED_REMINDER.trim_end()),
            "should contain the reminder"
        );
        let fence_start = format!("{FENCE_PREFIX}{VERSION}{MARKER_CLOSE}");
        assert!(content.contains(&fence_start));
    }

    #[test]
    fn apply_fence_replaces_stale_fence() {
        let old_content = format!(
            "Preamble\n\n{FENCE_PREFIX}0.0.9{MARKER_CLOSE}\nold reminder\n{FENCE_SUFFIX}0.0.9{MARKER_CLOSE}\n\nPostamble\n"
        );
        let (content, action) = apply_fence(Some(&old_content));

        assert_eq!(
            action,
            FenceAction::Updated {
                old_version: "0.0.9".to_string()
            }
        );
        assert!(
            content.starts_with("Preamble\n"),
            "should preserve preamble"
        );
        assert!(content.contains("Postamble"), "should preserve postamble");
        assert!(
            !content.contains("old reminder"),
            "should remove old fence content"
        );

        let new_start = format!("{FENCE_PREFIX}{VERSION}{MARKER_CLOSE}");
        assert!(
            content.contains(&new_start),
            "should have new version marker"
        );
        assert!(content.contains(CONDENSED_REMINDER.trim_end()));
    }

    #[test]
    fn apply_fence_is_idempotent() {
        let (first_content, first_action) = apply_fence(None);
        assert_eq!(first_action, FenceAction::Created);

        let (second_content, second_action) = apply_fence(Some(&first_content));
        assert_eq!(second_action, FenceAction::AlreadyCurrent);
        assert_eq!(first_content, second_content, "content should be identical");
    }

    #[test]
    fn apply_fence_preserves_rp1_fences() {
        let rp1_section = "<!-- rp1:start:v0.7.1 -->\nrp1 managed content\n<!-- rp1:end:v0.7.1 -->";
        let existing = format!("# Instructions\n\n{rp1_section}\n\nSome other content\n");
        let (content, action) = apply_fence(Some(&existing));

        assert_eq!(action, FenceAction::Created);
        assert!(
            content.contains(rp1_section),
            "rp1 fenced section should be completely untouched"
        );
        assert!(
            content.contains("Some other content"),
            "other content should be preserved"
        );
        assert!(content.contains(CONDENSED_REMINDER.trim_end()));
    }
}
