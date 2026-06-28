//! Runs the Claude Code CLI headlessly for a single message.

use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::agent::{Request, RunError, RunOutput};

/// Runner invokes the `claude` binary in print mode.
pub struct Runner {
    pub bin: String,
    pub permission_mode: String,
}

#[derive(Deserialize, Default)]
struct CliResult {
    #[serde(default)]
    result: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    subtype: String,
}

impl Runner {
    /// Executes one turn and returns Claude's reply text, or a RunError.
    pub async fn run(&self, req: Request<'_>, timeout: Duration) -> Result<RunOutput, RunError> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("-p")
            .arg(req.prompt)
            .arg("--output-format")
            .arg("json")
            .arg("--permission-mode")
            .arg(&self.permission_mode)
            .arg("--add-dir")
            .arg(req.work_dir);
        if req.is_new {
            cmd.arg("--session-id").arg(req.session_id);
        } else {
            cmd.arg("--resume").arg(req.session_id);
        }
        if !req.system_append.trim().is_empty() {
            cmd.arg("--append-system-prompt").arg(req.system_append);
        }
        cmd.current_dir(req.work_dir);
        cmd.kill_on_drop(true);

        let out = match tokio::time::timeout(timeout, cmd.output()).await {
            Err(_) => return Err(RunError::Timeout),
            Ok(Err(e)) => return Err(RunError::Failed(format!("spawn claude: {e}"))),
            Ok(Ok(o)) => o,
        };

        // claude prints its JSON envelope to stdout even when it exits non-zero
        // (e.g. an API error), so parse stdout regardless of exit status.
        match serde_json::from_slice::<CliResult>(&out.stdout) {
            Ok(r) if r.is_error => {
                let msg = if r.result.is_empty() {
                    r.subtype
                } else {
                    r.result
                };
                Err(RunError::Failed(msg))
            }
            Ok(r) => Ok(RunOutput {
                reply: r.result.trim().to_string(),
                session_id: Some(r.session_id),
            }),
            Err(_) => {
                if out.status.success() {
                    Ok(RunOutput {
                        reply: String::from_utf8_lossy(&out.stdout).trim().to_string(),
                        session_id: None,
                    })
                } else {
                    Err(RunError::Failed(
                        String::from_utf8_lossy(&out.stderr).trim().to_string(),
                    ))
                }
            }
        }
    }
}
