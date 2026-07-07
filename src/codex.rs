//! Runs the Codex CLI headlessly for a single message.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use uuid::Uuid;

use crate::agent::{Request, RunError, RunOutput};

/// Runner invokes `codex exec` in non-interactive mode.
pub struct Runner {
    pub bin: String,
    pub sandbox: String,
    pub approval_policy: String,
    pub model: Option<String>,
}

#[derive(Deserialize)]
struct JsonEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    item: Value,
}

impl Runner {
    /// Executes one turn and returns Codex's final reply plus the Codex session id.
    pub async fn run(&self, req: Request<'_>, timeout: Duration) -> Result<RunOutput, RunError> {
        let out_path = Path::new(req.work_dir)
            .join(format!(".push-codex-last-message-{}.txt", Uuid::new_v4()));
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--ask-for-approval")
            .arg(&self.approval_policy)
            .arg("--sandbox")
            .arg(&self.sandbox);
        if req.is_new {
            cmd.arg("exec")
                .arg("--json")
                .arg("--skip-git-repo-check")
                .arg("-C")
                .arg(req.work_dir)
                .arg("--add-dir")
                .arg(req.work_dir)
                .arg("-o")
                .arg(&out_path);
            if let Some(model) = self.model.as_deref() {
                cmd.arg("-m").arg(model);
            }
            cmd.arg(prompt(&req));
        } else {
            cmd.arg("exec")
                .arg("resume")
                .arg("--json")
                .arg("--skip-git-repo-check")
                .arg("-o")
                .arg(&out_path);
            if let Some(model) = self.model.as_deref() {
                cmd.arg("-m").arg(model);
            }
            cmd.arg(req.session_id).arg(prompt(&req));
        }
        cmd.current_dir(req.work_dir);
        cmd.kill_on_drop(true);

        let out = match tokio::time::timeout(timeout, cmd.output()).await {
            Err(_) => return Err(RunError::Timeout),
            Ok(Err(e)) => return Err(RunError::Failed(format!("spawn codex: {e}"))),
            Ok(Ok(o)) => o,
        };

        let stdout = String::from_utf8_lossy(&out.stdout);
        let session_id = session_id_from_jsonl(&stdout);
        let reply = std::fs::read_to_string(&out_path)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| last_agent_message_from_jsonl(&stdout))
            .unwrap_or_default();
        let _ = std::fs::remove_file(&out_path);

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(RunError::Failed(if stderr.is_empty() {
                "codex exited without a final reply".to_string()
            } else {
                stderr
            }));
        }
        if req.is_new && session_id.is_none() {
            return Err(RunError::Failed(
                "codex did not report a session id".to_string(),
            ));
        }
        if reply.trim().is_empty() {
            return Err(RunError::Failed(
                "codex exited without a final reply".to_string(),
            ));
        }

        Ok(RunOutput {
            reply: reply.trim().to_string(),
            session_id,
        })
    }
}

fn prompt(req: &Request<'_>) -> String {
    if req.system_append.trim().is_empty() {
        return req.prompt.to_string();
    }
    format!(
        "You are running as a personal assistant through the push gateway.\n\nAssistant context:\n{}\n\nUser message:\n{}",
        req.system_append.trim(),
        req.prompt
    )
}

fn session_id_from_jsonl(s: &str) -> Option<String> {
    s.lines().find_map(|line| {
        let ev: JsonEvent = serde_json::from_str(line).ok()?;
        if ev.kind != "thread.started" {
            return None;
        }
        let thread_id = ev.thread_id?;
        let thread_id = thread_id.trim();
        (!thread_id.is_empty()).then(|| thread_id.to_string())
    })
}

fn last_agent_message_from_jsonl(s: &str) -> Option<String> {
    s.lines().filter_map(agent_message_from_line).next_back()
}

fn agent_message_from_line(line: &str) -> Option<String> {
    let ev: JsonEvent = serde_json::from_str(line).ok()?;
    if ev.kind != "item.completed" {
        return None;
    }
    if ev.item.get("type")?.as_str()? != "agent_message" {
        return None;
    }
    ev.item.get("text")?.as_str().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_thread_id() {
        let s = r#"{"type":"thread.started","thread_id":"abc"}"#;
        assert_eq!(session_id_from_jsonl(s), Some("abc".to_string()));
    }

    #[test]
    fn ignores_empty_thread_id() {
        let s = r#"{"type":"thread.started","thread_id":" \t\n "}"#;
        assert_eq!(session_id_from_jsonl(s), None);
    }

    #[test]
    fn extracts_last_agent_message() {
        let s = concat!(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"one"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"two"}}"#
        );
        assert_eq!(last_agent_message_from_jsonl(s), Some("two".to_string()));
    }
}
