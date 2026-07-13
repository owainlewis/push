//! Agent backend boundary. The gateway owns messaging and assistant context;
//! concrete agent CLIs own reasoning, tools, and execution.

use std::time::Duration;

use uuid::Uuid;

use crate::config::{AgentBackend, Config, PermissionCapability};
use crate::{claude, codex, pi};

/// One headless agent turn.
pub struct Request<'a> {
    pub session_id: &'a str,
    pub is_new: bool,
    pub work_dir: &'a str,
    pub additional_dirs: &'a [&'a str],
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
    Pi(pi::Runner),
    #[cfg(test)]
    Fake(FakeRunner),
}

/// Linux can briefly report ETXTBSY when an executable was just installed or
/// replaced. Retry only that transient spawn error, within the caller's
/// overall timeout, and preserve every other error unchanged. `spawn` must
/// build a fresh child process attempt on every call.
pub(crate) async fn output_with_retry<F, Fut>(mut spawn: F) -> std::io::Result<std::process::Output>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<std::process::Output>>,
{
    let mut attempts = 0;
    loop {
        match spawn().await {
            Err(error) if error.raw_os_error() == Some(26) && attempts < 3 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            result => return result,
        }
    }
}

impl Runner {
    pub fn for_backend(backend: AgentBackend, cfg: &Config) -> Self {
        match backend {
            AgentBackend::Claude => Runner::Claude(claude::Runner {
                bin: cfg.claude_bin.clone(),
            }),
            AgentBackend::Codex => Runner::Codex(codex::Runner {
                bin: cfg.codex_bin.clone(),
                model: cfg.codex_model.clone(),
            }),
            AgentBackend::Pi => Runner::Pi(pi::Runner {
                bin: cfg.pi_bin.clone(),
            }),
        }
    }

    pub fn backend(&self) -> AgentBackend {
        match self {
            Runner::Claude(_) => AgentBackend::Claude,
            Runner::Codex(_) => AgentBackend::Codex,
            Runner::Pi(_) => AgentBackend::Pi,
            #[cfg(test)]
            Runner::Fake(r) => r.backend,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Runner::Claude(_) => "Claude",
            Runner::Codex(_) => "Codex",
            Runner::Pi(_) => "Pi",
            #[cfg(test)]
            Runner::Fake(_) => "Fake",
        }
    }

    pub fn initial_session_id(&self) -> String {
        match self {
            Runner::Claude(_) => Uuid::new_v4().to_string(),
            Runner::Codex(_) => String::new(),
            Runner::Pi(_) => String::new(),
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
            Runner::Pi(r) => r.run(req, timeout).await,
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
    pub failure: Option<String>,
    pub resume_missing_once: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub struct FakeRunCall {
    pub session_id: String,
    pub is_new: bool,
    pub prompt: String,
    pub permission: PermissionCapability,
    pub instructions: String,
    pub additional_dirs: Vec<String>,
}

#[cfg(test)]
impl FakeRunner {
    async fn run(&self, req: Request<'_>, _timeout: Duration) -> Result<RunOutput, RunError> {
        self.calls.lock().unwrap().push(FakeRunCall {
            session_id: req.session_id.to_string(),
            is_new: req.is_new,
            prompt: req.prompt.to_string(),
            permission: req.permission,
            instructions: req.instructions.to_string(),
            additional_dirs: req
                .additional_dirs
                .iter()
                .map(|path| (*path).to_string())
                .collect(),
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
        if let Some(message) = &self.failure {
            return Err(RunError::Failed(message.clone()));
        }
        Ok(RunOutput {
            reply: format!("fake reply: {}", req.prompt),
            session_id: req.is_new.then(|| self.session_id.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::Mutex;

    fn fake_spawner(
        outputs: impl Into<VecDeque<std::io::Result<std::process::Output>>>,
    ) -> (
        Mutex<VecDeque<std::io::Result<std::process::Output>>>,
        Mutex<usize>,
    ) {
        (Mutex::new(outputs.into()), Mutex::new(0))
    }

    fn successful_output() -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    #[tokio::test]
    async fn retries_text_file_busy_then_succeeds() {
        let (outputs, calls) = fake_spawner([
            Err(std::io::Error::from_raw_os_error(26)),
            Err(std::io::Error::from_raw_os_error(26)),
            Ok(successful_output()),
        ]);

        output_with_retry(|| {
            *calls.lock().unwrap() += 1;
            std::future::ready(outputs.lock().unwrap().pop_front().expect("fake output"))
        })
        .await
        .unwrap();

        assert_eq!(*calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn stops_after_three_text_file_busy_retries() {
        let (outputs, calls) = fake_spawner(
            (0..4)
                .map(|_| Err(std::io::Error::from_raw_os_error(26)))
                .collect::<VecDeque<_>>(),
        );

        let error = output_with_retry(|| {
            *calls.lock().unwrap() += 1;
            std::future::ready(outputs.lock().unwrap().pop_front().expect("fake output"))
        })
        .await
        .unwrap_err();

        assert_eq!(error.raw_os_error(), Some(26));
        assert_eq!(*calls.lock().unwrap(), 4);
    }

    #[tokio::test]
    async fn does_not_retry_other_spawn_errors() {
        let (outputs, calls) = fake_spawner([Err(std::io::Error::from_raw_os_error(2))]);

        let error = output_with_retry(|| {
            *calls.lock().unwrap() += 1;
            std::future::ready(outputs.lock().unwrap().pop_front().expect("fake output"))
        })
        .await
        .unwrap_err();

        assert_eq!(error.raw_os_error(), Some(2));
        assert_eq!(*calls.lock().unwrap(), 1);
    }
}
