use std::process::Command;

use uuid::Uuid;

#[test]
fn init_without_path_creates_assistant_in_current_directory() {
    let root = temp_dir("default");
    let output = Command::new(env!("CARGO_BIN_EXE_push"))
        .arg("init")
        .current_dir(&root)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let assistant = root.join("assistant");
    assert!(assistant.join("SOUL.md").is_file());
    assert!(assistant.join("context/README.md").is_file());
    assert!(assistant.join("jobs").is_dir());
    assert!(assistant.join(".git").exists());
    let config = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(config.contains(
        &assistant
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string()
    ));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("push doctor"));
    assert!(stdout.contains("SOUL.md"));
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

    let output = Command::new(env!("CARGO_BIN_EXE_push"))
        .current_dir(&root)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("configuration not found at config.toml"));
    assert!(stderr.contains("push init --config config.toml"));
    assert!(stderr.contains("Then configure a channel"));
    assert!(!stderr.contains("Caused by:"));
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
