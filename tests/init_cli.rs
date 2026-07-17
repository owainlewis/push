use std::process::Command;

use uuid::Uuid;

#[test]
fn help_commands_print_usage_without_creating_files() {
    let root = temp_dir("help");
    let home = root.join("home");
    let workdir = root.join("workdir");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();

    for args in [&["help"][..], &["init", "--help"][..]] {
        let output = Command::new(env!("CARGO_BIN_EXE_push"))
            .args(args)
            .current_dir(&workdir)
            .env("HOME", &home)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Usage: push"));
        assert!(stdout.contains("restart"));
        assert!(output.stderr.is_empty());
    }
    assert_eq!(std::fs::read_dir(&workdir).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(root);
}

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
    assert!(assistant.join("evals").is_dir());
    assert!(assistant.join("jobs").is_dir());
    assert!(assistant.join(".git").exists());
    let config_path = home.join(".push/config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    assert!(!workdir.join("config.toml").exists());
    assert!(config.contains("channel = \"telegram\""));
    assert!(config.contains("agent = \"codex\""));
    assert!(config.contains("[telegram]"));
    assert!(config.contains("allow_user_ids = []"));
    assert!(config.contains("bot_token = \"\""));
    assert!(config.contains(
        &assistant
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string()
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&config_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn restart_invokes_the_platform_service_manager() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_dir("restart-service-manager");
    let bin_dir = root.join("bin");
    let args_path = root.join("args");
    std::fs::create_dir(&bin_dir).unwrap();
    let manager = if cfg!(target_os = "macos") {
        "launchctl"
    } else {
        "systemctl"
    };
    let manager_path = bin_dir.join(manager);
    std::fs::write(
        &manager_path,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$PUSH_RESTART_ARGS_PATH\"\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&manager_path).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&manager_path, permissions).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin_dir.clone()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_push"))
        .arg("restart")
        .env("PATH", path)
        .env("PUSH_RESTART_ARGS_PATH", &args_path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Restarted the Push gateway."
    );
    let args = std::fs::read_to_string(args_path).unwrap();
    if cfg!(target_os = "macos") {
        let lines = args.lines().collect::<Vec<_>>();
        assert_eq!(&lines[..2], &["kickstart", "-k"]);
        assert!(lines[2].starts_with("gui/"));
        assert!(lines[2].ends_with("/com.owainlewis.push"));
    } else {
        assert_eq!(args, "--user\nrestart\npush.service\n");
    }
    let _ = std::fs::remove_dir_all(root);
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
