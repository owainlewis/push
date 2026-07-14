use std::process::Command;

use uuid::Uuid;

#[test]
fn init_without_path_creates_assistant_in_current_directory() {
    let root = temp_dir("default");
    let home = root.join("home");
    let workdir = root.join("workdir");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_push"))
        .arg("init")
        .current_dir(&workdir)
        .env("HOME", &home)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let assistant = workdir.join("assistant");
    assert!(assistant.join("SOUL.md").is_file());
    assert!(assistant.join("context/README.md").is_file());
    assert!(assistant.join("jobs").is_dir());
    assert!(assistant.join(".git").exists());
    let config_path = home.join(".push/config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    assert!(!workdir.join("config.toml").exists());
    assert!(config.contains("channel = \"telegram\""));
    assert!(config.contains("agent = \"codex\""));
    assert!(config.contains("[telegram]"));
    assert!(config.contains("allow_user_ids = []"));
    assert!(config.contains("TELEGRAM_BOT_TOKEN"));
    assert!(config.contains(
        &assistant
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string()
    ));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Review or configure the channel and its allowlist:"));
    assert!(stdout.contains("push doctor"));
    assert!(!stdout.contains("push doctor --config"));
    assert!(stdout.contains(&format!("$EDITOR {}", config_path.display())));
    assert!(
        stdout
            .find(&format!("$EDITOR {}", config_path.display()))
            .unwrap()
            < stdout.find("push doctor").unwrap()
    );
    assert!(stdout.contains("SOUL.md"));

    let run_output = Command::new(env!("CARGO_BIN_EXE_push"))
        .current_dir(&workdir)
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(!run_output.status.success());
    let run_stderr = String::from_utf8_lossy(&run_output.stderr);
    assert!(run_stderr.contains(&format!("load config {}", config_path.display())));
    assert!(run_stderr.contains("set telegram.allow_user_ids or telegram.allow_chat_ids"));
    assert!(!run_stderr.contains("imessage"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn init_expands_home_in_requested_path() {
    let root = temp_dir("home");
    let home = root.join("home");
    let config = root.join("push.toml");
    std::fs::create_dir_all(&home).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_push"))
        .args(["init", "~/chosen", "--config"])
        .arg(&config)
        .env("HOME", &home)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(home.join("chosen/SOUL.md").is_file());
    assert!(std::fs::read_to_string(config).unwrap().contains(
        &home
            .join("chosen")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string()
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn run_without_default_config_reports_first_run_guidance() {
    let root = temp_dir("missing-default-config");
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_push"))
        .current_dir(&root)
        .env("HOME", &home)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("configuration not found at ~/.push/config.toml"));
    assert!(stderr.contains("Create it with:\n  push init"));
    assert!(!stderr.contains("push init --config"));
    assert!(stderr.contains("Then configure a channel"));
    assert!(!stderr.contains("Caused by:"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn run_reads_existing_default_config_from_home() {
    let root = temp_dir("existing-default-config");
    let home = root.join("home");
    let config_dir = home.join(".push");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "invalid = [").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_push"))
        .current_dir(&root)
        .env("HOME", &home)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("parse TOML"));
    assert!(!stderr.contains("configuration not found"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn doctor_without_default_config_reports_init_guidance() {
    assert_missing_default_config_guidance(&["doctor"]);
}

#[test]
fn job_without_default_config_reports_init_guidance() {
    assert_missing_default_config_guidance(&["job", "list"]);
}

fn assert_missing_default_config_guidance(args: &[&str]) {
    let root = temp_dir("missing-default-config-command");
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_push"))
        .args(args)
        .current_dir(&root)
        .env("HOME", &home)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("configuration not found at ~/.push/config.toml"));
    assert!(combined.contains("Create it with:\n  push init"));
    assert!(!combined.contains("config.toml.example"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn run_with_missing_custom_config_reports_selected_path() {
    let root = temp_dir("missing-custom-config");
    let config = root.join("custom config's.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_push"))
        .args(["--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let quoted_path = format!("'{}'", config.display().to_string().replace('\'', "'\\''"));
    let expected = format!("push init --config {quoted_path}");
    assert!(stderr.contains(&expected));
    assert!(!stderr.contains("read config"));
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn run_with_dangling_config_symlink_preserves_load_error() {
    use std::os::unix::fs::symlink;

    let root = temp_dir("dangling-config-symlink");
    let config = root.join("config.toml");
    symlink(root.join("missing-target.toml"), &config).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_push"))
        .args(["--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("read config"));
    assert!(!stderr.contains("Create it with:"));
    let _ = std::fs::remove_dir_all(root);
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("push-init-cli-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}
