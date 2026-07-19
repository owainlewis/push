//! Sends iMessages through the Messages app via osascript.

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use tokio::process::Command;

#[derive(Clone)]
pub struct Sender {
    program: PathBuf,
}

impl Sender {
    pub fn new() -> Self {
        Self {
            program: PathBuf::from("osascript"),
        }
    }

    #[cfg(test)]
    fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// Delivers `text` to the given iMessage handle (phone number or email).
    ///
    /// The message and target are passed as AppleScript argv rather than
    /// interpolated into the script, so no escaping is needed.
    #[cfg_attr(test, allow(dead_code))]
    pub async fn send(&self, target: &str, text: &str) -> Result<()> {
        let mut command = Command::new(&self.program);
        command
            .args([
                "-e",
                "on run argv",
                "-e",
                "set msg to item 1 of argv",
                "-e",
                "set target to item 2 of argv",
                "-e",
                "tell application \"Messages\"",
                "-e",
                "set theService to 1st account whose service type = iMessage",
                "-e",
                "set theBuddy to participant target of theService",
                "-e",
                "send msg to theBuddy",
                "-e",
                "end tell",
                "-e",
                "end run",
                "--",
            ])
            .arg(text)
            .arg(target);
        command.kill_on_drop(true);
        let out = command.output().await?;
        if !out.status.success() {
            return Err(anyhow!(
                "osascript send: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command as StdCommand, Stdio};
    use std::time::Duration;

    #[tokio::test]
    async fn cancelling_send_kills_in_flight_sender() {
        let temp_dir = std::env::temp_dir().join(format!(
            "push-imessage-sender-cancellation-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&temp_dir).unwrap();
        let fake_sender = temp_dir.join("fake-osascript");
        let pid_file = temp_dir.join("pid");
        fs::write(
            &fake_sender,
            "#!/bin/sh\nfor arg do pid_file=$arg; done\necho $$ > \"$pid_file\"\nexec sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&fake_sender, fs::Permissions::from_mode(0o755)).unwrap();

        let sender = Sender::with_program(&fake_sender);
        let target = pid_file.to_string_lossy().into_owned();
        let send = tokio::spawn(async move { sender.send(&target, "hello").await });

        let pid = wait_for_pid(&pid_file).await;
        assert!(
            process_is_running(pid),
            "fake sender did not remain running"
        );

        send.abort();
        assert!(send.await.unwrap_err().is_cancelled());

        for _ in 0..50 {
            if !process_is_running(pid) {
                fs::remove_dir_all(temp_dir).unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        panic!("fake sender process {pid} survived cancellation");
    }

    async fn wait_for_pid(pid_file: &Path) -> u32 {
        for _ in 0..250 {
            if let Ok(pid) = fs::read_to_string(pid_file) {
                return pid.trim().parse().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("fake sender did not record its pid");
    }

    fn process_is_running(pid: u32) -> bool {
        StdCommand::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
