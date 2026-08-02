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

const PREVIEW_MAX: usize = 120;
const PROGRESS_MESSAGE_MAX: usize = 3500;

/// Hermes-style Markdown block for one tool start (Telegram renders fences as copyable).
pub fn format_progress_block(event: &ProgressEvent) -> String {
    let preview = truncate_one_line(&event.preview, PREVIEW_MAX);
    let failed = matches!(event.phase, ProgressPhase::End { is_error: true });
    let prefix = if failed { "⚠️ " } else { "" };
    match event.tool_name.as_str() {
        "bash" | "terminal" | "execute_code" => {
            let cmd = if preview.is_empty() { "…" } else { &preview };
            format!("{prefix}💻 Shell\n```\n{cmd}\n```")
        }
        "read" | "read_file" => {
            let path = if preview.is_empty() { "…" } else { &preview };
            format!("{prefix}📖 read\n`{path}`")
        }
        "write" | "write_file" => {
            let path = if preview.is_empty() { "…" } else { &preview };
            format!("{prefix}✍️ write\n`{path}`")
        }
        "edit" | "patch" => {
            let path = if preview.is_empty() { "…" } else { &preview };
            format!("{prefix}✏️ edit\n`{path}`")
        }
        "grep" | "find" | "search_files" | "web_search" => {
            let q = if preview.is_empty() { "…" } else { &preview };
            format!("{prefix}🔍 search\n`{q}`")
        }
        "ls" => {
            let path = if preview.is_empty() { "." } else { &preview };
            format!("{prefix}📁 ls\n`{path}`")
        }
        other => {
            let name = if other.is_empty() { "tool" } else { other };
            if preview.is_empty() {
                format!("{prefix}⚙️ {name}")
            } else {
                format!("{prefix}⚙️ {name}\n`{preview}`")
            }
        }
    }
}

/// Append a new tool block into an accumulating progress message.
pub fn append_progress_message(existing: &str, block: &str) -> String {
    let combined = if existing.is_empty() {
        block.to_string()
    } else {
        format!("{existing}\n\n{block}")
    };
    trim_progress_message(&combined, PROGRESS_MESSAGE_MAX)
}

fn trim_progress_message(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut blocks: Vec<&str> = text.split("\n\n").collect();
    while blocks.len() > 1 && blocks.join("\n\n").chars().count() > max {
        blocks.remove(0);
    }
    let joined = blocks.join("\n\n");
    if joined.chars().count() <= max {
        format!("…\n\n{joined}")
    } else {
        let mut out = String::from("…\n\n");
        for ch in joined
            .chars()
            .rev()
            .take(max.saturating_sub(4))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            out.push(ch);
        }
        out
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
    fn formats_hermes_style_shell_fence() {
        let block = format_progress_block(&ProgressEvent {
            tool_name: "bash".into(),
            preview: "curl https://example.com".into(),
            phase: ProgressPhase::Start,
        });
        assert!(block.contains("💻 Shell"));
        assert!(block.contains("```\ncurl https://example.com\n```"));
    }

    #[test]
    fn preview_prefers_command_for_bash() {
        assert_eq!(
            preview_from_args("bash", &json!({"command": "ls -la"})),
            "ls -la"
        );
    }

    #[test]
    fn append_keeps_multiple_tool_blocks() {
        let first = format_progress_block(&ProgressEvent {
            tool_name: "bash".into(),
            preview: "ls".into(),
            phase: ProgressPhase::Start,
        });
        let second = format_progress_block(&ProgressEvent {
            tool_name: "read".into(),
            preview: "a.txt".into(),
            phase: ProgressPhase::Start,
        });
        let msg = append_progress_message(&first, &second);
        assert!(msg.contains("Shell"));
        assert!(msg.contains("read"));
    }
}
