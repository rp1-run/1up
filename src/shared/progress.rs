use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use serde::de::DeserializeOwned;

/// Error explaining why a present status file could not be turned into a value.
///
/// Kept as a typed source so call sites can log the underlying cause; the
/// classifier never acts on the distinction itself.
#[derive(Debug, thiserror::Error)]
pub enum StatusReadError {
    #[error("failed to read status file: {0}")]
    Io(#[source] std::io::Error),
    #[error("failed to parse status file: {0}")]
    Parse(#[source] serde_json::Error),
}

/// Three-state outcome of reading a JSON status file.
///
/// This makes the "not yet written" case (`Absent`) distinct from the
/// "present but torn/corrupt" case (`Unreadable`), so a reader never silently
/// degrades a partial write to an empty/default value. Producing this outcome is
/// pure: [`read_status_file`] performs a single attempt with no sleeps, and any
/// retry-or-propagate policy lives at the call site.
#[derive(Debug)]
pub enum StatusFileRead<T> {
    /// The file does not exist yet (a legitimate not-yet-written state).
    Absent,
    /// The file was read and parsed into a value.
    Parsed(T),
    /// The file exists but could not be read or parsed (e.g. a torn write).
    Unreadable(StatusReadError),
}

/// Classify a single read of a JSON status file into [`StatusFileRead`].
///
/// Pure and sleep-free: exactly one filesystem read and one parse attempt. A
/// missing file maps to [`StatusFileRead::Absent`]; any other IO failure or a
/// parse failure (including an empty or truncated file) maps to
/// [`StatusFileRead::Unreadable`], never a default. Callers own retry/propagate
/// policy via [`crate::shared::constants::STATUS_READ_RETRY_ATTEMPTS`] and
/// [`crate::shared::constants::STATUS_READ_RETRY_DELAY_MS`].
pub fn read_status_file<T: DeserializeOwned>(path: &Path) -> StatusFileRead<T> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return StatusFileRead::Absent,
        Err(err) => return StatusFileRead::Unreadable(StatusReadError::Io(err)),
    };
    match serde_json::from_str(&content) {
        Ok(value) => StatusFileRead::Parsed(value),
        Err(err) => StatusFileRead::Unreadable(StatusReadError::Parse(err)),
    }
}

const PROGRESS_TICK_INTERVAL: Duration = Duration::from_millis(80);
const DRAW_RATE_HZ: u8 = 10;
const SPINNER_TICKS: &[&str] = &["-", "\\", "|", "/"];

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner} {msg}")
        .expect("spinner template is valid")
        .tick_strings(SPINNER_TICKS)
}

fn item_bar_style() -> ProgressStyle {
    ProgressStyle::with_template("{bar:40.cyan/blue} {pos}/{len} {percent:>3}% {msg}")
        .expect("item progress template is valid")
        .progress_chars("=> ")
}

fn byte_bar_style() -> ProgressStyle {
    ProgressStyle::with_template("{bar:40.cyan/blue} {percent:>3}% {msg}")
        .expect("byte progress template is valid")
        .progress_chars("=> ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressUnit {
    Items,
    Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressState {
    Spinner {
        message: String,
    },
    Bounded {
        message: String,
        current: u64,
        total: u64,
        unit: ProgressUnit,
    },
}

impl ProgressState {
    pub fn spinner(message: impl Into<String>) -> Self {
        Self::Spinner {
            message: message.into(),
        }
    }

    pub fn items(message: impl Into<String>, current: u64, total: u64) -> Self {
        Self::Bounded {
            message: message.into(),
            current,
            total,
            unit: ProgressUnit::Items,
        }
    }

    pub fn bytes(message: impl Into<String>, current: u64, total: u64) -> Self {
        Self::Bounded {
            message: message.into(),
            current,
            total,
            unit: ProgressUnit::Bytes,
        }
    }

    fn kind(&self) -> ProgressKind {
        match self {
            Self::Spinner { .. } => ProgressKind::Spinner,
            Self::Bounded {
                unit: ProgressUnit::Items,
                ..
            } => ProgressKind::Items,
            Self::Bounded {
                unit: ProgressUnit::Bytes,
                ..
            } => ProgressKind::Bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProgressKind {
    Spinner,
    Items,
    Bytes,
}

#[derive(Debug)]
pub struct ProgressUi {
    bar: ProgressBar,
    visible: bool,
    active_kind: Option<ProgressKind>,
}

impl ProgressUi {
    pub fn stderr_if(state: ProgressState, enabled: bool) -> Self {
        let visible = enabled && std::io::stderr().is_terminal();
        let mut ui = Self {
            bar: ProgressBar::hidden(),
            visible,
            active_kind: None,
        };
        ui.set_state(state);
        ui
    }

    pub fn set_state(&mut self, state: ProgressState) {
        if !self.visible {
            self.active_kind = Some(state.kind());
            return;
        }

        let next_kind = state.kind();
        if self.active_kind != Some(next_kind) {
            if self.active_kind.is_some() {
                self.bar.finish_and_clear();
            }
            self.bar = create_progress_bar(next_kind);
            self.active_kind = Some(next_kind);
        }

        apply_state(&self.bar, &state);
    }

    pub fn success(&mut self) {
        self.finish(None, false);
    }

    pub fn success_with(&mut self, message: impl Into<String>) {
        self.finish(Some(message.into()), false);
    }

    pub fn warn_with(&mut self, message: impl Into<String>) {
        self.finish(Some(message.into()), true);
    }

    fn finish(&mut self, message: Option<String>, warning: bool) {
        self.active_kind = None;
        if !self.visible {
            return;
        }

        self.bar.finish_and_clear();
        if let Some(message) = message {
            if warning {
                eprintln!("warning: {message}");
            } else {
                eprintln!("{message}");
            }
        }
        self.bar = ProgressBar::hidden();
    }
}

fn create_progress_bar(kind: ProgressKind) -> ProgressBar {
    let bar = match kind {
        ProgressKind::Spinner => {
            let bar = ProgressBar::new_spinner();
            bar.set_style(spinner_style());
            bar.enable_steady_tick(PROGRESS_TICK_INTERVAL);
            bar
        }
        ProgressKind::Items => {
            let bar = ProgressBar::new(1);
            bar.set_style(item_bar_style());
            bar
        }
        ProgressKind::Bytes => {
            let bar = ProgressBar::new(1);
            bar.set_style(byte_bar_style());
            bar
        }
    };

    bar.set_draw_target(ProgressDrawTarget::stderr_with_hz(DRAW_RATE_HZ));
    bar
}

fn apply_state(bar: &ProgressBar, state: &ProgressState) {
    match state {
        ProgressState::Spinner { message } => {
            bar.set_message(message.clone());
        }
        ProgressState::Bounded {
            message,
            current,
            total,
            ..
        } => {
            let total = (*total).max(1);
            bar.set_length(total);
            bar.set_position((*current).min(total));
            bar.set_message(message.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_state_preserves_units() {
        assert_eq!(
            ProgressState::items("Processing files", 2, 5),
            ProgressState::Bounded {
                message: "Processing files".to_string(),
                current: 2,
                total: 5,
                unit: ProgressUnit::Items,
            }
        );
        assert_eq!(
            ProgressState::bytes("Downloading model", 3, 9),
            ProgressState::Bounded {
                message: "Downloading model".to_string(),
                current: 3,
                total: 9,
                unit: ProgressUnit::Bytes,
            }
        );
    }

    #[test]
    fn hidden_progress_ui_accepts_mode_transitions() {
        let mut ui = ProgressUi::stderr_if(ProgressState::spinner("Scanning files"), false);
        ui.set_state(ProgressState::items("Processing files", 1, 4));
        ui.set_state(ProgressState::bytes("Downloading model", 2, 8));
        ui.success_with("complete");
    }

    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct SampleStatus {
        progress: u32,
    }

    #[test]
    fn read_status_file_absent_file_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing_status.json");

        match read_status_file::<SampleStatus>(&path) {
            StatusFileRead::Absent => {}
            other => panic!("expected Absent, got {other:?}"),
        }
    }

    #[test]
    fn read_status_file_valid_json_is_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("status.json");
        std::fs::write(&path, br#"{"progress":42}"#).unwrap();

        match read_status_file::<SampleStatus>(&path) {
            StatusFileRead::Parsed(value) => {
                assert_eq!(value, SampleStatus { progress: 42 });
            }
            other => panic!("expected Parsed, got {other:?}"),
        }
    }

    #[test]
    fn read_status_file_torn_content_is_unreadable() {
        // Both historically observed torn-write symptoms: truncated JSON and a
        // zero-length file. Same classifier branch, kept together.
        for torn in [&br#"{"progress":4"#[..], &b""[..]] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("status.json");
            std::fs::write(&path, torn).unwrap();

            match read_status_file::<SampleStatus>(&path) {
                StatusFileRead::Unreadable(StatusReadError::Parse(_)) => {}
                other => panic!("expected Unreadable(Parse) for {torn:?}, got {other:?}"),
            }
        }
    }
}
