//! Cosmetic tool-progress feedback for chat. Never enters canonical history.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One tool lifecycle event from a backend JSON stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressEvent {
    pub tool_name: String,
    pub preview: String,
    pub phase: ProgressPhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressPhase {
    Start,
    End { is_error: bool },
}

/// Per-conversation `/stream` toggle. In-memory only; restarts reset to off.
#[derive(Clone, Default)]
pub struct StreamPrefs {
    enabled: Arc<Mutex<HashMap<String, bool>>>,
}

impl StreamPrefs {
    pub fn is_enabled(&self, thread: &str) -> bool {
        self.enabled
            .lock()
            .unwrap()
            .get(thread)
            .copied()
            .unwrap_or(false)
    }

    pub fn set(&self, thread: &str, enabled: bool) {
        self.enabled
            .lock()
            .unwrap()
            .insert(thread.to_string(), enabled);
    }

    pub fn toggle(&self, thread: &str) -> bool {
        let enabled = !self.is_enabled(thread);
        self.set(thread, enabled);
        enabled
    }
}

const PREVIEW_MAX: usize = 100;

/// Hermes-style cosmetic line: `⚡ Running ls -la …`
pub fn format_progress_line(event: &ProgressEvent) -> String {
    let verb = tool_verb(&event.tool_name);
    let body = if event.preview.is_empty() {
        verb
    } else {
        format!("{verb} {}", &event.preview)
    };
    let body = truncate_one_line(&body, PREVIEW_MAX);
    match &event.phase {
        ProgressPhase::Start => format!("⚡ {body}"),
        ProgressPhase::End { is_error: true } => format!("⚡ Failed {body}"),
        ProgressPhase::End { is_error: false } => format!("⚡ Done {body}"),
    }
}

fn tool_verb(tool_name: &str) -> String {
    match tool_name {
        "bash" | "terminal" | "execute_code" => "Running".to_string(),
        "read" | "read_file" => "Reading".to_string(),
        "write" | "write_file" => "Writing".to_string(),
        "edit" | "patch" => "Editing".to_string(),
        "grep" | "find" | "search_files" | "web_search" => "Searching".to_string(),
        "ls" => "Listing".to_string(),
        "" => "Working".to_string(),
        other => format!("Using {other}"),
    }
}

/// Build a short one-line preview from Pi/tool JSON args.
pub fn preview_from_args(tool_name: &str, args: &serde_json::Value) -> String {
    let raw = match tool_name {
        "bash" | "terminal" => args
            .get("command")
            .or_else(|| args.get("cmd"))
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "read" | "read_file" | "write" | "write_file" | "edit" | "patch" | "ls" => args
            .get("path")
            .or_else(|| args.get("file_path"))
            .or_else(|| args.get("file"))
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "grep" | "search_files" | "web_search" | "find" => args
            .get("pattern")
            .or_else(|| args.get("query"))
            .or_else(|| args.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        _ => args
            .as_object()
            .and_then(|obj| {
                obj.values()
                    .find_map(|v| v.as_str())
                    .or_else(|| obj.keys().next().map(|k| k.as_str()))
            })
            .unwrap_or(""),
    };
    truncate_one_line(raw, PREVIEW_MAX)
}

fn truncate_one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out = String::new();
    for ch in flat.chars() {
        if out.chars().count() + 1 >= max.saturating_sub(1) {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stream_prefs_default_off_and_toggle() {
        let prefs = StreamPrefs::default();
        assert!(!prefs.is_enabled("telegram:dm:1"));
        assert!(prefs.toggle("telegram:dm:1"));
        assert!(prefs.is_enabled("telegram:dm:1"));
        prefs.set("telegram:dm:1", false);
        assert!(!prefs.is_enabled("telegram:dm:1"));
    }

    #[test]
    fn formats_hermes_style_bash_line() {
        let line = format_progress_line(&ProgressEvent {
            tool_name: "bash".into(),
            preview: "curl https://example.com".into(),
            phase: ProgressPhase::Start,
        });
        assert_eq!(line, "⚡ Running curl https://example.com");
    }

    #[test]
    fn preview_prefers_command_for_bash() {
        assert_eq!(
            preview_from_args("bash", &json!({"command": "ls -la"})),
            "ls -la"
        );
    }
}
