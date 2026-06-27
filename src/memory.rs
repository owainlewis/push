//! User-owned assistant context (User.md, Memory.md) injected into every run.

use std::path::Path;

const FILES: [&str; 2] = ["User.md", "Memory.md"];

/// Reads the assistant memory files from `dir` and joins them for
/// `--append-system-prompt`. Missing or empty files are skipped.
pub fn load(dir: &str) -> String {
    let mut parts = Vec::new();
    for name in FILES {
        if let Ok(s) = std::fs::read_to_string(Path::new(dir).join(name)) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
    }
    parts.join("\n\n")
}
