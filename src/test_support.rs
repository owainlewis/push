use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

use crate::agent::{Request, RunError, RunOutput};
use crate::config::PermissionCapability;

pub struct FakeCli {
    root: PathBuf,
    bin: PathBuf,
}

impl FakeCli {
    pub fn new(name: &str, script: &str) -> Self {
        use std::io::Write;

        let root = temp_dir(&format!("fake-{name}"));
        let bin = root.join(name);
        let tmp = root.join(format!("{name}.tmp"));
        {
            let mut file = std::fs::File::create(&tmp).unwrap();
            file.write_all(script.as_bytes()).unwrap();
            file.sync_all().unwrap();
        }
        make_executable(&tmp);
        std::fs::rename(&tmp, &bin).unwrap();
        Self { root, bin }
    }

    pub fn bin(&self) -> String {
        self.bin.to_string_lossy().to_string()
    }

    fn keep_alive(&self) {}
}

impl Drop for FakeCli {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("push-test-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("push-test-{name}-{}", Uuid::new_v4()))
}

pub fn sh_arg(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub struct RunnerContract {
    pub name: &'static str,
    pub new_session: fn() -> ContractCase,
    pub resumed_session: fn() -> ContractCase,
    pub failed_run: fn() -> ContractCase,
    pub timeout_run: fn() -> ContractCase,
}

pub struct ContractCase {
    pub fake_cli: FakeCli,
    pub runner: Box<dyn ContractRunner>,
    pub request: ContractRequest,
    pub timeout: Duration,
}

pub struct ContractRequest {
    pub session_id: String,
    pub is_new: bool,
    pub work_dir: PathBuf,
    pub instructions: String,
    pub permission: PermissionCapability,
    pub prompt: String,
}

impl ContractRequest {
    fn as_request(&self) -> Request<'_> {
        Request {
            session_id: &self.session_id,
            is_new: self.is_new,
            work_dir: self.work_dir.to_str().unwrap(),
            instructions: &self.instructions,
            permission: self.permission,
            prompt: &self.prompt,
        }
    }
}

pub trait ContractRunner {
    fn run<'a>(
        &'a self,
        req: Request<'a>,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RunOutput, RunError>> + 'a>>;
}

pub async fn assert_runner_contract(contract: RunnerContract) {
    let case = (contract.new_session)();
    case.fake_cli.keep_alive();
    let out = case
        .runner
        .run(case.request.as_request(), case.timeout)
        .await
        .unwrap_or_else(|err| panic!("{} new session failed: {err:?}", contract.name));
    assert!(
        !out.reply.trim().is_empty(),
        "{} new session returned an empty reply",
        contract.name
    );

    let case = (contract.resumed_session)();
    case.fake_cli.keep_alive();
    let out = case
        .runner
        .run(case.request.as_request(), case.timeout)
        .await
        .unwrap_or_else(|err| panic!("{} resumed session failed: {err:?}", contract.name));
    assert!(
        !out.reply.trim().is_empty(),
        "{} resumed session returned an empty reply",
        contract.name
    );

    let case = (contract.failed_run)();
    case.fake_cli.keep_alive();
    match case
        .runner
        .run(case.request.as_request(), case.timeout)
        .await
    {
        Err(RunError::Failed(_)) => {}
        Err(RunError::Timeout) => panic!("{} failed run timed out", contract.name),
        Err(RunError::SessionMissing(msg)) => {
            panic!(
                "{} failed run reported missing session: {msg}",
                contract.name
            )
        }
        Ok(_) => panic!("{} failed run succeeded", contract.name),
    }

    let case = (contract.timeout_run)();
    case.fake_cli.keep_alive();
    match case
        .runner
        .run(case.request.as_request(), case.timeout)
        .await
    {
        Err(RunError::Timeout) => {}
        Err(RunError::Failed(msg)) => panic!("{} timeout failed: {msg}", contract.name),
        Err(RunError::SessionMissing(msg)) => {
            panic!("{} timeout reported missing session: {msg}", contract.name)
        }
        Ok(_) => panic!("{} timeout run succeeded", contract.name),
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}
