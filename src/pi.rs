//! Runs the Pi coding agent headlessly for a single message.

use std::time::Duration;
use std::{io, process::Stdio};

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::agent::{Request, RunError, RunOutput};
use crate::config::PermissionCapability;

/// Runner invokes `pi` in non-interactive JSON event mode.
pub struct Runner {
    pub bin: String,
}

impl Runner {
    /// Executes one turn and returns Pi's final reply plus its stable session id.
    pub async fn run(&self, req: Request<'_>, timeout: Duration) -> Result<RunOutput, RunError> {
        let attempt = crate::agent::output_with_retry(|| {
            let mut cmd = self.command(&req);
            let prompt = req.prompt.as_bytes().to_vec();
            async move {
                let mut child = cmd.spawn()?;
                let mut stdin = child.stdin.take().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "pi stdin unavailable")
                })?;
                let write_result = stdin.write_all(&prompt).await;
                drop(stdin);
                let output = child.wait_with_output().await?;
                if output.status.success() {
                    write_result?;
                }
                Ok(output)
            }
        });
        let out = match tokio::time::timeout(timeout, attempt).await {
            Err(_) => return Err(RunError::Timeout),
            Ok(Err(error)) => return Err(RunError::Failed(format!("run pi: {error}"))),
            Ok(Ok(output)) => output,
        };

        let parsed = parse_jsonl(&out.stdout);
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !req.is_new && missing_resume_error(&stderr) {
                return Err(RunError::SessionMissing(
                    "Pi could not find the saved session; Push will rebuild it from conversation history"
                        .to_string(),
                ));
            }
            return Err(RunError::Failed(exit_diagnostic(
                out.status.code(),
                parsed.as_ref().err().map(String::as_str),
            )));
        }

        let parsed = parsed.map_err(RunError::Failed)?;
        if req.is_new && parsed.session_id.is_none() {
            return Err(RunError::Failed(
                "pi did not report a session id".to_string(),
            ));
        }
        if parsed.assistant_failed {
            return Err(RunError::Failed(
                "Pi assistant request failed; check Pi provider and authentication settings"
                    .to_string(),
            ));
        }
        let reply = parsed.reply.unwrap_or_default();
        if reply.trim().is_empty() {
            return Err(RunError::Failed(
                "pi exited without a final assistant reply".to_string(),
            ));
        }

        Ok(RunOutput {
            reply: reply.trim().to_string(),
            session_id: req.is_new.then_some(parsed.session_id).flatten(),
        })
    }

    fn command(&self, req: &Request<'_>) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--print").arg("--mode").arg("json");
        if !req.instructions.trim().is_empty() {
            cmd.arg("--append-system-prompt")
                .arg(req.instructions.trim());
        }
        if let Some(tools) = tools(req.permission) {
            cmd.arg("--tools").arg(tools);
        }
        if !req.is_new {
            cmd.arg("--session").arg(req.session_id);
        }
        cmd.current_dir(req.work_dir);
        cmd.kill_on_drop(true);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd
    }
}

#[derive(Default)]
struct ParsedOutput {
    session_id: Option<String>,
    reply: Option<String>,
    assistant_failed: bool,
}

fn parse_jsonl(stdout: &[u8]) -> Result<ParsedOutput, String> {
    let stdout = std::str::from_utf8(stdout)
        .map_err(|_| "pi returned malformed JSON output (invalid UTF-8)".to_string())?;
    let mut parsed = ParsedOutput::default();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let event: Value = serde_json::from_str(line)
            .map_err(|_| "pi returned malformed JSON output".to_string())?;
        match event.get("type").and_then(Value::as_str) {
            Some("session") => {
                parsed.session_id = event
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
            }
            Some("message_end") => {
                let Some(message) = event.get("message") else {
                    continue;
                };
                if message.get("role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                if matches!(
                    message.get("stopReason").and_then(Value::as_str),
                    Some("error" | "aborted")
                ) {
                    parsed.reply = None;
                    parsed.assistant_failed = true;
                    continue;
                }
                let text = message
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("");
                parsed.reply = Some(text);
                parsed.assistant_failed = false;
            }
            _ => {}
        }
    }
    Ok(parsed)
}

fn exit_diagnostic(status: Option<i32>, parse_error: Option<&str>) -> String {
    if let Some(error) = parse_error {
        error.to_string()
    } else {
        match status {
            Some(code) => format!(
                "Pi exited with status {code}; run Pi directly as the service user to check provider and authentication settings"
            ),
            None => "Pi was terminated; run Pi directly as the service user to check provider and authentication settings"
                .to_string(),
        }
    }
}

fn missing_resume_error(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("no session found matching")
}

const READ_ONLY_TOOLS: &str = "read,grep,find,ls";
const WORKSPACE_TOOLS: &str = "read,edit,write,grep,find,ls";
const FULL_ACCESS_TOOLS: &str = "read,bash,edit,write,grep,find,ls";

fn tools(capability: PermissionCapability) -> Option<&'static str> {
    match capability {
        PermissionCapability::ReadOnly => Some(READ_ONLY_TOOLS),
        PermissionCapability::Workspace => Some(WORKSPACE_TOOLS),
        PermissionCapability::Inherit => None,
        PermissionCapability::FullAccess => Some(FULL_ACCESS_TOOLS),
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
            name: "Pi",
            new_session: contract_new_session,
            resumed_session: contract_resumed_session,
            failed_run: contract_failed_run,
            timeout_run: contract_timeout_run,
        })
        .await;
    }

    #[tokio::test]
    async fn creates_session_and_separates_instructions_from_prompt() {
        let args_path = temp_path("pi-new-args");
        let work_dir = temp_dir("pi-new-work");
        let cli = FakeCli::new("pi", &success_script(&args_path, "pi-session", "hello"));
        let runner = Runner { bin: cli.bin() };

        let output = runner
            .run(
                Request {
                    session_id: "",
                    is_new: true,
                    work_dir: work_dir.to_str().unwrap(),
                    additional_dirs: &[],
                    instructions: "SOUL instructions",
                    permission: PermissionCapability::Workspace,
                    prompt: "user message",
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(output.reply, "hello");
        assert_eq!(output.session_id.as_deref(), Some("pi-session"));
        let args = read_args(&args_path);
        assert_arg_pair(&args, "--mode", "json");
        assert_arg_pair(&args, "--append-system-prompt", "SOUL instructions");
        assert_arg_pair(&args, "--tools", WORKSPACE_TOOLS);
        assert!(!args.contains(&"--session".to_string()));
        assert_eq!(read_prompt(&args_path), "user message");
        assert!(!args.contains(&"user message".to_string()));
    }

    #[tokio::test]
    async fn sends_option_and_file_like_prompts_verbatim_over_stdin() {
        for prompt in [
            "@/private/file",
            "--provider attacker",
            "-p",
            "line one\nline two",
        ] {
            let args_path = temp_path("pi-verbatim-prompt-args");
            let work_dir = temp_dir("pi-verbatim-prompt-work");
            let cli = FakeCli::new("pi", &success_script(&args_path, "pi-session", "reply"));
            let runner = Runner { bin: cli.bin() };

            runner
                .run(
                    Request {
                        prompt,
                        ..request(work_dir.to_str().unwrap(), true)
                    },
                    Duration::from_secs(5),
                )
                .await
                .unwrap();

            assert_eq!(read_prompt(&args_path), prompt);
            assert!(!read_args(&args_path).iter().any(|arg| arg == prompt));
        }
    }

    #[tokio::test]
    async fn resumes_exact_session() {
        let args_path = temp_path("pi-resume-args");
        let work_dir = temp_dir("pi-resume-work");
        let cli = FakeCli::new("pi", &success_script(&args_path, "pi-session", "again"));
        let runner = Runner { bin: cli.bin() };

        let output = runner
            .run(
                Request {
                    session_id: "pi-session",
                    is_new: false,
                    work_dir: work_dir.to_str().unwrap(),
                    additional_dirs: &[],
                    instructions: "SOUL instructions",
                    permission: PermissionCapability::ReadOnly,
                    prompt: "continue",
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(output.reply, "again");
        assert_eq!(output.session_id, None);
        let args = read_args(&args_path);
        assert_arg_pair(&args, "--session", "pi-session");
        assert_arg_pair(&args, "--tools", READ_ONLY_TOOLS);
    }

    #[tokio::test]
    async fn missing_resume_is_typed_for_rehydration() {
        let work_dir = temp_dir("pi-missing-work");
        let cli = FakeCli::new(
            "pi",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' \"No session found matching 'missing'\" >&2\nexit 1\n",
        );
        let runner = Runner { bin: cli.bin() };
        let error = runner
            .run(
                request(work_dir.to_str().unwrap(), false),
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RunError::SessionMissing(_)));
    }

    #[tokio::test]
    async fn immediate_missing_resume_with_large_prompt_is_typed_for_rehydration() {
        let work_dir = temp_dir("pi-immediate-missing-work");
        let cli = FakeCli::new(
            "pi",
            "#!/bin/sh\nprintf '%s\\n' \"No session found matching 'missing'\" >&2\nexit 1\n",
        );
        let runner = Runner { bin: cli.bin() };
        let prompt = "x".repeat(1024 * 1024);

        let error = runner
            .run(
                Request {
                    prompt: &prompt,
                    ..request(work_dir.to_str().unwrap(), false)
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, RunError::SessionMissing(_)));
    }

    #[tokio::test]
    async fn rejects_malformed_and_empty_output() {
        for (script, expected) in [
            (
                "#!/bin/sh\ncat >/dev/null\nprintf 'not-json\\n'\n",
                "malformed JSON",
            ),
            (
                "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"type\":\"session\",\"id\":\"id\"}'\n",
                "without a final assistant reply",
            ),
        ] {
            let work_dir = temp_dir("pi-bad-output");
            let cli = FakeCli::new("pi", script);
            let runner = Runner { bin: cli.bin() };
            let error = runner
                .run(
                    request(work_dir.to_str().unwrap(), true),
                    Duration::from_secs(5),
                )
                .await
                .unwrap_err();
            assert!(failed_message(error).contains(expected));
        }
    }

    #[tokio::test]
    async fn reports_non_zero_exit_without_forwarding_stderr_secrets() {
        let work_dir = temp_dir("pi-failed-work");
        let secret = "stored-extension-token-123456";
        let cli = FakeCli::new(
            "pi",
            &format!(
                "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' 'provider failed with {secret}' >&2\nexit 2\n"
            ),
        );
        let runner = Runner { bin: cli.bin() };
        let error = runner
            .run(
                request(work_dir.to_str().unwrap(), true),
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        let message = failed_message(error);
        assert!(message.contains("status 2"));
        assert!(!message.contains(secret));
    }

    #[tokio::test]
    async fn successful_auto_retry_uses_the_later_assistant_reply() {
        let work_dir = temp_dir("pi-retry-work");
        let script = "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"type\":\"session\",\"id\":\"retry-session\"}'\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[],\"stopReason\":\"error\",\"errorMessage\":\"secret first failure\"}}'\nprintf '%s\\n' '{\"type\":\"auto_retry_start\",\"attempt\":1}'\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"recovered\"}],\"stopReason\":\"stop\"}}'\n";
        let cli = FakeCli::new("pi", script);
        let runner = Runner { bin: cli.bin() };

        let output = runner
            .run(
                request(work_dir.to_str().unwrap(), true),
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(output.reply, "recovered");
    }

    #[test]
    fn maps_all_permission_profiles_conservatively() {
        assert_eq!(tools(PermissionCapability::ReadOnly), Some(READ_ONLY_TOOLS));
        assert_eq!(
            tools(PermissionCapability::Workspace),
            Some(WORKSPACE_TOOLS)
        );
        assert_eq!(tools(PermissionCapability::Inherit), None);
        assert_eq!(
            tools(PermissionCapability::FullAccess),
            Some(FULL_ACCESS_TOOLS)
        );
        assert!(!WORKSPACE_TOOLS.split(',').any(|tool| tool == "bash"));
    }

    fn success_script(args_path: &std::path::Path, session: &str, reply: &str) -> String {
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\ncat > {}.stdin\nprintf '%s\\n' '{{\"type\":\"session\",\"id\":\"{session}\"}}'\nprintf '%s\\n' '{{\"type\":\"message_end\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{reply}\"}}],\"stopReason\":\"stop\"}}}}'\n",
            sh_arg(args_path),
            sh_arg(args_path)
        )
    }

    fn request(work_dir: &str, is_new: bool) -> Request<'_> {
        Request {
            session_id: if is_new { "" } else { "existing-session" },
            is_new,
            work_dir,
            additional_dirs: &[],
            instructions: "",
            permission: PermissionCapability::ReadOnly,
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

    fn read_prompt(path: &std::path::Path) -> String {
        std::fs::read_to_string(format!("{}.stdin", path.to_string_lossy())).unwrap()
    }

    fn assert_arg_pair(args: &[String], flag: &str, value: &str) {
        let index = args.iter().position(|arg| arg == flag).unwrap();
        assert_eq!(args.get(index + 1).map(String::as_str), Some(value));
    }

    fn failed_message(error: RunError) -> String {
        match error {
            RunError::Failed(message) => message,
            other => panic!("expected failed error, got {other:?}"),
        }
    }

    fn contract_new_session() -> ContractCase {
        contract_success(true)
    }

    fn contract_resumed_session() -> ContractCase {
        contract_success(false)
    }

    fn contract_success(is_new: bool) -> ContractCase {
        let work_dir = temp_dir("pi-contract-success");
        let cli = FakeCli::new(
            "pi",
            &success_script(&temp_path("pi-contract-args"), "pi-session", "reply"),
        );
        let bin = cli.bin();
        ContractCase {
            fake_cli: cli,
            runner: Box::new(Runner { bin }),
            request: contract_request(work_dir, is_new),
            timeout: Duration::from_secs(5),
        }
    }

    fn contract_failed_run() -> ContractCase {
        let work_dir = temp_dir("pi-contract-fail");
        let cli = FakeCli::new(
            "pi",
            "#!/bin/sh\ncat >/dev/null\nprintf 'failed\\n' >&2\nexit 1\n",
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
        let work_dir = temp_dir("pi-contract-timeout");
        let cli = FakeCli::new("pi", "#!/bin/sh\ncat >/dev/null\nsleep 2\n");
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
            session_id: if is_new {
                String::new()
            } else {
                "existing-session".to_string()
            },
            is_new,
            work_dir,
            instructions: String::new(),
            permission: PermissionCapability::ReadOnly,
            prompt: "hello".to_string(),
        }
    }
}
