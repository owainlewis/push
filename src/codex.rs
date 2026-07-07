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
    use std::time::Duration;

    use crate::agent::Request;
    use crate::test_support::{
        assert_runner_contract, sh_arg, temp_dir, temp_path, ContractCase, ContractRequest,
        ContractRunner, FakeCli, RunnerContract,
    };

    impl ContractRunner for Runner {
        fn run<'a>(
            &'a self,
            req: Request<'a>,
            timeout: Duration,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RunOutput, RunError>> + 'a>>
        {
            Box::pin(self.run(req, timeout))
        }
    }

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

    #[tokio::test]
    async fn satisfies_runner_contract() {
        assert_runner_contract(RunnerContract {
            name: "Codex",
            new_session: contract_new_session,
            resumed_session: contract_resumed_session,
            failed_run: contract_failed_run,
            timeout_run: contract_timeout_run,
        })
        .await;
    }

    #[tokio::test]
    async fn runs_new_session_and_reads_jsonl_thread_id() {
        let args_path = temp_path("codex-args");
        let work_dir = temp_dir("codex-work");
        let script = codex_success_script(&args_path, "codex reply", Some("codex-thread"));
        let cli = FakeCli::new("codex", &script);
        let runner = runner(cli.bin());

        let out = runner
            .run(
                Request {
                    session_id: "",
                    is_new: true,
                    work_dir: work_dir.to_str().unwrap(),
                    system_append: "assistant context",
                    prompt: "hello",
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(out.reply, "codex reply");
        assert_eq!(out.session_id, Some("codex-thread".to_string()));
        let args = read_args(&args_path);
        assert_arg_pair(&args, "--ask-for-approval", "never");
        assert_arg_pair(&args, "--sandbox", "workspace-write");
        assert_arg_present(&args, "exec");
        assert_arg_present(&args, "--json");
        assert_arg_pair(&args, "-C", work_dir.to_str().unwrap());
        assert_arg_pair(&args, "--add-dir", work_dir.to_str().unwrap());
        let raw_args = std::fs::read_to_string(&args_path).unwrap();
        assert!(raw_args.contains("Assistant context:"));
        assert!(raw_args.contains("User message:\nhello"));
    }

    #[tokio::test]
    async fn runs_resumed_session_with_resume_command() {
        let args_path = temp_path("codex-resume-args");
        let work_dir = temp_dir("codex-resume-work");
        let script = codex_success_script(&args_path, "resumed reply", None);
        let cli = FakeCli::new("codex", &script);
        let runner = runner(cli.bin());

        let out = runner
            .run(
                Request {
                    session_id: "existing-thread",
                    is_new: false,
                    work_dir: work_dir.to_str().unwrap(),
                    system_append: "",
                    prompt: "continue",
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(out.reply, "resumed reply");
        assert_eq!(out.session_id, None);
        let args = read_args(&args_path);
        assert_arg_sequence(&args, &["exec", "resume"]);
        assert_arg_present(&args, "existing-thread");
        assert!(!args.contains(&"-C".to_string()));
    }

    #[tokio::test]
    async fn reports_non_zero_exit_stderr() {
        let work_dir = temp_dir("codex-error-work");
        let cli = FakeCli::new(
            "codex",
            "#!/bin/sh\nprintf '%s\\n' 'codex failed' >&2\nexit 2\n",
        );
        let runner = runner(cli.bin());

        let err = match runner
            .run(request(work_dir.to_str().unwrap()), Duration::from_secs(5))
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("expected Codex run to fail"),
        };

        assert_failed(err, "codex failed");
    }

    #[tokio::test]
    async fn reports_timeout() {
        let work_dir = temp_dir("codex-timeout-work");
        let cli = FakeCli::new("codex", "#!/bin/sh\nsleep 2\n");
        let runner = runner(cli.bin());

        let err = match runner
            .run(
                request(work_dir.to_str().unwrap()),
                Duration::from_millis(10),
            )
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("expected Codex run to time out"),
        };

        assert_timeout(err);
    }

    fn runner(bin: String) -> Runner {
        Runner {
            bin,
            sandbox: "workspace-write".to_string(),
            approval_policy: "never".to_string(),
            model: None,
        }
    }

    fn request(work_dir: &str) -> Request<'_> {
        Request {
            session_id: "",
            is_new: true,
            work_dir,
            system_append: "",
            prompt: "hello",
        }
    }

    fn codex_success_script(
        args_path: &std::path::Path,
        reply: &str,
        thread_id: Option<&str>,
    ) -> String {
        let thread_event = thread_id
            .map(|id| {
                format!("printf '%s\\n' '{{\"type\":\"thread.started\",\"thread_id\":\"{id}\"}}'\n")
            })
            .unwrap_or_default();
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nout=''\nprev=''\nfor arg in \"$@\"; do\n  if [ \"$prev\" = '-o' ]; then out=\"$arg\"; fi\n  prev=\"$arg\"\ndone\nprintf '%s\\n' {} > \"$out\"\n{}",
            sh_arg(args_path),
            shell_string(reply),
            thread_event
        )
    }

    fn shell_string(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn read_args(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn assert_arg_pair(args: &[String], flag: &str, value: &str) {
        let idx = args
            .iter()
            .position(|arg| arg == flag)
            .unwrap_or_else(|| panic!("missing flag {flag} in {args:?}"));
        assert_eq!(args.get(idx + 1).map(String::as_str), Some(value));
    }

    fn assert_arg_present(args: &[String], value: &str) {
        assert!(
            args.iter().any(|arg| arg == value),
            "missing {value} in {args:?}"
        );
    }

    fn assert_arg_sequence(args: &[String], values: &[&str]) {
        assert!(
            args.windows(values.len())
                .any(|window| window.iter().map(String::as_str).eq(values.iter().copied())),
            "missing sequence {values:?} in {args:?}"
        );
    }

    fn assert_failed(err: RunError, expected: &str) {
        match err {
            RunError::Failed(msg) => assert_eq!(msg, expected),
            RunError::Timeout => panic!("expected failed error, got timeout"),
        }
    }

    fn assert_timeout(err: RunError) {
        match err {
            RunError::Timeout => {}
            RunError::Failed(msg) => panic!("expected timeout, got failed: {msg}"),
        }
    }

    fn contract_new_session() -> ContractCase {
        let work_dir = temp_dir("codex-contract-new");
        let cli = FakeCli::new(
            "codex",
            &codex_success_script(
                &temp_path("codex-contract-new-args"),
                "new reply",
                Some("codex-thread"),
            ),
        );
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(runner(bin)),
            request: contract_request(work_dir, true),
            timeout: Duration::from_secs(5),
        }
    }

    fn contract_resumed_session() -> ContractCase {
        let work_dir = temp_dir("codex-contract-resume");
        let cli = FakeCli::new(
            "codex",
            &codex_success_script(
                &temp_path("codex-contract-resume-args"),
                "resumed reply",
                None,
            ),
        );
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(runner(bin)),
            request: contract_request(work_dir, false),
            timeout: Duration::from_secs(5),
        }
    }

    fn contract_failed_run() -> ContractCase {
        let work_dir = temp_dir("codex-contract-fail");
        let cli = FakeCli::new("codex", "#!/bin/sh\nprintf '%s\\n' 'failed' >&2\nexit 1\n");
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(runner(bin)),
            request: contract_request(work_dir, true),
            timeout: Duration::from_secs(5),
        }
    }

    fn contract_timeout_run() -> ContractCase {
        let work_dir = temp_dir("codex-contract-timeout");
        let cli = FakeCli::new("codex", "#!/bin/sh\nsleep 2\n");
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(runner(bin)),
            request: contract_request(work_dir, true),
            timeout: Duration::from_millis(10),
        }
    }

    fn contract_request(work_dir: std::path::PathBuf, is_new: bool) -> ContractRequest {
        ContractRequest {
            session_id: if is_new {
                String::new()
            } else {
                "existing-thread".to_string()
            },
            is_new,
            work_dir,
            system_append: String::new(),
            prompt: "hello".to_string(),
        }
    }
}
