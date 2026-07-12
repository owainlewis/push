//! Agent backend boundary. The gateway owns messaging and assistant context;
//! concrete agent CLIs own reasoning, tools, and execution.

use std::time::Duration;

use uuid::Uuid;

use crate::config::{AgentBackend, PermissionCapability};
use crate::{claude, codex};

/// One headless agent turn.
pub struct Request<'a> {
    pub session_id: &'a str,
    pub is_new: bool,
    pub work_dir: &'a str,
    pub instructions: &'a str,
    pub permission: PermissionCapability,
    pub prompt: &'a str,
}

/// A completed agent turn.
#[derive(Debug)]
pub struct RunOutput {
    pub reply: String,
    pub session_id: Option<String>,
}

/// What went wrong, separated so the gateway can phrase timeouts differently.
#[derive(Debug)]
pub enum RunError {
    Timeout,
    SessionMissing(String),
    Failed(String),
}

pub enum Runner {
    Claude(claude::Runner),
    Codex(codex::Runner),
    #[cfg(test)]
    Fake(FakeRunner),
}

impl Runner {
    pub fn backend(&self) -> AgentBackend {
        match self {
            Runner::Claude(_) => AgentBackend::Claude,
            Runner::Codex(_) => AgentBackend::Codex,
            #[cfg(test)]
            Runner::Fake(r) => r.backend,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Runner::Claude(_) => "Claude",
            Runner::Codex(_) => "Codex",
            #[cfg(test)]
            Runner::Fake(_) => "Fake",
        }
    }

    pub fn initial_session_id(&self) -> String {
        match self {
            Runner::Claude(_) => Uuid::new_v4().to_string(),
            Runner::Codex(_) => String::new(),
            #[cfg(test)]
            Runner::Fake(_) => String::new(),
        }
    }

    pub fn mark_started_before_run(&self) -> bool {
        matches!(self, Runner::Claude(_))
    }

    pub async fn run(&self, req: Request<'_>, timeout: Duration) -> Result<RunOutput, RunError> {
        match self {
            Runner::Claude(r) => r.run(req, timeout).await,
            Runner::Codex(r) => r.run(req, timeout).await,
            #[cfg(test)]
            Runner::Fake(r) => r.run(req, timeout).await,
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
pub struct FakeRunner {
    pub backend: AgentBackend,
    pub session_id: String,
    pub calls: std::sync::Arc<std::sync::Mutex<Vec<FakeRunCall>>>,
    pub before_return: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    pub resume_missing_once: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub struct FakeRunCall {
    pub session_id: String,
    pub is_new: bool,
    pub prompt: String,
    pub permission: PermissionCapability,
}

#[cfg(test)]
impl FakeRunner {
    async fn run(&self, req: Request<'_>, _timeout: Duration) -> Result<RunOutput, RunError> {
        self.calls.lock().unwrap().push(FakeRunCall {
            session_id: req.session_id.to_string(),
            is_new: req.is_new,
            prompt: req.prompt.to_string(),
            permission: req.permission,
        });
        if !req.is_new
            && self
                .resume_missing_once
                .as_ref()
                .is_some_and(|missing| missing.swap(false, std::sync::atomic::Ordering::SeqCst))
        {
            return Err(RunError::SessionMissing(
                "No conversation found with session ID fake-session".to_string(),
            ));
        }
        if let Some(before_return) = &self.before_return {
            before_return();
        }
        Ok(RunOutput {
            reply: format!("fake reply: {}", req.prompt),
            session_id: req.is_new.then(|| self.session_id.clone()),
        })
    }
}
