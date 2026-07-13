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

fn temp_dir(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("push-init-cli-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}
