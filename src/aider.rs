//! Runs the aider.chat CLI headlessly for a single message.
//!
//! Unlike Claude, Codex, and Pi, aider has no server-side session identifier;
//! conversation context lives in a per-session Markdown chat history file. Push
//! therefore owns the session id (a UUID generated up front, like Claude) and
//! maps it to a history file under `sessions_dir`, restoring that file to
//! resume a conversation. A missing history file on resume is reported as
//! `SessionMissing` so the gateway can rebuild context from its own history.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::agent::{Request, RunError, RunOutput};

/// Runner invokes the `aider` binary in non-interactive message mode.
pub struct Runner {
    pub bin: String,
    pub sessions_dir: String,
}

impl Runner {
    /// Executes one turn and returns aider's reply text, or a RunError.
    pub async fn run(&self, req: Request<'_>, timeout: Duration) -> Result<RunOutput, RunError> {
        let history = self.session_file(req.session_id, "chat.history.md")?;
        if !req.is_new && !history.exists() {
            return Err(RunError::SessionMissing(format!(
                "aider chat history {} is missing; Push will rebuild it from conversation history",
                history.display()
            )));
        }
        if let Some(parent) = history.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| RunError::Failed(format!("create aider sessions dir: {e}")))?;
        }

        // Aider has no system-prompt flag, so persistent instructions are
        // written to a conventions file and attached read-only, matching how
        // the other backends fold SOUL.md in as separate context.
        let conventions = if req.instructions.trim().is_empty() {
            None
        } else {
            let path = self.session_file(req.session_id, "conventions.md")?;
            std::fs::write(&path, req.instructions.trim())
                .map_err(|e| RunError::Failed(format!("write aider instructions: {e}")))?;
            Some(path)
        };

        let attempt = crate::agent::output_with_retry(|| {
            let mut cmd = self.command(&req, &history, conventions.as_deref());
            async move { cmd.output().await }
        });
        let out = match tokio::time::timeout(timeout, attempt).await {
            Err(_) => return Err(RunError::Timeout),
            Ok(Err(e)) => return Err(RunError::Failed(format!("spawn aider: {e}"))),
            Ok(Ok(o)) => o,
        };

        if !out.status.success() {
            return Err(RunError::Failed(exit_diagnostic(out.status.code())));
        }

        let reply = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if reply.is_empty() {
            return Err(RunError::Failed(
                "aider exited without a reply on stdout".to_string(),
            ));
        }

        Ok(RunOutput {
            reply,
            session_id: req.is_new.then(|| req.session_id.to_string()),
        })
    }

    fn command(&self, req: &Request<'_>, history: &Path, conventions: Option<&Path>) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--message")
            .arg(req.prompt)
            .arg("--yes-always")
            .arg("--no-pretty")
            .arg("--no-stream")
            .arg("--chat-history-file")
            .arg(history);
        if !req.is_new {
            cmd.arg("--restore-chat-history");
        }
        if let Some(conventions) = conventions {
            cmd.arg("--read").arg(conventions);
        }
        cmd.current_dir(req.work_dir);
        cmd.kill_on_drop(true);
        cmd
    }

    /// Resolves a Push-owned per-session file, rejecting ids that could escape
    /// `sessions_dir`.
    fn session_file(&self, session_id: &str, suffix: &str) -> Result<PathBuf, RunError> {
        let id = session_id.trim();
        if id.is_empty()
            || id == "."
            || id == ".."
            || id.contains('/')
            || id.contains('\\')
            || id.contains('\0')
        {
            return Err(RunError::Failed(format!(
                "invalid aider session id {session_id:?}"
            )));
        }
        Ok(Path::new(&self.sessions_dir).join(format!("aider-{id}.{suffix}")))
    }
}

fn exit_diagnostic(status: Option<i32>) -> String {
    match status {
        Some(code) => format!(
            "aider exited with status {code}; run aider directly as the service user to check model provider and authentication settings"
        ),
        None => "aider was terminated; run aider directly as the service user to check model provider and authentication settings"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[tokio::test]
    async fn satisfies_runner_contract() {
        assert_runner_contract(RunnerContract {
            name: "Aider",
            new_session: contract_new_session,
            resumed_session: contract_resumed_session,
            failed_run: contract_failed_run,
            timeout_run: contract_timeout_run,
        })
        .await;
    }

    #[tokio::test]
    async fn new_session_writes_history_and_instructions_without_restore() {
        let args_path = temp_path("aider-new-args");
        let work_dir = temp_dir("aider-new-work");
        let sessions = temp_dir("aider-new-sessions");
        let cli = FakeCli::new("aider", &success_script(&args_path, "hello"));
        let runner = Runner {
            bin: cli.bin(),
            sessions_dir: sessions.to_string_lossy().to_string(),
        };

        let out = runner
            .run(
                Request {
                    session_id: "abc123",
                    is_new: true,
                    work_dir: work_dir.to_str().unwrap(),
                    instructions: "SOUL instructions",
                    prompt: "hello",
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(out.reply, "hello");
        assert_eq!(out.session_id.as_deref(), Some("abc123"));
        let history = sessions.join("aider-abc123.chat.history.md");
        let conventions = sessions.join("aider-abc123.conventions.md");
        let args = read_args(&args_path);
        assert_arg_pair(&args, "--message", "hello");
        assert_arg_pair(&args, "--chat-history-file", history.to_str().unwrap());
        assert_arg_pair(&args, "--read", conventions.to_str().unwrap());
        assert!(args.iter().any(|arg| arg == "--yes-always"));
        assert!(!args.contains(&"--restore-chat-history".to_string()));
        assert_eq!(
            std::fs::read_to_string(&conventions).unwrap(),
            "SOUL instructions"
        );
    }

    #[tokio::test]
    async fn resume_restores_existing_history_and_keeps_the_session_id() {
        let args_path = temp_path("aider-resume-args");
        let work_dir = temp_dir("aider-resume-work");
        let sessions = temp_dir("aider-resume-sessions");
        let history = sessions.join("aider-existing.chat.history.md");
        std::fs::write(&history, "#### earlier turn\n").unwrap();
        let cli = FakeCli::new("aider", &success_script(&args_path, "again"));
        let runner = Runner {
            bin: cli.bin(),
            sessions_dir: sessions.to_string_lossy().to_string(),
        };

        let out = runner
            .run(
                Request {
                    session_id: "existing",
                    is_new: false,
                    work_dir: work_dir.to_str().unwrap(),
                    instructions: "",
                    prompt: "continue",
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(out.reply, "again");
        assert_eq!(out.session_id, None);
        let args = read_args(&args_path);
        assert_arg_pair(&args, "--chat-history-file", history.to_str().unwrap());
        assert!(args.iter().any(|arg| arg == "--restore-chat-history"));
        assert!(!args.contains(&"--read".to_string()));
    }

    #[tokio::test]
    async fn missing_history_on_resume_is_typed_for_rehydration() {
        let work_dir = temp_dir("aider-missing-work");
        let sessions = temp_dir("aider-missing-sessions");
        let cli = FakeCli::new("aider", &success_script(&temp_path("aider-unused"), "x"));
        let runner = Runner {
            bin: cli.bin(),
            sessions_dir: sessions.to_string_lossy().to_string(),
        };

        let error = runner
            .run(
                Request {
                    session_id: "gone",
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
    async fn rejects_session_ids_that_escape_the_sessions_dir() {
        let work_dir = temp_dir("aider-escape-work");
        let sessions = temp_dir("aider-escape-sessions");
        let cli = FakeCli::new("aider", &success_script(&temp_path("aider-unused"), "x"));
        let runner = Runner {
            bin: cli.bin(),
            sessions_dir: sessions.to_string_lossy().to_string(),
        };

        for id in ["../escape", "a/b", ""] {
            let error = runner
                .run(
                    Request {
                        session_id: id,
                        is_new: true,
                        work_dir: work_dir.to_str().unwrap(),
                        instructions: "",
                        prompt: "hello",
                    },
                    Duration::from_secs(5),
                )
                .await
                .unwrap_err();
            assert!(matches!(error, RunError::Failed(_)));
        }
    }

    #[tokio::test]
    async fn reports_non_zero_exit_without_forwarding_stderr_secrets() {
        let work_dir = temp_dir("aider-failed-work");
        let sessions = temp_dir("aider-failed-sessions");
        let secret = "provider-token-abcdef";
        let cli = FakeCli::new(
            "aider",
            &format!("#!/bin/sh\nprintf '%s\\n' 'boom {secret}' >&2\nexit 2\n"),
        );
        let runner = Runner {
            bin: cli.bin(),
            sessions_dir: sessions.to_string_lossy().to_string(),
        };

        let error = runner
            .run(
                Request {
                    session_id: "abc123",
                    is_new: true,
                    work_dir: work_dir.to_str().unwrap(),
                    instructions: "",
                    prompt: "hello",
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();

        let message = failed_message(error);
        assert!(message.contains("status 2"));
        assert!(!message.contains(secret));
    }

    #[tokio::test]
    async fn rejects_empty_reply() {
        let work_dir = temp_dir("aider-empty-work");
        let sessions = temp_dir("aider-empty-sessions");
        let cli = FakeCli::new("aider", "#!/bin/sh\nprintf '   \\n'\n");
        let runner = Runner {
            bin: cli.bin(),
            sessions_dir: sessions.to_string_lossy().to_string(),
        };

        let error = runner
            .run(
                Request {
                    session_id: "abc123",
                    is_new: true,
                    work_dir: work_dir.to_str().unwrap(),
                    instructions: "",
                    prompt: "hello",
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();

        assert!(failed_message(error).contains("without a reply"));
    }

    fn success_script(args_path: &std::path::Path, reply: &str) -> String {
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nprintf '%s\\n' '{reply}'\n",
            sh_arg(args_path)
        )
    }

    fn read_args(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn assert_arg_pair(args: &[String], flag: &str, value: &str) {
        let index = args
            .iter()
            .position(|arg| arg == flag)
            .unwrap_or_else(|| panic!("missing flag {flag} in {args:?}"));
        assert_eq!(args.get(index + 1).map(String::as_str), Some(value));
    }

    fn failed_message(error: RunError) -> String {
        match error {
            RunError::Failed(message) => message,
            other => panic!("expected failed error, got {other:?}"),
        }
    }

    fn contract_new_session() -> ContractCase {
        let sessions = temp_dir("aider-contract-new-sessions");
        let work_dir = temp_dir("aider-contract-new-work");
        let cli = FakeCli::new("aider", "#!/bin/sh\nprintf 'new reply\\n'\n");
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(Runner {
                bin,
                sessions_dir: sessions.to_string_lossy().to_string(),
            }),
            request: contract_request(work_dir, true),
            timeout: Duration::from_secs(5),
        }
    }

    fn contract_resumed_session() -> ContractCase {
        let sessions = temp_dir("aider-contract-resume-sessions");
        std::fs::write(
            sessions.join("aider-contract-session.chat.history.md"),
            "#### earlier\n",
        )
        .unwrap();
        let work_dir = temp_dir("aider-contract-resume-work");
        let cli = FakeCli::new("aider", "#!/bin/sh\nprintf 'resumed reply\\n'\n");
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(Runner {
                bin,
                sessions_dir: sessions.to_string_lossy().to_string(),
            }),
            request: contract_request(work_dir, false),
            timeout: Duration::from_secs(5),
        }
    }

    fn contract_failed_run() -> ContractCase {
        let sessions = temp_dir("aider-contract-fail-sessions");
        let work_dir = temp_dir("aider-contract-fail-work");
        let cli = FakeCli::new("aider", "#!/bin/sh\nprintf 'boom\\n' >&2\nexit 1\n");
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(Runner {
                bin,
                sessions_dir: sessions.to_string_lossy().to_string(),
            }),
            request: contract_request(work_dir, true),
            timeout: Duration::from_secs(5),
        }
    }

    fn contract_timeout_run() -> ContractCase {
        let sessions = temp_dir("aider-contract-timeout-sessions");
        let work_dir = temp_dir("aider-contract-timeout-work");
        let cli = FakeCli::new("aider", "#!/bin/sh\nsleep 2\n");
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(Runner {
                bin,
                sessions_dir: sessions.to_string_lossy().to_string(),
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
