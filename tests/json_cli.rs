use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rusqlite::params;
use serde_json::Value;
use uuid::Uuid;

const SECRET: &str = "xoxb-json-contract-secret";

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    config: PathBuf,
    assistant: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("push-json-{name}-{}", Uuid::new_v4()));
        let home = root.join("home");
        let assistant = root.join("assistant");
        let config_dir = home.join(".push");
        let config = config_dir.join("config.toml");
        std::fs::create_dir_all(assistant.join("jobs")).unwrap();
        std::fs::create_dir_all(assistant.join("evals")).unwrap();
        std::fs::create_dir_all(assistant.join("context")).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(assistant.join("SOUL.md"), "# Assistant\n").unwrap();
        std::fs::write(
            &config,
            format!(
                "channel = \"telegram\"\nagent = \"codex\"\nassistant_root = {:?}\n\n[telegram]\nbot_token = {SECRET:?}\nallow_user_ids = [123]\n",
                assistant
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        Self {
            root,
            home,
            config,
            assistant,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_push"));
        command
            .arg("--json")
            .args(["--config"])
            .arg(&self.config)
            .env("HOME", &self.home)
            .env_remove("PUSH_HOME");
        command
    }

    fn install_job(&self, name: &str, contents: &str) {
        std::fs::write(
            self.assistant.join("jobs").join(format!("{name}.md")),
            contents,
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn valid_job() -> &'static str {
    "+++\nversion = 1\ntimeout = \"5m\"\nbackend = \"codex\"\n\n[[triggers]]\nid = \"daily\"\nkind = \"cron\"\nschedule = \"0 9 * * *\"\ntimezone = \"Europe/London\"\nenabled = false\n+++\n\nInspect the assistant repository.\n"
}

fn json_stdout(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn json_stderr(output: &Output) -> Value {
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stderr).unwrap()
}

fn assert_keys(value: &Value, expected: &[&str]) {
    let object = value.as_object().expect("value is an object");
    let actual = object.keys().map(String::as_str).collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

fn assert_success_envelope(payload: &Value, command: &str) {
    assert_keys(payload, &["command", "data", "ok", "schema_version"]);
    assert_eq!(payload["schema_version"], 1);
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["command"], command);
    assert!(payload["data"].is_object());
}

#[test]
fn scoped_commands_emit_one_json_document_without_unrelated_output() {
    let fixture = Fixture::new("success");
    fixture.install_job("daily", valid_job());

    let paths = json_stdout(&fixture.command().arg("paths").output().unwrap());
    assert_success_envelope(&paths, "paths");
    assert_keys(
        &paths["data"],
        &[
            "assistant_context",
            "assistant_evals",
            "assistant_root",
            "audit_log",
            "cache",
            "config",
            "database",
            "default_config",
            "imessage_database",
            "jobs",
            "jobs_run",
            "push_home",
            "slack_inbox",
            "state",
        ],
    );
    for value in paths["data"].as_object().unwrap().values() {
        assert!(value.is_string());
    }

    for (subcommand, expected_command) in [("list", "job.list"), ("validate", "job.validate")] {
        let payload = json_stdout(
            &fixture
                .command()
                .args(["job", subcommand])
                .output()
                .unwrap(),
        );
        assert_success_envelope(&payload, expected_command);
        assert_keys(
            &payload["data"],
            &["invalid", "invalid_count", "valid", "valid_count"],
        );
        assert_eq!(payload["data"]["valid_count"], 1);
        assert_eq!(payload["data"]["invalid_count"], 0);
        assert_keys(
            &payload["data"]["valid"][0],
            &["backend", "name", "path", "status"],
        );
        assert_eq!(payload["data"]["valid"][0]["name"], "daily");
        assert_eq!(payload["data"]["valid"][0]["status"], "valid");
        assert!(payload["data"]["valid"][0]["path"].is_string());
        assert!(payload["data"]["valid"][0]["backend"].is_string());
        assert!(payload["data"]["invalid"].as_array().unwrap().is_empty());
    }

    let show = json_stdout(
        &fixture
            .command()
            .args(["job", "show", "daily"])
            .output()
            .unwrap(),
    );
    assert_success_envelope(&show, "job.show");
    assert_keys(
        &show["data"],
        &[
            "backend",
            "body",
            "evals",
            "name",
            "path",
            "snapshot_hash",
            "timeout_ms",
            "triggers",
            "workdir",
        ],
    );
    for field in [
        "backend",
        "body",
        "name",
        "path",
        "snapshot_hash",
        "workdir",
    ] {
        assert!(show["data"][field].is_string(), "{field}");
    }
    assert!(show["data"]["timeout_ms"].is_u64());
    assert!(show["data"]["evals"].is_array());
    assert_keys(
        &show["data"]["triggers"][0],
        &["enabled", "id", "kind", "schedule", "timezone"],
    );
    assert!(show["data"]["triggers"][0]["enabled"].is_boolean());
    for field in ["id", "kind", "schedule", "timezone"] {
        assert!(show["data"]["triggers"][0][field].is_string(), "{field}");
    }

    let runs = json_stdout(&fixture.command().args(["job", "runs"]).output().unwrap());
    assert_success_envelope(&runs, "job.runs");
    assert_keys(&runs["data"], &["job_name", "runs"]);
    assert!(runs["data"]["job_name"].is_null());
    assert!(runs["data"]["runs"].is_array());

    let reviews = json_stdout(&fixture.command().args(["job", "reviews"]).output().unwrap());
    assert_success_envelope(&reviews, "job.reviews");
    assert_keys(&reviews["data"], &["job_name", "reviews"]);
    assert!(reviews["data"]["job_name"].is_null());
    assert!(reviews["data"]["reviews"].is_array());
}

#[test]
fn paths_use_pushpaths_for_the_selected_runtime_home() {
    let fixture = Fixture::new("push-paths");
    let push_home = fixture.root.join("isolated-runtime");
    let output = fixture
        .command()
        .arg("paths")
        .env("PUSH_HOME", &push_home)
        .output()
        .unwrap();
    let payload = json_stdout(&output);
    let data = &payload["data"];

    assert_eq!(data["push_home"], push_home.to_string_lossy().as_ref());
    assert_eq!(
        data["default_config"],
        push_home.join("config.toml").to_string_lossy().as_ref()
    );
    assert_eq!(
        data["config"],
        fixture
            .config
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        data["database"],
        push_home.join("push.db").to_string_lossy().as_ref()
    );
    assert_eq!(
        data["state"],
        push_home.join("state.json").to_string_lossy().as_ref()
    );
    assert_eq!(
        data["audit_log"],
        push_home.join("audit.jsonl").to_string_lossy().as_ref()
    );
    assert_eq!(
        data["jobs_run"],
        push_home.join("run").to_string_lossy().as_ref()
    );
    assert_eq!(
        data["slack_inbox"],
        push_home
            .join("state.json.slack-inbox.db")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        data["cache"],
        push_home.join("cache").to_string_lossy().as_ref()
    );
}

#[test]
fn help_and_version_pin_their_json_schemas() {
    for (command, fields) in [
        ("help", &["text"][..]),
        ("version", &["name", "version"][..]),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_push"))
            .args(["--json", command])
            .output()
            .unwrap();
        let payload = json_stdout(&output);
        assert_success_envelope(&payload, command);
        assert_keys(&payload["data"], fields);
        for field in fields {
            assert!(payload["data"][field].is_string());
        }
    }
}

#[test]
fn status_emits_json_with_a_fake_service_manager() {
    let fixture = Fixture::new("status");
    let bin_dir = fixture.root.join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let manager = if cfg!(target_os = "macos") {
        "launchctl"
    } else {
        "systemctl"
    };
    let manager_path = bin_dir.join(manager);
    let status_output = if cfg!(target_os = "macos") {
        "service = {\\n\\tstate = running\\n}"
    } else {
        "active"
    };
    std::fs::write(
        &manager_path,
        format!("#!/bin/sh\nprintf '{status_output}\\n'\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&manager_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = std::env::join_paths(std::iter::once(bin_dir).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();

    let output = fixture
        .command()
        .arg("status")
        .env("PATH", path)
        .output()
        .unwrap();
    let payload = json_stdout(&output);
    assert_success_envelope(&payload, "status");
    assert_keys(&payload["data"], &["manager", "running", "state", "unit"]);
    assert!(payload["data"]["manager"].is_string());
    assert!(payload["data"]["unit"].is_string());
    assert_eq!(payload["data"]["running"], true);
    assert_eq!(payload["data"]["state"], "active");
}

#[test]
fn doctor_success_is_machine_readable_and_redacts_credentials() {
    let fixture = Fixture::new("doctor");
    let bin_dir = fixture.root.join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    make_executable(&bin_dir.join("codex"));

    let output = fixture
        .command()
        .arg("doctor")
        .env("PATH", &bin_dir)
        .output()
        .unwrap();
    let payload = json_stdout(&output);
    assert_success_envelope(&payload, "doctor");
    assert_keys(&payload["data"], &["checks"]);
    let checks = payload["data"]["checks"].as_array().unwrap();
    assert!(!checks.is_empty());
    for check in checks {
        assert_keys(check, &["message", "name", "status"]);
        assert!(check["message"].is_string());
        assert!(check["name"].is_string());
        assert!(matches!(check["status"].as_str(), Some("pass" | "fail")));
    }
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
}

#[test]
fn unavailable_service_manager_is_a_structured_error() {
    let fixture = Fixture::new("status-unavailable");
    let bin_dir = fixture.root.join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let manager = if cfg!(target_os = "macos") {
        "launchctl"
    } else {
        "systemctl"
    };
    let manager_path = bin_dir.join(manager);
    std::fs::write(
        &manager_path,
        "#!/bin/sh\nprintf 'service manager unavailable\\n' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&manager_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = std::env::join_paths(std::iter::once(bin_dir).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();

    let output = fixture
        .command()
        .arg("status")
        .env("PATH", path)
        .output()
        .unwrap();
    let payload = json_stderr(&output);
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(payload["error"]["category"], "unavailable_dependency");
}

#[test]
fn mutations_are_rejected_before_config_loading_or_runtime_changes() {
    let fixture = Fixture::new("mutation-rejection");
    for args in [
        Vec::<&str>::new(),
        vec!["init"],
        vec!["restart"],
        vec!["job", "run", "daily"],
    ] {
        let output = fixture.command().args(&args).output().unwrap();
        let payload = json_stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(payload["error"]["category"], "invalid_input", "{args:?}");
        assert_eq!(payload["error"]["exit_code"], 2, "{args:?}");
        assert_eq!(payload["error"]["retryable"], false, "{args:?}");
        assert!(
            payload["error"]["message"]
                .as_str()
                .unwrap()
                .contains("--json is not supported"),
            "{args:?}"
        );
    }
}

#[test]
fn validation_failure_is_json_on_stderr_with_invalid_input_exit_code() {
    let fixture = Fixture::new("validation");
    fixture.install_job("broken", "not a runbook\n");

    let output = fixture
        .command()
        .args(["job", "validate"])
        .output()
        .unwrap();
    let payload = json_stderr(&output);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(payload["error"]["category"], "invalid_input");
    assert_eq!(payload["error"]["exit_code"], 2);
    assert_eq!(payload["error"]["retryable"], false);
    assert_eq!(payload["error"]["details"]["invalid_count"], 1);
    assert_keys(
        &payload["error"]["details"]["invalid"][0],
        &["message", "name", "path", "status"],
    );
}

#[test]
fn missing_config_is_json_on_stderr_with_configuration_exit_code() {
    let fixture = Fixture::new("missing");
    let missing = fixture.root.join("missing.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_push"))
        .args(["--json", "--config"])
        .arg(&missing)
        .arg("paths")
        .env("HOME", &fixture.home)
        .output()
        .unwrap();
    let payload = json_stderr(&output);
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(payload["error"]["category"], "configuration");
    assert_eq!(payload["error"]["exit_code"], 3);
}

#[test]
fn unavailable_backend_is_json_on_stderr_without_secrets() {
    let fixture = Fixture::new("backend");
    let empty_path = fixture.root.join("empty-bin");
    std::fs::create_dir(&empty_path).unwrap();

    let output = fixture
        .command()
        .arg("doctor")
        .env("PATH", empty_path)
        .output()
        .unwrap();
    let payload = json_stderr(&output);
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(payload["error"]["category"], "unavailable_dependency");
    assert_eq!(payload["error"]["exit_code"], 4);
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
}

#[test]
fn malformed_config_errors_do_not_echo_secret_values() {
    let fixture = Fixture::new("redaction");
    std::fs::write(
        &fixture.config,
        format!("telegram_bot_token = {SECRET:?}\ninvalid = ["),
    )
    .unwrap();

    let output = fixture.command().arg("paths").output().unwrap();
    let payload = json_stderr(&output);
    assert_eq!(payload["error"]["category"], "configuration");
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
}

#[test]
fn missing_installed_job_is_a_configuration_error() {
    let fixture = Fixture::new("missing-job");
    let output = fixture
        .command()
        .args(["job", "show", "absent"])
        .output()
        .unwrap();
    let payload = json_stderr(&output);
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(payload["error"]["category"], "configuration");
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_job_filename_still_produces_valid_json() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let fixture = Fixture::new("non-utf8");
    let filename = OsString::from_vec(b"invalid-\xff.md".to_vec());
    std::fs::write(fixture.assistant.join("jobs").join(filename), "invalid").unwrap();

    let output = fixture.command().args(["job", "list"]).output().unwrap();
    let payload = json_stdout(&output);
    assert_eq!(payload["data"]["invalid_count"], 1);
    assert!(payload["data"]["invalid"][0]["path"].is_string());
}

#[test]
fn job_runs_json_omits_stored_content_fields() {
    let fixture = Fixture::new("run-content");
    let initial = fixture.command().args(["job", "runs"]).output().unwrap();
    json_stdout(&initial);
    let database = fixture.home.join(".push/push.db");
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute(
            "INSERT INTO job_runs (
                id, job_name, snapshot_hash, trigger_kind, owner_kind,
                queued_at_ms, backend, permission_profile, timeout_ms, workdir,
                state, result, error, evaluation_state, evaluation_result,
                evaluation_error, delivery_state, delivery_error
             ) VALUES (
                ?1, 'daily', 'hash', 'manual', 'manual_cli',
                1, 'codex', 'agent', 1000, '/tmp',
                'failed', ?2, ?3, 'error', ?4, ?5, 'failed', ?6
             )",
            params![
                "run-1",
                "stored-result-secret",
                "stored-error-secret",
                "stored-evaluation-secret",
                "stored-evaluation-error-secret",
                "stored-delivery-error-secret",
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO channel_cursors (channel, cursor) VALUES ('telegram', 42)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO backend_sessions (
                channel, thread_key, backend, session_id, started
             ) VALUES ('telegram', 'dm:123', 'codex', ?1, 1)",
            ["sqlite-session-secret"],
        )
        .unwrap();

    let output = fixture.command().args(["job", "runs"]).output().unwrap();
    let payload = json_stdout(&output);
    assert_success_envelope(&payload, "job.runs");
    assert_keys(&payload["data"], &["job_name", "runs"]);
    let run = &payload["data"]["runs"][0];
    assert_keys(
        run,
        &[
            "backend",
            "delivery",
            "evaluation",
            "execution",
            "id",
            "job_name",
            "queued_at_ms",
            "state",
            "trigger",
        ],
    );
    assert_keys(&run["trigger"], &["id", "kind", "scheduled_at_ms"]);
    assert_keys(&run["execution"], &["has_error", "has_result"]);
    assert_keys(&run["evaluation"], &["has_error", "has_result", "state"]);
    assert_keys(
        &run["delivery"],
        &["attempts", "channel", "has_error", "state", "target"],
    );
    let text = payload.to_string();
    assert_eq!(payload["data"]["runs"][0]["execution"]["has_result"], true);
    assert_eq!(payload["data"]["runs"][0]["execution"]["has_error"], true);
    for secret in [
        "stored-result-secret",
        "stored-error-secret",
        "stored-evaluation-secret",
        "stored-evaluation-error-secret",
        "stored-delivery-error-secret",
        "sqlite-session-secret",
    ] {
        assert!(!text.contains(secret));
    }
}

#[test]
fn job_reviews_json_exposes_exact_activation_metadata() {
    let fixture = Fixture::new("schedule-review");
    let initial = fixture.command().args(["job", "reviews"]).output().unwrap();
    json_stdout(&initial);
    let database = fixture.home.join(".push/push.db");
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute(
            "INSERT INTO job_schedule_reviews (
                id, job_name, content_hash, snapshot_hash, file_identity, path,
                schedules_json, backend, timeout_ms, workdir, delivery_channel,
                delivery_target, status, proposed_at_ms, decided_at_ms,
                activated_at_ms, reviewed_by, reason
             ) VALUES (
                ?1, 'daily', ?2, ?3, ?4, ?5, ?6, 'codex', 300000, ?7,
                'telegram', '123', 'activated', 1000, 1100, 1200, ?8, ?9
             )",
            params![
                "review-fingerprint",
                "content-sha256",
                "snapshot-sha256",
                "unix:1:2",
                fixture.assistant.join("jobs/daily.md").to_string_lossy(),
                r#"[{"id":"daily","kind":"cron","schedule":"0 9 * * *","timezone":"Europe/London","enabled":true}]"#,
                fixture.assistant.to_string_lossy(),
                "channel=telegram thread=dm:123 sender=123 chat=123",
                "approved exact revision",
            ],
        )
        .unwrap();

    let output = fixture
        .command()
        .args(["job", "reviews", "daily"])
        .output()
        .unwrap();
    let payload = json_stdout(&output);
    assert_success_envelope(&payload, "job.reviews");
    assert_keys(&payload["data"], &["job_name", "reviews"]);
    assert_eq!(payload["data"]["job_name"], "daily");
    let review = &payload["data"]["reviews"][0];
    assert_keys(
        review,
        &[
            "backend",
            "content_hash",
            "delivery",
            "job_name",
            "reason",
            "review_id",
            "reviewed_by",
            "schedules",
            "status",
            "timeout_ms",
            "workdir",
        ],
    );
    assert_eq!(review["review_id"], "review-fingerprint");
    assert_eq!(review["job_name"], "daily");
    assert_eq!(review["status"], "activated");
    assert_eq!(review["content_hash"], "content-sha256");
    assert_eq!(review["backend"], "codex");
    assert_eq!(review["timeout_ms"], 300000);
    assert_eq!(review["delivery"]["channel"], "telegram");
    assert_eq!(review["delivery"]["target"], "123");
    assert_keys(
        &review["schedules"][0],
        &["enabled", "id", "kind", "schedule", "timezone"],
    );
    assert_eq!(review["schedules"][0]["id"], "daily");
    assert_eq!(review["schedules"][0]["enabled"], true);
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(not(unix))]
fn make_executable(path: &Path) {
    std::fs::write(path, "").unwrap();
}
