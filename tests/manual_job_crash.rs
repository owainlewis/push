#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension};
use uuid::Uuid;

#[test]
fn job_cli_validates_lists_shows_runs_and_reads_history() {
    let root = std::env::temp_dir().join(format!("push-job-cli-{}", Uuid::new_v4()));
    let jobs = root.join("assistant/jobs");
    let work = root.join("work");
    let run = root.join("run");
    let database = root.join("push.db");
    let config = root.join("config.toml");
    let codex = root.join("fake-codex");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    write_executable(
        &codex,
        r#"#!/bin/sh
out=''
prev=''
for arg in "$@"; do
  if [ "$prev" = '-o' ]; then out="$arg"; fi
  prev="$arg"
done
printf '%s\n' 'cli result' > "$out"
printf '%s\n' '{"type":"thread.started","thread_id":"cli-thread"}'
"#,
    );
    write_job_and_config(&jobs, &work, &run, &database, &config, &codex);
    let binary = env!("CARGO_BIN_EXE_push");

    let validate = run_cli(binary, &config, &["job", "validate"]);
    assert!(validate.status.success());
    assert!(stdout(&validate).contains("VALID\tcrash-test"));

    let list = run_cli(binary, &config, &["job", "list"]);
    assert!(list.status.success());
    assert!(stdout(&list).contains("crash-test\tvalid\tcodex"));

    let show = run_cli(binary, &config, &["job", "show", "crash-test"]);
    assert!(show.status.success());
    assert!(stdout(&show).contains("name: crash-test"));
    assert!(stdout(&show).contains("Return a short result."));

    let run_result = run_cli(binary, &config, &["job", "run", "crash-test"]);
    assert!(run_result.status.success());
    assert!(stdout(&run_result).contains("cli result"));

    let history = run_cli(binary, &config, &["job", "runs", "crash-test"]);
    assert!(history.status.success());
    assert!(stdout(&history).contains("\tcrash-test\tsucceeded\tcodex\t"));
    assert!(stdout(&history).contains("cli result"));

    Connection::open(&database)
        .unwrap()
        .execute(
            "UPDATE job_runs SET delivery_state = 'failed', delivery_attempts = 3,
                delivery_error = 'delivery boom', delivery_channel = 'telegram',
                delivery_target = '7' WHERE job_name = 'crash-test'",
            [],
        )
        .unwrap();
    let failed_delivery = run_cli(binary, &config, &["job", "runs", "crash-test"]);
    let failed_output = stdout(&failed_delivery);
    assert!(failed_output.contains("cli result"));
    assert!(failed_output.contains("delivery boom"));
    assert!(failed_output.contains("failed(3)\ttelegram:7"));

    std::fs::write(jobs.join("invalid.md"), "not a runbook").unwrap();
    let invalid = run_cli(binary, &config, &["job", "validate"]);
    assert!(!invalid.status.success());
    assert!(stdout(&invalid).contains("INVALID\tinvalid"));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("1 invalid job(s)"));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn concurrent_first_runs_on_a_fresh_database_skip_without_sqlite_errors() {
    let root = std::env::temp_dir().join(format!("push-job-race-{}", Uuid::new_v4()));
    let jobs = root.join("assistant/jobs");
    let work = root.join("work");
    let run = root.join("run");
    let database = root.join("push.db");
    let config = root.join("config.toml");
    let codex = root.join("fake-codex");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    write_executable(&codex, "#!/bin/sh\nsleep 30\n");
    write_job_and_config(&jobs, &work, &run, &database, &config, &codex);

    let binary = env!("CARGO_BIN_EXE_push");
    let mut first = spawn_run(binary, &config);
    let mut second = spawn_run(binary, &config);
    wait_for_counts(&database, 1, 1);

    let (first_status, second_status) = wait_for_one_exit(&mut first, &mut second);
    let skipped_status = first_status.or(second_status).unwrap();
    assert!(skipped_status.success());
    if first_status.is_none() {
        first.kill().unwrap();
        first.wait().unwrap();
    } else {
        second.kill().unwrap();
        second.wait().unwrap();
    }

    assert_eq!(count_state(&database, "running"), 1);
    assert_eq!(count_state(&database, "skipped_overlap"), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn live_cli_is_not_reclaimed_and_crashed_cli_is_recovered() {
    let root = std::env::temp_dir().join(format!("push-job-crash-{}", Uuid::new_v4()));
    let jobs = root.join("assistant/jobs");
    let work = root.join("work");
    let run = root.join("run");
    let database = root.join("push.db");
    let config = root.join("config.toml");
    let codex = root.join("fake-codex");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    write_executable(&codex, "#!/bin/sh\nsleep 30\n");
    write_job_and_config(&jobs, &work, &run, &database, &config, &codex);

    let binary = env!("CARGO_BIN_EXE_push");
    let mut live = Command::new(binary)
        .args(["job", "run", "crash-test", "--config"])
        .arg(&config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_state(&database, "running");

    let overlap = Command::new(binary)
        .args(["job", "run", "crash-test", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(overlap.status.success());
    assert!(String::from_utf8_lossy(&overlap.stdout).contains("skipped_overlap"));
    assert_eq!(count_state(&database, "running"), 1);

    live.kill().unwrap();
    let _ = live.wait();
    write_executable(
        &codex,
        r#"#!/bin/sh
out=''
prev=''
for arg in "$@"; do
  if [ "$prev" = '-o' ]; then out="$arg"; fi
  prev="$arg"
done
printf '%s\n' 'recovered result' > "$out"
printf '%s\n' '{"type":"thread.started","thread_id":"fresh-thread"}'
"#,
    );

    let recovered = Command::new(binary)
        .args(["job", "run", "crash-test", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(String::from_utf8_lossy(&recovered.stdout).contains("recovered result"));
    assert_eq!(count_state(&database, "succeeded"), 1);
    assert_eq!(count_state(&database, "failed"), 1);
    assert_eq!(count_state(&database, "skipped_overlap"), 1);
    assert_eq!(count_state(&database, "running"), 0);

    let _ = std::fs::remove_dir_all(root);
}

fn spawn_run(binary: &str, config: &Path) -> std::process::Child {
    Command::new(binary)
        .args(["job", "run", "crash-test", "--config"])
        .arg(config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_for_one_exit(
    first: &mut Child,
    second: &mut Child,
) -> (Option<ExitStatus>, Option<ExitStatus>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let first_status = first.try_wait().unwrap();
        let second_status = second.try_wait().unwrap();
        if first_status.is_some() ^ second_status.is_some() {
            return (first_status, second_status);
        }
        assert!(
            first_status.is_none() && second_status.is_none(),
            "both concurrent runs exited"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for the skipped run to exit");
}

fn run_cli(binary: &str, config: &Path, args: &[&str]) -> std::process::Output {
    Command::new(binary)
        .args(args)
        .arg("--config")
        .arg(config)
        .output()
        .unwrap()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write_job_and_config(
    jobs: &Path,
    work: &Path,
    run: &Path,
    database: &Path,
    config: &Path,
    codex: &Path,
) {
    std::fs::write(
        jobs.join("crash-test.md"),
        format!(
            "+++\nversion = 1\ntimeout = \"1m\"\nworkdir = {:?}\nbackend = \"codex\"\n+++\n\nReturn a short result.\n",
            work.to_string_lossy()
        ),
    )
    .unwrap();
    std::fs::write(
        config,
        format!(
            "channel = \"telegram\"\nagent = \"codex\"\ntelegram_bot_token = \"test\"\ntelegram_allow_user_ids = [1]\ndatabase_path = {:?}\nassistant_root = {:?}\njobs_run_dir = {:?}\ncodex_bin = {:?}\n",
            database.to_string_lossy(),
            jobs.parent().unwrap().to_string_lossy(),
            run.to_string_lossy(),
            codex.to_string_lossy(),
        ),
    )
    .unwrap();
}

fn wait_for_state(database: &Path, state: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if database.exists() {
            if let Ok(connection) = Connection::open(database) {
                let found = connection
                    .query_row(
                        "SELECT 1 FROM job_runs WHERE state = ?1 LIMIT 1",
                        [state],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .ok()
                    .flatten()
                    .is_some();
                if found {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for job state {state}");
}

fn wait_for_counts(database: &Path, running: i64, skipped: i64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if database.exists()
            && count_state_if_ready(database, "running") == Some(running)
            && count_state_if_ready(database, "skipped_overlap") == Some(skipped)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for concurrent run states");
}

fn count_state_if_ready(database: &Path, state: &str) -> Option<i64> {
    Connection::open(database)
        .ok()?
        .query_row(
            "SELECT COUNT(*) FROM job_runs WHERE state = ?1",
            [state],
            |row| row.get(0),
        )
        .ok()
}

fn count_state(database: &Path, state: &str) -> i64 {
    Connection::open(database)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM job_runs WHERE state = ?1",
            [state],
            |row| row.get(0),
        )
        .unwrap()
}

fn write_executable(path: &PathBuf, content: &str) {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, content).unwrap();
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::rename(temporary, path).unwrap();
}
