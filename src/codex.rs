//! Runs the Codex CLI headlessly for a single message.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use uuid::Uuid;

use crate::agent::{final_reply, Request, RunError, RunOutput};

/// Runner invokes `codex exec` in non-interactive mode.
pub struct Runner {
    pub bin: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunMode {
    Configured,
    Unattended,
    Evaluator,
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

struct OutputFile {
    path: PathBuf,
}

impl OutputFile {
    fn create() -> std::io::Result<Self> {
        let path =
            std::env::temp_dir().join(format!("push-codex-last-message-{}.txt", Uuid::new_v4()));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok(Self { path })
    }
}

impl Drop for OutputFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Runner {
    /// Executes one turn and returns Codex's final reply plus the Codex session id.
    pub async fn run(&self, req: Request<'_>, timeout: Duration) -> Result<RunOutput, RunError> {
        self.run_with_mode(req, timeout, RunMode::Configured).await
    }

    pub async fn run_unattended(
        &self,
        req: Request<'_>,
        timeout: Duration,
    ) -> Result<RunOutput, RunError> {
        self.run_with_mode(req, timeout, RunMode::Unattended).await
    }

    pub async fn run_evaluator(
        &self,
        req: Request<'_>,
        timeout: Duration,
    ) -> Result<RunOutput, RunError> {
        self.run_with_mode(req, timeout, RunMode::Evaluator).await
    }

    async fn run_with_mode(
        &self,
        req: Request<'_>,
        timeout: Duration,
        mode: RunMode,
    ) -> Result<RunOutput, RunError> {
        let output_file = OutputFile::create()
            .map_err(|error| RunError::Failed(format!("prepare Codex output: {error}")))?;
        let out_path = output_file.path.as_path();
        let attempt = crate::agent::output_with_retry(|| {
            let mut cmd = self.command(&req, out_path, mode);
            async move { cmd.output().await }
        });
        let out = match tokio::time::timeout(timeout, attempt).await {
            Err(_) => return Err(RunError::Timeout),
            Ok(Err(e)) => return Err(RunError::Failed(format!("spawn codex: {e}"))),
            Ok(Ok(o)) => o,
        };

        let stdout = String::from_utf8_lossy(&out.stdout);
        let session_id = session_id_from_jsonl(&stdout);
        let reply = std::fs::read_to_string(out_path)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| last_agent_message_from_jsonl(&stdout))
            .unwrap_or_default();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let message = if stderr.is_empty() {
                "codex exited without a final reply".to_string()
            } else {
                stderr
            };
            if !req.is_new && missing_resume_error(&message) {
                return Err(RunError::SessionMissing(message));
            }
            return Err(RunError::Failed(message));
        }
        if req.is_new && session_id.is_none() {
            return Err(RunError::Failed(
                "codex did not report a session id".to_string(),
            ));
        }
        Ok(RunOutput {
            reply: final_reply("codex", &reply)?,
            session_id,
        })
    }

    fn command(&self, req: &Request<'_>, out_path: &Path, mode: RunMode) -> Command {
        let mut cmd = Command::new(&self.bin);
        if !req.instructions.trim().is_empty() {
            cmd.arg("-c")
                .arg(developer_instructions(req.instructions.trim()));
        }
        if mode == RunMode::Evaluator {
            cmd.arg("-c")
                .arg("mcp_servers={}")
                .arg("-c")
                .arg("project_doc_max_bytes=0")
                .arg("-c")
                .arg("web_search=\"disabled\"");
            for feature in [
                "shell_tool",
                "unified_exec",
                "browser_use",
                "browser_use_external",
                "browser_use_full_cdp_access",
                "computer_use",
                "apps",
                "in_app_browser",
                "image_generation",
                "multi_agent",
                "code_mode",
                "code_mode_host",
                "standalone_web_search",
                "hooks",
                "plugins",
                "plugin_sharing",
                "workspace_dependencies",
                "goals",
                "request_permissions_tool",
                "auth_elicitation",
                "tool_call_mcp_elicitation",
            ] {
                cmd.arg("--disable").arg(feature);
            }
        } else if mode == RunMode::Unattended {
            cmd.arg("--sandbox")
                .arg("danger-full-access")
                .arg("--ask-for-approval")
                .arg("never");
        }
        if req.is_new {
            cmd.arg("exec")
                .arg("--json")
                .arg("--skip-git-repo-check")
                .arg("-C")
                .arg(req.work_dir)
                .arg("-o")
                .arg(out_path);
            if mode == RunMode::Evaluator {
                cmd.arg("--sandbox")
                    .arg("read-only")
                    .arg("--ephemeral")
                    .arg("--ignore-user-config");
            }
            cmd.arg(req.prompt);
        } else {
            cmd.arg("exec");
            cmd.arg("resume")
                .arg("--json")
                .arg("--skip-git-repo-check")
                .arg("-o")
                .arg(out_path);
            cmd.arg(req.session_id).arg(req.prompt);
        }
        cmd.current_dir(req.work_dir);
        cmd.kill_on_drop(true);
        cmd
    }
}

fn missing_resume_error(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("no rollout found for thread id")
}

fn developer_instructions(instructions: &str) -> String {
    format!(
        "developer_instructions={}",
        toml::Value::String(instructions.to_string())
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
    async fn unattended_new_session_bypasses_permissions() {
        let args_path = temp_path("codex-args");
        let work_dir = temp_dir("codex-work");
        let script = codex_success_script(&args_path, "codex reply", Some("codex-thread"));
        let cli = FakeCli::new("codex", &script);
        let runner = runner(cli.bin());

        let out = runner
            .run_unattended(
                Request {
                    session_id: "",
                    is_new: true,
                    work_dir: work_dir.to_str().unwrap(),
                    instructions: "assistant identity",
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
        assert_arg_pair(&args, "--sandbox", "danger-full-access");
        assert_arg_present(&args, "exec");
        assert_arg_present(&args, "--json");
        assert_arg_pair(&args, "-C", work_dir.to_str().unwrap());
        assert!(!args.contains(&"--add-dir".to_string()));
        assert_arg_pair(&args, "-c", &developer_instructions("assistant identity"));
        assert_eq!(args.last().unwrap(), "hello");
    }

    #[tokio::test]
    async fn unattended_resumed_session_bypasses_permissions() {
        let args_path = temp_path("codex-resume-args");
        let work_dir = temp_dir("codex-resume-work");
        let script = codex_success_script(&args_path, "resumed reply", None);
        let cli = FakeCli::new("codex", &script);
        let runner = runner(cli.bin());

        let out = runner
            .run_unattended(
                Request {
                    session_id: "existing-thread",
                    is_new: false,
                    work_dir: work_dir.to_str().unwrap(),
                    instructions: "assistant identity",
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
        assert_arg_pair(&args, "--ask-for-approval", "never");
        assert_arg_pair(&args, "--sandbox", "danger-full-access");
        assert!(!args.contains(&"--add-dir".to_string()));
        assert_arg_present(&args, "existing-thread");
        assert_arg_pair(&args, "-c", &developer_instructions("assistant identity"));
        assert_eq!(args.last().unwrap(), "continue");
        assert!(!args.contains(&"-C".to_string()));
    }

    #[tokio::test]
    async fn rejects_successful_whitespace_only_reply() {
        let args_path = temp_path("codex-empty-reply-args");
        let work_dir = temp_dir("codex-empty-reply-work");
        let script = codex_success_script(&args_path, " \t\n ", Some("codex-thread"));
        let cli = FakeCli::new("codex", &script);
        let runner = runner(cli.bin());

        let error = runner
            .run(request(work_dir.to_str().unwrap()), Duration::from_secs(5))
            .await
            .unwrap_err();

        assert_failed(error, "codex exited without a final reply");
    }

    #[tokio::test]
    async fn configured_run_preserves_backend_permission_settings() {
        let args_path = temp_path("codex-configured-args");
        let work_dir = temp_dir("codex-configured-work");
        let cli = FakeCli::new(
            "codex",
            &codex_success_script(&args_path, "reply", Some("codex-thread")),
        );
        let runner = runner(cli.bin());

        runner
            .run(request(work_dir.to_str().unwrap()), Duration::from_secs(5))
            .await
            .unwrap();

        let args = read_args(&args_path);
        assert!(!args.contains(&"--ask-for-approval".to_string()));
        assert!(!args.contains(&"--sandbox".to_string()));
    }

    #[tokio::test]
    async fn resumed_lookup_failure_is_typed_before_gateway_retry() {
        let work_dir = temp_dir("codex-missing-resume-work");
        let cli = FakeCli::new(
            "codex",
            "#!/bin/sh\nprintf '%s\n' 'No rollout found for thread id missing' >&2\nexit 1\n",
        );
        let runner = runner(cli.bin());

        let error = runner
            .run(
                Request {
                    session_id: "missing",
                    is_new: false,
                    work_dir: work_dir.to_str().unwrap(),
                    instructions: "",
                    prompt: "continue",
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, RunError::SessionMissing(_)));
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
        let cli = FakeCli::new(
            "codex",
            r#"#!/bin/sh
out=''
while [ "$#" -gt 0 ]; do
  if [ "$1" = '-o' ]; then
    shift
    out="$1"
  fi
  shift
done
printf '%s\n' 'partial reply' > "$out"
sleep 2
"#,
        );
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
        assert!(std::fs::read_dir(&work_dir).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn evaluator_disables_tools_and_user_integrations() {
        let work_dir = temp_dir("codex-evaluator-work");
        let args_path = temp_path("codex-evaluator-args");
        let cli = FakeCli::new(
            "codex",
            &codex_success_script(&args_path, "VERDICT: PASS", Some("eval-thread")),
        );
        let runner = runner(cli.bin());

        runner
            .run_evaluator(request(work_dir.to_str().unwrap()), Duration::from_secs(5))
            .await
            .unwrap();

        let args = read_args(&args_path);
        assert_arg_pair(&args, "--sandbox", "read-only");
        assert!(!args.contains(&"--ask-for-approval".to_string()));
        assert!(args.iter().any(|arg| arg == "--ephemeral"));
        assert!(args.iter().any(|arg| arg == "--ignore-user-config"));
        assert!(args.iter().any(|arg| arg == "mcp_servers={}"));
        assert!(args.iter().any(|arg| arg == "project_doc_max_bytes=0"));
        assert!(args.iter().any(|arg| arg == "web_search=\"disabled\""));
        for feature in ["shell_tool", "unified_exec"] {
            assert!(args.windows(2).any(|pair| pair == ["--disable", feature]));
        }
    }

    fn runner(bin: String) -> Runner {
        Runner { bin }
    }

    fn request(work_dir: &str) -> Request<'_> {
        Request {
            session_id: "",
            is_new: true,
            work_dir,
            instructions: "",
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
            RunError::SessionMissing(msg) => panic!("unexpected missing session: {msg}"),
        }
    }

    fn assert_timeout(err: RunError) {
        match err {
            RunError::Timeout => {}
            RunError::Failed(msg) => panic!("expected timeout, got failed: {msg}"),
            RunError::SessionMissing(msg) => panic!("unexpected missing session: {msg}"),
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
            instructions: String::new(),
            prompt: "hello".to_string(),
        }
    }

    #[test]
    fn classifies_only_codex_resume_lookup_errors_as_missing_sessions() {
        assert!(missing_resume_error("No rollout found for thread id 123"));
        assert!(!missing_resume_error("tool thread not found"));
    }
}
