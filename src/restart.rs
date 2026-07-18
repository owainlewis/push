use std::io::{self, Write};
use std::process::Command;

use anyhow::{bail, Context, Result};

const LAUNCHD_LABEL: &str = "com.owainlewis.push";
const SYSTEMD_UNIT: &str = "push.service";

pub fn gateway() -> Result<()> {
    let command = platform_command()?;
    println!("Restarting gateway...");
    io::stdout()
        .flush()
        .context("flush gateway restart status")?;
    let message = execute(&command, |command| {
        let mut process = Command::new(command.program);
        process.args(&command.args);
        let status = process.status()?;
        Ok(ProcessStatus {
            success: status.success(),
            description: status.to_string(),
        })
    })?;
    println!("{message}");
    Ok(())
}

struct ProcessStatus {
    success: bool,
    description: String,
}

#[derive(Debug, PartialEq, Eq)]
struct PlatformCommand {
    program: &'static str,
    args: Vec<String>,
}

impl PlatformCommand {
    fn display(&self) -> String {
        std::iter::once(self.program.to_string())
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn execute(
    command: &PlatformCommand,
    runner: impl FnOnce(&PlatformCommand) -> std::io::Result<ProcessStatus>,
) -> Result<&'static str> {
    let status = runner(command).with_context(|| format!("run {}", command.display()))?;
    if !status.success {
        bail!(
            "gateway restart failed: {} exited with {}",
            command.display(),
            status.description
        );
    }
    Ok("Gateway restarted.")
}

fn platform_command() -> Result<PlatformCommand> {
    command_for(std::env::consts::OS, effective_user_id()?.as_deref())
}

fn command_for(os: &str, user_id: Option<&str>) -> Result<PlatformCommand> {
    match os {
        "macos" => {
            let user_id = user_id.context("determine current user id for launchd")?;
            Ok(PlatformCommand {
                program: "launchctl",
                args: vec![
                    "kickstart".to_string(),
                    "-k".to_string(),
                    format!("gui/{user_id}/{LAUNCHD_LABEL}"),
                ],
            })
        }
        "linux" => Ok(PlatformCommand {
            program: "systemctl",
            args: vec![
                "--user".to_string(),
                "restart".to_string(),
                SYSTEMD_UNIT.to_string(),
            ],
        }),
        _ => bail!("gateway restart is supported only on macOS and Linux"),
    }
}

fn effective_user_id() -> Result<Option<String>> {
    if std::env::consts::OS != "macos" {
        return Ok(None);
    }
    let output = Command::new("id").arg("-u").output().context("run id -u")?;
    if !output.status.success() {
        bail!("id -u exited with {}", output.status);
    }
    let user_id = String::from_utf8(output.stdout).context("read user id")?;
    let user_id = user_id.trim();
    if user_id.is_empty() || !user_id.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("id -u returned an invalid user id");
    }
    Ok(Some(user_id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_restarts_the_documented_launchd_service() {
        assert_eq!(
            command_for("macos", Some("501")).unwrap(),
            PlatformCommand {
                program: "launchctl",
                args: vec![
                    "kickstart".to_string(),
                    "-k".to_string(),
                    "gui/501/com.owainlewis.push".to_string(),
                ],
            }
        );
    }

    #[test]
    fn linux_restarts_the_documented_user_service() {
        assert_eq!(
            command_for("linux", None).unwrap(),
            PlatformCommand {
                program: "systemctl",
                args: vec![
                    "--user".to_string(),
                    "restart".to_string(),
                    "push.service".to_string(),
                ],
            }
        );
    }

    #[test]
    fn unsupported_platform_reports_the_supported_hosts() {
        let error = command_for("windows", None).unwrap_err();

        assert!(error
            .to_string()
            .contains("supported only on macOS and Linux"));
    }

    #[test]
    fn successful_restart_reports_completion() {
        let command = command_for("linux", None).unwrap();

        let message = execute(&command, |_| {
            Ok(ProcessStatus {
                success: true,
                description: "exit status: 0".to_string(),
            })
        })
        .unwrap();

        assert_eq!(message, "Gateway restarted.");
    }

    #[test]
    fn failed_restart_reports_the_command_and_status() {
        let command = command_for("linux", None).unwrap();

        let error = execute(&command, |_| {
            Ok(ProcessStatus {
                success: false,
                description: "exit status: 5".to_string(),
            })
        })
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("systemctl --user restart push.service exited with exit status: 5"));
    }

    #[test]
    fn restart_spawn_failure_reports_the_command() {
        let command = command_for("linux", None).unwrap();

        let error = execute(&command, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "missing service manager",
            ))
        })
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("run systemctl --user restart push.service"));
    }
}
