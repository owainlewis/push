//! Runs the Pi coding agent headlessly for a single message.

use std::time::Duration;
use std::{io, process::Stdio};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::agent::{final_reply, Request, RunError, RunOutput};
use crate::progress::{preview_from_args, ProgressEvent, ProgressPhase};

/// Runner invokes `pi` in non-interactive JSON event mode.
pub struct Runner {
    pub bin: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunMode {
    Configured,
    Unattended { trust_project_resources: bool },
    Evaluator,
}

impl Runner {
    /// Executes one turn and returns Pi's final reply plus its stable session id.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn run(&self, req: Request<'_>, timeout: Duration) -> Result<RunOutput, RunError> {
        self.run_with_progress(req, timeout, None).await
    }

    pub async fn run_with_progress(
        &self,
        req: Request<'_>,
        timeout: Duration,
        progress: Option<mpsc::UnboundedSender<ProgressEvent>>,
    ) -> Result<RunOutput, RunError> {
        self.run_with_mode(req, timeout, RunMode::Configured, progress)
            .await
    }

    pub async fn run_unattended(
        &self,
        req: Request<'_>,
        timeout: Duration,
        trust_project_resources: bool,
    ) -> Result<RunOutput, RunError> {
        self.run_with_mode(
            req,
            timeout,
            RunMode::Unattended {
                trust_project_resources,
            },
            None,
        )
        .await
    }

    pub async fn run_evaluator(
        &self,
        req: Request<'_>,
        timeout: Duration,
    ) -> Result<RunOutput, RunError> {
        self.run_with_mode(req, timeout, RunMode::Evaluator, None)
            .await
    }

    async fn run_with_mode(
        &self,
        req: Request<'_>,
        timeout: Duration,
        mode: RunMode,
        progress: Option<mpsc::UnboundedSender<ProgressEvent>>,
    ) -> Result<RunOutput, RunError> {
        let attempt = crate::agent::output_with_retry(|| {
            let mut cmd = self.command(&req, mode);
            let prompt = req.prompt.as_bytes().to_vec();
            let progress = progress.clone();
            async move { run_child(&mut cmd, &prompt, progress).await }
        });
        let out = match tokio::time::timeout(timeout, attempt).await {
            Err(_) => return Err(RunError::Timeout),
            Ok(Err(error)) => return Err(RunError::Failed(format!("run pi: {error}"))),
            Ok(Ok(output)) => output,
        };

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
                out.parse_error.as_deref(),
            )));
        }

        if let Some(error) = out.parse_error {
            return Err(RunError::Failed(error));
        }
        if req.is_new && out.parsed.session_id.is_none() {
            return Err(RunError::Failed(
                "pi did not report a session id".to_string(),
            ));
        }
        if out.parsed.assistant_failed {
            return Err(RunError::Failed(
                "Pi assistant request failed; check Pi provider and authentication settings"
                    .to_string(),
            ));
        }
        let reply = out.parsed.reply.unwrap_or_default();

        Ok(RunOutput {
            reply: final_reply("pi", &reply)?,
            session_id: req.is_new.then_some(out.parsed.session_id).flatten(),
        })
    }

    fn command(&self, req: &Request<'_>, mode: RunMode) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--print").arg("--mode").arg("json");
        if mode == RunMode::Evaluator {
            cmd.arg("--no-approve")
                .arg("--no-tools")
                .arg("--no-extensions")
                .arg("--no-skills")
                .arg("--no-prompt-templates")
                .arg("--no-context-files")
                .arg("--no-session");
        } else if let RunMode::Unattended {
            trust_project_resources,
        } = mode
        {
            cmd.arg(if trust_project_resources {
                "--approve"
            } else {
                "--no-approve"
            });
        }
        if !req.instructions.trim().is_empty() {
            cmd.arg("--append-system-prompt")
                .arg(req.instructions.trim());
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

struct ChildOutput {
    status: std::process::ExitStatus,
    stderr: Vec<u8>,
    parsed: ParsedOutput,
    parse_error: Option<String>,
}

async fn run_child(
    cmd: &mut Command,
    prompt: &[u8],
    progress: Option<mpsc::UnboundedSender<ProgressEvent>>,
) -> io::Result<ChildOutput> {
    let mut child = cmd.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "pi stdin unavailable"))?;
    let write_result = stdin.write_all(prompt).await;
    drop(stdin);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "pi stdout unavailable"))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "pi stderr unavailable"))?;

    let stderr_task = tokio::spawn(async move {
        let mut stderr = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut stderr).await;
        stderr
    });

    let mut parsed = ParsedOutput::default();
    let mut parse_error = None;
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match apply_jsonl_line(&mut parsed, &line, progress.as_ref()) {
            Ok(()) => {}
            Err(error) => {
                parse_error = Some(error);
                break;
            }
        }
    }

    let status = child.wait().await?;
    let stderr = stderr_task.await.unwrap_or_default();
    if status.success() {
        write_result?;
    }
    Ok(ChildOutput {
        status,
        stderr,
        parsed,
        parse_error,
    })
}

#[derive(Default)]
struct ParsedOutput {
    session_id: Option<String>,
    reply: Option<String>,
    assistant_failed: bool,
}

fn apply_jsonl_line(
    parsed: &mut ParsedOutput,
    line: &str,
    progress: Option<&mpsc::UnboundedSender<ProgressEvent>>,
) -> Result<(), String> {
    let event: Value =
        serde_json::from_str(line).map_err(|_| "pi returned malformed JSON output".to_string())?;
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
                return Ok(());
            };
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                return Ok(());
            }
            if matches!(
                message.get("stopReason").and_then(Value::as_str),
                Some("error" | "aborted")
            ) {
                parsed.reply = None;
                parsed.assistant_failed = true;
                return Ok(());
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
        Some("tool_execution_start") => {
            if let Some(tx) = progress {
                let tool_name = event
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = event.get("args").cloned().unwrap_or(Value::Null);
                let _ = tx.send(ProgressEvent {
                    preview: preview_from_args(&tool_name, &args),
                    tool_name,
                    phase: ProgressPhase::Start,
                });
            }
        }
        Some("tool_execution_end") => {
            if let Some(tx) = progress {
                if event.get("isError").and_then(Value::as_bool) == Some(true) {
                    let tool_name = event
                        .get("toolName")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let args = event.get("args").cloned().unwrap_or(Value::Null);
                    let _ = tx.send(ProgressEvent {
                        preview: preview_from_args(&tool_name, &args),
                        tool_name,
                        phase: ProgressPhase::End { is_error: true },
                    });
                }
            }
        }
        _ => {}
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        assert_runner_contract, composed_prompt_parts, sh_arg, temp_dir, temp_path, ContractCase,
        ContractRequest, ContractRunner, FakeCli, RunnerContract,
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
        let (instructions, prompt) = composed_prompt_parts(&work_dir);

        let output = runner
            .run(
                Request {
                    session_id: "",
                    is_new: true,
                    work_dir: work_dir.to_str().unwrap(),
                    instructions: &instructions,
                    prompt: &prompt,
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(output.reply, "hello");
        assert_eq!(output.session_id.as_deref(), Some("pi-session"));
        let args = read_args(&args_path);
        assert_arg_pair(&args, "--mode", "json");
        let raw_args = std::fs::read_to_string(&args_path).unwrap();
        assert!(raw_args.contains(&format!("--append-system-prompt\n{instructions}\n")));
        assert!(!args.contains(&"--approve".to_string()));
        assert!(!args.contains(&"--no-approve".to_string()));
        assert!(!args.contains(&"--tools".to_string()));
        assert!(!args.contains(&"--session".to_string()));
        assert_eq!(read_prompt(&args_path), prompt);
        assert!(!args.contains(&prompt));
    }

    #[tokio::test]
    async fn streams_tool_progress_events_before_final_reply() {
        let work_dir = temp_dir("pi-progress-work");
        let script = "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"type\":\"session\",\"id\":\"progress-session\"}'\nprintf '%s\\n' '{\"type\":\"tool_execution_start\",\"toolCallId\":\"1\",\"toolName\":\"bash\",\"args\":{\"command\":\"ls -la\"}}'\nprintf '%s\\n' '{\"type\":\"tool_execution_end\",\"toolCallId\":\"1\",\"toolName\":\"bash\",\"result\":null,\"isError\":false}'\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}],\"stopReason\":\"stop\"}}'\n";
        let cli = FakeCli::new("pi", script);
        let runner = Runner { bin: cli.bin() };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let output = runner
            .run_with_progress(
                request(work_dir.to_str().unwrap(), true),
                Duration::from_secs(5),
                Some(tx),
            )
            .await
            .unwrap();

        assert_eq!(output.reply, "done");
        let event = rx.recv().await.expect("progress event");
        assert_eq!(event.tool_name, "bash");
        assert_eq!(event.preview, "ls -la");
        assert!(matches!(event.phase, crate::progress::ProgressPhase::Start));
        assert!(
            rx.try_recv().is_err(),
            "successful tool end should not emit"
        );
    }

    #[tokio::test]
    async fn unattended_run_trusts_project_local_resources() {
        let args_path = temp_path("pi-unattended-args");
        let work_dir = temp_dir("pi-unattended-work");
        let cli = FakeCli::new("pi", &success_script(&args_path, "pi-session", "done"));
        let runner = Runner { bin: cli.bin() };

        runner
            .run_unattended(
                request(work_dir.to_str().unwrap(), true),
                Duration::from_secs(5),
                true,
            )
            .await
            .unwrap();

        let args = read_args(&args_path);
        assert!(args.iter().any(|arg| arg == "--approve"));
        assert!(!args.iter().any(|arg| arg == "--no-approve"));
    }

    #[tokio::test]
    async fn unattended_run_does_not_trust_external_project_resources() {
        let args_path = temp_path("pi-unattended-untrusted-args");
        let work_dir = temp_dir("pi-unattended-untrusted-work");
        let cli = FakeCli::new("pi", &success_script(&args_path, "pi-session", "done"));
        let runner = Runner { bin: cli.bin() };

        runner
            .run_unattended(
                request(work_dir.to_str().unwrap(), true),
                Duration::from_secs(5),
                false,
            )
            .await
            .unwrap();

        let args = read_args(&args_path);
        assert!(args.iter().any(|arg| arg == "--no-approve"));
        assert!(!args.iter().any(|arg| arg == "--approve"));
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
                    instructions: "SOUL instructions",
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
        assert!(!args.contains(&"--tools".to_string()));
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
                "without a final reply",
            ),
            (
                r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"type":"session","id":"id"}'
printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":" \t "}],"stopReason":"stop"}}'
"#,
                "without a final reply",
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

    #[tokio::test]
    async fn evaluator_disables_all_tools_and_extensions() {
        let work_dir = temp_dir("pi-evaluator-work");
        let args_path = temp_path("pi-evaluator-args");
        let cli = FakeCli::new(
            "pi",
            &success_script(&args_path, "eval-session", "VERDICT: PASS"),
        );
        let runner = Runner { bin: cli.bin() };

        runner
            .run_evaluator(
                request(work_dir.to_str().unwrap(), true),
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        let args = read_args(&args_path);
        assert!(args.iter().any(|arg| arg == "--no-tools"));
        assert!(args.iter().any(|arg| arg == "--no-extensions"));
        assert!(args.iter().any(|arg| arg == "--no-skills"));
        assert!(args.iter().any(|arg| arg == "--no-context-files"));
        assert!(args.iter().any(|arg| arg == "--no-approve"));
        assert!(!args.iter().any(|arg| arg == "--approve"));
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
            prompt: "hello".to_string(),
        }
    }
}
