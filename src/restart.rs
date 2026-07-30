use std::io::{self, Write};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;

const LAUNCHD_LABEL: &str = "com.owainlewis.push";
const SYSTEMD_UNIT: &str = "push.service";

pub fn gateway() -> Result<()> {
    let command = platform_command()?;
    write_status("Restarting gateway...");
    let message = execute(&command, |command| {
        let mut process = Command::new(command.program);
        process.args(&command.args);
        let status = process.status()?;
        Ok(ProcessStatus {
            success: status.success(),
            description: status.to_string(),
        })
    })?;
    write_status(message);
    Ok(())
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ServiceStatus {
    manager: &'static str,
    unit: &'static str,
    running: bool,
    state: String,
}

pub(crate) fn gateway_status() -> Result<ServiceStatus> {
    let (manager, unit, command) =
        status_command_for(std::env::consts::OS, effective_user_id()?.as_deref())?;
    status_with(manager, unit, &command, |command| {
        let mut process = Command::new(command.program);
        process.args(&command.args);
        let output = process.output()?;
        Ok(StatusOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    })
}

pub(crate) fn print_gateway_status() -> Result<()> {
    let status = gateway_status()?;
    println!(
        "Gateway service: {}",
        if status.running {
            "running"
        } else {
            status.state.as_str()
        }
    );
    println!("Manager: {}", status.manager);
    println!("Unit: {}", status.unit);
    Ok(())
}

fn write_status(message: &str) {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{message}");
    let _ = stdout.flush();
}

struct ProcessStatus {
    success: bool,
    description: String,
}

struct StatusOutput {
    success: bool,
    stdout: String,
    stderr: String,
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

fn status_command_for(
    os: &str,
    user_id: Option<&str>,
) -> Result<(&'static str, &'static str, PlatformCommand)> {
    match os {
        "macos" => {
            let user_id = user_id.context("determine current user id for launchd")?;
            Ok((
                "launchd",
                LAUNCHD_LABEL,
                PlatformCommand {
                    program: "launchctl",
                    args: vec![
                        "print".to_string(),
                        format!("gui/{user_id}/{LAUNCHD_LABEL}"),
                    ],
                },
            ))
        }
        "linux" => Ok((
            "systemd",
            SYSTEMD_UNIT,
            PlatformCommand {
                program: "systemctl",
                args: vec![
                    "--user".to_string(),
                    "is-active".to_string(),
                    SYSTEMD_UNIT.to_string(),
                ],
            },
        )),
        _ => bail!("gateway status is supported only on macOS and Linux"),
    }
}

fn status_with(
    manager: &'static str,
    unit: &'static str,
    command: &PlatformCommand,
    runner: impl FnOnce(&PlatformCommand) -> std::io::Result<StatusOutput>,
) -> Result<ServiceStatus> {
    let output = runner(command).with_context(|| format!("run {}", command.display()))?;
    let state = if manager == "launchd" && output.success {
        normalize_launchd_state(&output.stdout)?
    } else if output.success {
        "active".to_string()
    } else if manager == "launchd"
        && (output.stderr.contains("Could not find service")
            || output.stderr.contains("service not found"))
    {
        "not_loaded".to_string()
    } else if manager == "systemd"
        && matches!(
            output.stdout.as_str(),
            "inactive" | "failed" | "activating" | "deactivating" | "unknown"
        )
    {
        output.stdout
    } else {
        let detail = if output.stderr.is_empty() {
            "no status was returned"
        } else {
            output.stderr.as_str()
        };
        bail!("{} failed: {detail}", command.display());
    };
    Ok(ServiceStatus {
        manager,
        unit,
        running: output.success && state == "active",
        state,
    })
}

fn normalize_launchd_state(output: &str) -> Result<String> {
    let state = output
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("state = "))
        .context("launchctl did not report a service state")?;
    Ok(match state {
        "running" => "active".to_string(),
        "not running" => "inactive".to_string(),
        other => other.replace(' ', "_"),
    })
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
    fn macos_status_inspects_the_documented_launchd_service() {
        let (manager, unit, command) = status_command_for("macos", Some("501")).unwrap();
        assert_eq!(manager, "launchd");
        assert_eq!(unit, LAUNCHD_LABEL);
        assert_eq!(
            command,
            PlatformCommand {
                program: "launchctl",
                args: vec![
                    "print".to_string(),
                    "gui/501/com.owainlewis.push".to_string(),
                ],
            }
        );
    }

    #[test]
    fn macos_status_parses_running_launchd_service() {
        let (manager, unit, command) = status_command_for("macos", Some("501")).unwrap();
        let status = status_with(manager, unit, &command, |_| {
            Ok(StatusOutput {
                success: true,
                stdout: "service = {\n\tstate = running\n}".to_string(),
                stderr: String::new(),
            })
        })
        .unwrap();

        assert_eq!(
            status,
            ServiceStatus {
                manager: "launchd",
                unit: LAUNCHD_LABEL,
                running: true,
                state: "active".to_string(),
            }
        );
    }

    #[test]
    fn macos_status_parses_loaded_but_stopped_launchd_service() {
        let (manager, unit, command) = status_command_for("macos", Some("501")).unwrap();
        let status = status_with(manager, unit, &command, |_| {
            Ok(StatusOutput {
                success: true,
                stdout: "service = {\n\tstate = not running\n}".to_string(),
                stderr: String::new(),
            })
        })
        .unwrap();

        assert_eq!(
            status,
            ServiceStatus {
                manager: "launchd",
                unit: LAUNCHD_LABEL,
                running: false,
                state: "inactive".to_string(),
            }
        );
    }

    #[test]
    fn linux_status_reports_inactive_as_a_successful_observation() {
        let (manager, unit, command) = status_command_for("linux", None).unwrap();
        let status = status_with(manager, unit, &command, |_| {
            Ok(StatusOutput {
                success: false,
                stdout: "inactive".to_string(),
                stderr: String::new(),
            })
        })
        .unwrap();

        assert_eq!(
            status,
            ServiceStatus {
                manager: "systemd",
                unit: SYSTEMD_UNIT,
                running: false,
                state: "inactive".to_string(),
            }
        );
    }

    #[test]
    fn service_manager_operational_failure_is_an_error() {
        let (manager, unit, command) = status_command_for("linux", None).unwrap();
        let error = status_with(manager, unit, &command, |_| {
            Ok(StatusOutput {
                success: false,
                stdout: String::new(),
                stderr: "Failed to connect to bus".to_string(),
            })
        })
        .unwrap_err();

        assert!(error.to_string().contains("Failed to connect to bus"));
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
