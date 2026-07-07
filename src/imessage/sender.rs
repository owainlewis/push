//! Sends iMessages through the Messages app via osascript.

use anyhow::{anyhow, Result};
use tokio::process::Command;

#[derive(Clone)]
pub struct Sender;

impl Sender {
    pub fn new() -> Self {
        Self
    }

    /// Delivers `text` to the given iMessage handle (phone number or email).
    ///
    /// The message and target are passed as AppleScript argv rather than
    /// interpolated into the script, so no escaping is needed.
    #[cfg_attr(test, allow(dead_code))]
    pub async fn send(&self, target: &str, text: &str) -> Result<()> {
        let out = Command::new("osascript")
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
            .arg(target)
            .output()
            .await?;
        if !out.status.success() {
            return Err(anyhow!(
                "osascript send: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
}
