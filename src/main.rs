//! push is a tiny iMessage gateway for personal assistant agents. It polls the
//! macOS Messages database for new messages, sends each through a configured
//! coding-agent backend, and texts the reply back.

mod agent;
mod claude;
mod codex;
mod config;
mod gateway;
mod imessage;
mod memory;
mod store;
#[cfg(test)]
mod test_support;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    let cfg = config::Config::load(&config_path()).context("config")?;
    preflight(&cfg).context("preflight")?;
    gateway::Gateway::new(cfg).context("init")?.run().await
}

fn config_path() -> String {
    let args: Vec<String> = std::env::args().collect();
    for pair in args.windows(2) {
        if pair[0] == "--config" {
            return pair[1].clone();
        }
    }
    "config.json".to_string()
}

/// Fails fast with actionable messages when the environment is not ready.
fn preflight(cfg: &config::Config) -> Result<()> {
    match std::fs::File::open(&cfg.db_path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!(
            "cannot read {}: grant Full Disk Access to your terminal in System Settings -> Privacy & Security -> Full Disk Access",
            cfg.db_path
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("Messages database not found at {}; is iMessage set up on this Mac?", cfg.db_path)
        }
        Err(e) => bail!("open {}: {e}", cfg.db_path),
    }

    let mut bins = cfg.required_agent_bins().context("agent")?;
    bins.push("osascript");
    bins.sort_unstable();
    bins.dedup();
    for bin in bins {
        if which(bin).is_none() {
            bail!("{bin:?} not found on PATH");
        }
    }

    std::fs::create_dir_all(&cfg.sessions_dir)
        .with_context(|| format!("sessions_dir {} not writable", cfg.sessions_dir))?;
    if let Some(parent) = Path::new(&cfg.state_path).parent() {
        std::fs::create_dir_all(parent).context("state_path dir not writable")?;
    }
    Ok(())
}

fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return p.exists().then_some(p);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(bin))
        .find(|cand| cand.is_file())
}
