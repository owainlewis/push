//! User-owned assistant context (User.md, Memory.md) injected into every run.

use std::path::Path;

use crate::config::AssistantProfile;

const FILES: [&str; 2] = ["User.md", "Memory.md"];

/// Reads the assistant memory files from `dir` and joins them for
/// `--append-system-prompt`. Missing or empty files are skipped.
pub fn load(dir: &str, profile: &AssistantProfile) -> String {
    let mut parts = Vec::new();
    if let Some(s) = profile_prompt(profile) {
        parts.push(s);
    }
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

fn profile_prompt(profile: &AssistantProfile) -> Option<String> {
    let mut lines = Vec::new();
    if !profile.name.trim().is_empty() {
        lines.push(format!("Name: {}", profile.name.trim()));
    }
    if !profile.tone.trim().is_empty() {
        lines.push(format!("Tone: {}", profile.tone.trim()));
    }
    if !profile.business.trim().is_empty() {
        lines.push(format!("Business: {}", profile.business.trim()));
    }
    if !profile.projects.is_empty() {
        lines.push("Projects:".to_string());
        for p in &profile.projects {
            if !p.trim().is_empty() {
                lines.push(format!("- {}", p.trim()));
            }
        }
    }
    if !profile.preferences.is_empty() {
        lines.push("Preferences:".to_string());
        for p in &profile.preferences {
            if !p.trim().is_empty() {
                lines.push(format!("- {}", p.trim()));
            }
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(format!("Assistant profile:\n{}", lines.join("\n")))
    }
}
