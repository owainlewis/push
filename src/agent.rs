//! Agent backend boundary. The gateway owns messaging and assistant context;
//! concrete agent CLIs own reasoning, tools, and execution.

use std::time::Duration;

use uuid::Uuid;

use crate::{claude, codex};

/// One headless agent turn.
pub struct Request<'a> {
    pub session_id: &'a str,
    pub is_new: bool,
    pub work_dir: &'a str,
    pub system_append: &'a str,
    pub prompt: &'a str,
}

/// A completed agent turn.
pub struct RunOutput {
    pub reply: String,
    pub session_id: Option<String>,
}

/// What went wrong, separated so the gateway can phrase timeouts differently.
pub enum RunError {
    Timeout,
    Failed(String),
}

pub enum Runner {
    Claude(claude::Runner),
    Codex(codex::Runner),
}

impl Runner {
    pub fn backend(&self) -> &'static str {
        match self {
            Runner::Claude(_) => "claude",
            Runner::Codex(_) => "codex",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Runner::Claude(_) => "Claude",
            Runner::Codex(_) => "Codex",
        }
    }

    pub fn initial_session_id(&self) -> String {
        match self {
            Runner::Claude(_) => Uuid::new_v4().to_string(),
            Runner::Codex(_) => String::new(),
        }
    }

    pub fn mark_started_before_run(&self) -> bool {
        matches!(self, Runner::Claude(_))
    }

    pub async fn run(&self, req: Request<'_>, timeout: Duration) -> Result<RunOutput, RunError> {
        match self {
            Runner::Claude(r) => r.run(req, timeout).await,
            Runner::Codex(r) => r.run(req, timeout).await,
        }
    }
}
