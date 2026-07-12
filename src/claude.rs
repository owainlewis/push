//! Runs the Claude Code CLI headlessly for a single message.

use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::agent::{Request, RunError, RunOutput};

/// Runner invokes the `claude` binary in print mode.
pub struct Runner {
    pub bin: String,
    pub permission_mode: String,
    pub tools: Option<Vec<String>>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
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
        let out = match tokio::time::timeout(timeout, self.output_with_retry(req)).await {
            Err(_) => return Err(RunError::Timeout),
            Ok(Err(e)) => return Err(RunError::Failed(format!("spawn claude: {e}"))),
            Ok(Ok(o)) => o,
        };

        self.parse_output(out)
    }

    async fn output_with_retry(&self, req: Request<'_>) -> std::io::Result<std::process::Output> {
        let mut attempts = 0;
        loop {
            match self.command(&req).output().await {
                Err(e) if e.raw_os_error() == Some(26) && attempts < 3 => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                result => return result,
            }
        }
    }

    fn command(&self, req: &Request<'_>) -> Command {
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
        if !req.instructions.trim().is_empty() {
            cmd.arg("--append-system-prompt").arg(req.instructions);
        }
        if let Some(tools) = &self.tools {
            cmd.arg("--tools");
            cmd.args(tools);
        }
        for tool in &self.allowed_tools {
            cmd.arg("--allowed-tools").arg(tool);
        }
        for tool in &self.disallowed_tools {
            cmd.arg("--disallowed-tools").arg(tool);
        }
        cmd.current_dir(req.work_dir);
        cmd.kill_on_drop(true);
        cmd
    }

    fn parse_output(&self, out: std::process::Output) -> Result<RunOutput, RunError> {
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
                session_id: non_empty_session_id(&r.session_id).map(str::to_string),
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

fn non_empty_session_id(id: &str) -> Option<&str> {
    let trimmed = id.trim();
    (!trimmed.is_empty()).then_some(trimmed)
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
        let runner = Runner {
            bin: cli.bin(),
            permission_mode: "bypassPermissions".to_string(),
            tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
            allowed_tools: vec!["Read".to_string(), "Bash(git status:*)".to_string()],
            disallowed_tools: vec!["Edit".to_string()],
        };

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
        assert_arg_pair(&args, "--permission-mode", "bypassPermissions");
        assert_arg_pair(&args, "--append-system-prompt", "assistant identity");
        assert_arg_pair(&args, "-p", "hello");
        assert_arg_pair(&args, "--tools", "Read");
        assert_arg_pair(&args, "--allowed-tools", "Read");
        assert_arg_pair(&args, "--disallowed-tools", "Edit");
        assert!(args.contains(&"Grep".to_string()));
        assert!(args.contains(&"Bash(git status:*)".to_string()));
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
        let runner = Runner {
            bin: cli.bin(),
            permission_mode: "default".to_string(),
            tools: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
        };

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
        assert_arg_pair(&args, "-p", "continue");
    }

    #[tokio::test]
    async fn reports_cli_json_error() {
        let work_dir = temp_dir("claude-error-work");
        let cli = FakeCli::new(
            "claude",
            "#!/bin/sh\nprintf '%s\\n' '{\"is_error\":true,\"result\":\"api down\"}'\nexit 1\n",
        );
        let runner = Runner {
            bin: cli.bin(),
            permission_mode: "default".to_string(),
            tools: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
        };

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
        let runner = Runner {
            bin: cli.bin(),
            permission_mode: "default".to_string(),
            tools: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
        };

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
        }
    }

    fn assert_timeout(err: RunError) {
        match err {
            RunError::Timeout => {}
            RunError::Failed(msg) => panic!("expected timeout, got failed: {msg}"),
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
            runner: Box::new(Runner {
                bin,
                permission_mode: "default".to_string(),
                tools: None,
                allowed_tools: Vec::new(),
                disallowed_tools: Vec::new(),
            }),
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
            runner: Box::new(Runner {
                bin,
                permission_mode: "default".to_string(),
                tools: None,
                allowed_tools: Vec::new(),
                disallowed_tools: Vec::new(),
            }),
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
            runner: Box::new(Runner {
                bin,
                permission_mode: "default".to_string(),
                tools: None,
                allowed_tools: Vec::new(),
                disallowed_tools: Vec::new(),
            }),
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
            runner: Box::new(Runner {
                bin,
                permission_mode: "default".to_string(),
                tools: None,
                allowed_tools: Vec::new(),
                disallowed_tools: Vec::new(),
            }),
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
