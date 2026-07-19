//! Runs the Claude Code CLI headlessly for a single message.

use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::agent::{final_reply, Request, RunError, RunOutput};
use crate::util::non_empty_session_id;

/// Runner invokes the `claude` binary in print mode.
pub struct Runner {
    pub bin: String,
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
        self.run_with_mode(req, timeout, false).await
    }

    pub async fn run_evaluator(
        &self,
        req: Request<'_>,
        timeout: Duration,
    ) -> Result<RunOutput, RunError> {
        self.run_with_mode(req, timeout, true).await
    }

    async fn run_with_mode(
        &self,
        req: Request<'_>,
        timeout: Duration,
        evaluator: bool,
    ) -> Result<RunOutput, RunError> {
        let is_resume = !req.is_new;
        let attempt = crate::agent::output_with_retry(|| {
            let mut cmd = self.command(&req, evaluator);
            async move { cmd.output().await }
        });
        let out = match tokio::time::timeout(timeout, attempt).await {
            Err(_) => return Err(RunError::Timeout),
            Ok(Err(e)) => return Err(RunError::Failed(format!("spawn claude: {e}"))),
            Ok(Ok(o)) => o,
        };

        self.parse_output(out, is_resume)
    }

    fn command(&self, req: &Request<'_>, evaluator: bool) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("-p")
            .arg(req.prompt)
            .arg("--output-format")
            .arg("json");
        if evaluator {
            cmd.arg("--safe-mode")
                .arg("--tools")
                .arg("")
                .arg("--strict-mcp-config")
                .arg("--mcp-config")
                .arg("{}")
                .arg("--no-chrome")
                .arg("--no-session-persistence");
        }
        if req.is_new {
            cmd.arg("--session-id").arg(req.session_id);
        } else {
            cmd.arg("--resume").arg(req.session_id);
        }
        if !req.instructions.trim().is_empty() {
            cmd.arg("--append-system-prompt").arg(req.instructions);
        }
        cmd.current_dir(req.work_dir);
        cmd.kill_on_drop(true);
        cmd
    }

    fn parse_output(
        &self,
        out: std::process::Output,
        is_resume: bool,
    ) -> Result<RunOutput, RunError> {
        // claude prints its JSON envelope to stdout even when it exits non-zero
        // (e.g. an API error), so parse stdout regardless of exit status.
        match serde_json::from_slice::<CliResult>(&out.stdout) {
            Ok(r) if r.is_error => {
                let msg = if r.result.is_empty() {
                    r.subtype
                } else {
                    r.result
                };
                if is_resume && missing_resume_error(&msg) {
                    Err(RunError::SessionMissing(msg))
                } else {
                    Err(RunError::Failed(msg))
                }
            }
            Ok(r) => Ok(RunOutput {
                reply: final_reply("claude", &r.result)?,
                session_id: non_empty_session_id(&r.session_id).map(str::to_string),
            }),
            Err(_) => {
                if out.status.success() {
                    Ok(RunOutput {
                        reply: final_reply("claude", &String::from_utf8_lossy(&out.stdout))?,
                        session_id: None,
                    })
                } else {
                    let message = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    if is_resume && missing_resume_error(&message) {
                        Err(RunError::SessionMissing(message))
                    } else {
                        Err(RunError::Failed(message))
                    }
                }
            }
        }
    }
}

fn missing_resume_error(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("no conversation found with session id")
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
    fn ignores_empty_session_id() {
        assert_eq!(non_empty_session_id(""), None);
        assert_eq!(non_empty_session_id(" \t\n "), None);
    }

    #[test]
    fn keeps_valid_session_id() {
        assert_eq!(
            non_empty_session_id(" claude-session "),
            Some("claude-session")
        );
    }

    #[test]
    fn classifies_only_claude_resume_lookup_errors_as_missing_sessions() {
        assert!(missing_resume_error(
            "No conversation found with session ID 123"
        ));
        assert!(!missing_resume_error("tool session not found"));
    }

    #[tokio::test]
    async fn satisfies_runner_contract() {
        assert_runner_contract(RunnerContract {
            name: "Claude",
            new_session: contract_new_session,
            resumed_session: contract_resumed_session,
            failed_run: contract_failed_run,
            timeout_run: contract_timeout_run,
        })
        .await;
    }

    #[tokio::test]
    async fn runs_new_session_with_push_owned_session_id() {
        let args_path = temp_path("claude-args");
        let work_dir = temp_dir("claude-work");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nprintf '%s\\n' '{{\"result\":\" hello \",\"session_id\":\"claude-returned\"}}'\n",
            sh_arg(&args_path)
        );
        let cli = FakeCli::new("claude", &script);
        let runner = Runner { bin: cli.bin() };

        let out = runner
            .run(
                Request {
                    session_id: "push-session",
                    is_new: true,
                    work_dir: work_dir.to_str().unwrap(),
                    instructions: "assistant identity",
                    prompt: "hello",
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(out.reply, "hello");
        assert_eq!(out.session_id, Some("claude-returned".to_string()));
        let args = read_args(&args_path);
        assert_arg_pair(&args, "--session-id", "push-session");
        assert_arg_pair(&args, "--append-system-prompt", "assistant identity");
        assert_arg_pair(&args, "-p", "hello");
        for flag in [
            "--permission-mode",
            "--tools",
            "--allowed-tools",
            "--disallowed-tools",
        ] {
            assert!(
                !args.contains(&flag.to_string()),
                "unexpected {flag} in {args:?}"
            );
        }
        assert!(!args.contains(&"--resume".to_string()));
    }

    #[tokio::test]
    async fn runs_resumed_session_with_resume_flag() {
        let args_path = temp_path("claude-resume-args");
        let work_dir = temp_dir("claude-resume-work");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nprintf '%s\\n' '{{\"result\":\"resumed\",\"session_id\":\"claude-returned\"}}'\n",
            sh_arg(&args_path)
        );
        let cli = FakeCli::new("claude", &script);
        let runner = Runner { bin: cli.bin() };

        let out = runner
            .run(
                Request {
                    session_id: "existing-session",
                    is_new: false,
                    work_dir: work_dir.to_str().unwrap(),
                    instructions: "assistant identity",
                    prompt: "continue",
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(out.reply, "resumed");
        let args = read_args(&args_path);
        assert_arg_pair(&args, "--resume", "existing-session");
        assert!(!args.contains(&"--session-id".to_string()));
        assert_arg_pair(&args, "--append-system-prompt", "assistant identity");
        assert!(!args.contains(&"--add-dir".to_string()));
        assert_arg_pair(&args, "-p", "continue");
    }

    #[tokio::test]
    async fn rejects_successful_empty_replies() {
        for stdout in [
            r#"{"result":" \t\n ","session_id":"claude-session"}"#,
            " \t ",
        ] {
            let work_dir = temp_dir("claude-empty-reply-work");
            let cli = FakeCli::new(
                "claude",
                &format!("#!/bin/sh\nprintf '%s\\n' {}\n", sh_arg(stdout.as_ref())),
            );
            let runner = Runner { bin: cli.bin() };

            let error = runner
                .run(request(work_dir.to_str().unwrap()), Duration::from_secs(5))
                .await
                .unwrap_err();

            assert_failed(error, "claude exited without a final reply");
        }
    }

    #[tokio::test]
    async fn resumed_lookup_failure_is_typed_before_gateway_retry() {
        let work_dir = temp_dir("claude-missing-resume-work");
        let cli = FakeCli::new(
            "claude",
            "#!/bin/sh\nprintf '%s\n' '{\"is_error\":true,\"result\":\"No conversation found with session ID missing\"}'\nexit 1\n",
        );
        let runner = Runner { bin: cli.bin() };

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
    async fn reports_cli_json_error() {
        let work_dir = temp_dir("claude-error-work");
        let cli = FakeCli::new(
            "claude",
            "#!/bin/sh\nprintf '%s\\n' '{\"is_error\":true,\"result\":\"api down\"}'\nexit 1\n",
        );
        let runner = Runner { bin: cli.bin() };

        let err = match runner
            .run(request(work_dir.to_str().unwrap()), Duration::from_secs(5))
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("expected Claude run to fail"),
        };

        assert_failed(err, "api down");
    }

    #[tokio::test]
    async fn reports_timeout() {
        let work_dir = temp_dir("claude-timeout-work");
        let cli = FakeCli::new("claude", "#!/bin/sh\nsleep 2\n");
        let runner = Runner { bin: cli.bin() };

        let err = match runner
            .run(
                request(work_dir.to_str().unwrap()),
                Duration::from_millis(10),
            )
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("expected Claude run to time out"),
        };

        assert_timeout(err);
    }

    #[tokio::test]
    async fn evaluator_disables_tools_and_mcp() {
        let work_dir = temp_dir("claude-evaluator-work");
        let args_path = temp_path("claude-evaluator-args");
        let cli = FakeCli::new(
            "claude",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nprintf '%s\\n' '{{\"result\":\"VERDICT: PASS\",\"session_id\":\"eval-session\"}}'\n",
                sh_arg(&args_path)
            ),
        );
        let runner = Runner { bin: cli.bin() };

        runner
            .run_evaluator(request(work_dir.to_str().unwrap()), Duration::from_secs(5))
            .await
            .unwrap();

        let args = read_args(&args_path);
        assert_arg_pair(&args, "--tools", "");
        assert_arg_pair(&args, "--mcp-config", "{}");
        assert!(args.iter().any(|arg| arg == "--strict-mcp-config"));
        assert!(args.iter().any(|arg| arg == "--safe-mode"));
    }

    fn request(work_dir: &str) -> Request<'_> {
        Request {
            session_id: "session",
            is_new: true,
            work_dir,
            instructions: "",
            prompt: "hello",
        }
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
        let work_dir = temp_dir("claude-contract-new");
        let cli = FakeCli::new(
            "claude",
            "#!/bin/sh\nprintf '%s\\n' '{\"result\":\"new reply\",\"session_id\":\"claude-session\"}'\n",
        );
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(Runner { bin }),
            request: contract_request(work_dir, true),
            timeout: Duration::from_secs(5),
        }
    }

    fn contract_resumed_session() -> ContractCase {
        let work_dir = temp_dir("claude-contract-resume");
        let cli = FakeCli::new(
            "claude",
            "#!/bin/sh\nprintf '%s\\n' '{\"result\":\"resumed reply\",\"session_id\":\"claude-session\"}'\n",
        );
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(Runner { bin }),
            request: contract_request(work_dir, false),
            timeout: Duration::from_secs(5),
        }
    }

    fn contract_failed_run() -> ContractCase {
        let work_dir = temp_dir("claude-contract-fail");
        let cli = FakeCli::new(
            "claude",
            "#!/bin/sh\nprintf '%s\\n' '{\"is_error\":true,\"result\":\"failed\"}'\nexit 1\n",
        );
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(Runner { bin }),
            request: contract_request(work_dir, true),
            timeout: Duration::from_secs(5),
        }
    }

    fn contract_timeout_run() -> ContractCase {
        let work_dir = temp_dir("claude-contract-timeout");
        let cli = FakeCli::new("claude", "#!/bin/sh\nsleep 2\n");
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(Runner { bin }),
            request: contract_request(work_dir, true),
            timeout: Duration::from_millis(10),
        }
    }

    fn contract_request(work_dir: std::path::PathBuf, is_new: bool) -> ContractRequest {
        ContractRequest {
            session_id: "contract-session".to_string(),
            is_new,
            work_dir,
            instructions: String::new(),
            prompt: "hello".to_string(),
        }
    }
}
