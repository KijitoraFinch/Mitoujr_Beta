use crate::model::require_version;
use crate::workspace::ProcessExtension;
use anyhow::{Context, Result, bail, ensure};
use futures_util::{StreamExt, future::join_all};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;
use tokio_util::codec::{FramedRead, LinesCodec};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub revision: String,
    pub operations: Vec<String>,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub description: String,
}

impl Manifest {
    pub fn supports(&self, operation: &str) -> bool {
        self.operations
            .iter()
            .any(|capability| capability == operation)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.is_empty() && !self.revision.is_empty(),
            "extension manifest needs a name and revision"
        );
        if self.supports("resolve") {
            ensure!(
                self.supports("dependencies") && self.supports("compare"),
                "a region extension must provide dependencies, resolve, and compare"
            );
            ensure!(
                !self.domains.is_empty(),
                "a region extension must declare its comparison domains"
            );
        }
        if self.supports("check") {
            ensure!(
                self.supports("check_plan"),
                "a checker must provide check_plan and check"
            );
        }
        if self.supports("edit") {
            ensure!(
                self.supports("extract"),
                "an editor must provide extract and edit"
            );
        }
        if self.supports("commit") {
            ensure!(
                self.supports("observe"),
                "a writer must provide observe and commit"
            );
        }
        ensure!(
            self.domains.iter().all(|domain| !domain.is_empty()),
            "empty comparison domain"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Response {
    Success(Success),
    Failure(Failure),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Success {
    protocol: u32,
    id: u64,
    result: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Failure {
    protocol: u32,
    id: u64,
    error: String,
}

/// Owns every extension started by one CLI operation, including historical and
/// post-commit observation sessions. The caller always awaits shutdown.
#[derive(Default)]
pub struct Host {
    workers: BTreeMap<(PathBuf, String, String), Worker>,
}

impl Host {
    pub fn worker(
        &mut self,
        root: &Path,
        name: &str,
        config: &ProcessExtension,
    ) -> Result<&mut Worker> {
        let key = (
            root.to_owned(),
            name.to_owned(),
            serde_json::to_string(config)?,
        );
        if let std::collections::btree_map::Entry::Vacant(entry) = self.workers.entry(key.clone()) {
            entry.insert(Worker::spawn(root, config)?);
        }
        Ok(self.workers.get_mut(&key).expect("registered worker"))
    }

    pub async fn shutdown(&mut self, interrupted: bool) -> Result<()> {
        // Join all shutdowns so one failed extension cannot skip cleanup of others.
        let results = join_all(
            self.workers
                .iter_mut()
                .map(|((_, name, _), worker)| async move {
                    worker
                        .shutdown(interrupted)
                        .await
                        .with_context(|| format!("extension {name}"))
                }),
        )
        .await;
        let errors: Vec<_> = results
            .into_iter()
            .filter_map(Result::err)
            .map(|error| format!("{error:#}"))
            .collect();
        ensure!(
            errors.is_empty(),
            "extension shutdown failed: {}",
            errors.join("; ")
        );
        Ok(())
    }

    pub fn terminate(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        for worker in self.workers.values_mut() {
            if !worker.stopped
                && let Err(error) = worker.start_kill()
            {
                errors.push(error.to_string());
            }
        }
        ensure!(
            errors.is_empty(),
            "terminate extensions: {}",
            errors.join("; ")
        );
        Ok(())
    }
}

pub struct Worker {
    child: Box<dyn ChildWrapper>,
    stdin: Option<ChildStdin>,
    stdout: FramedRead<ChildStdout, LinesCodec>,
    stderr: tempfile::NamedTempFile,
    root: PathBuf,
    timeout: Duration,
    next_id: u64,
    failed: Option<String>,
    stopped: bool,
}

const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

impl Worker {
    pub fn spawn(root: &Path, extension: &ProcessExtension) -> Result<Self> {
        ensure!(!extension.command.is_empty(), "extension command is empty");
        let stderr = tempfile::NamedTempFile::new()?;
        let mut command = Command::new(&extension.command[0]);
        command
            .args(&extension.command[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr.reopen()?));
        let mut command = CommandWrap::from(command);
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(process_wrap::tokio::JobObject);
        let mut child = command
            .spawn()
            .with_context(|| format!("start extension command {:?}", extension.command))?;
        let stdin = child.stdin().take().expect("piped stdin");
        let stdout = child.stdout().take().expect("piped stdout");
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: FramedRead::new(stdout, LinesCodec::new()),
            stderr,
            root: root.to_owned(),
            timeout: Duration::from_millis(extension.timeout_ms),
            next_id: 1,
            failed: None,
            stopped: false,
        })
    }

    pub async fn invoke(&mut self, operation: &str, input: Value) -> Result<Value> {
        if let Some(error) = &self.failed {
            bail!("extension worker is unavailable: {error}");
        }
        ensure!(!self.stopped, "extension worker is stopped");
        ensure!(self.stdin.is_some(), "extension worker is shutting down");
        let id = self.next_id;
        self.next_id += 1;
        let mut request = serde_json::to_vec(&json!({
            "protocol": 1, "id": id, "operation": operation, "workspace": self.root, "input": input,
        }))?;
        request.push(b'\n');
        // Dropping this future can leave a partially sent request or an unread
        // response. Only a validated response permits another request.
        self.failed = Some(format!(
            "extension operation {operation} was cancelled during communication"
        ));
        let response: Result<Response> = async {
            let exchange = async {
                let stdin = self.stdin.as_mut().expect("live worker stdin");
                stdin
                    .write_all(&request)
                    .await
                    .context("send extension request")?;
                let line = self
                    .stdout
                    .next()
                    .await
                    .context("extension closed stdout without a response")?
                    .context("read extension response")?;
                let response: Response =
                    serde_json::from_str(&line).context("invalid extension response")?;
                let (protocol, response_id) = match &response {
                    Response::Success(result) => (result.protocol, result.id),
                    Response::Failure(result) => (result.protocol, result.id),
                };
                require_version(protocol)?;
                ensure!(
                    response_id == id,
                    "extension response ID mismatch: expected {id}, received {response_id}"
                );
                Ok(response)
            };
            timeout(self.timeout, exchange).await.with_context(|| {
                format!(
                    "extension operation {operation} timed out after {} ms",
                    self.timeout.as_millis()
                )
            })?
        }
        .await;
        match response {
            Ok(Response::Success(Success { result, .. })) => {
                self.failed = None;
                Ok(result)
            }
            Ok(Response::Failure(Failure { error, .. })) => {
                self.failed = None;
                bail!("extension operation {operation}: {error}")
            }
            Err(error) => {
                self.failed = Some(format!("{error:#}"));
                let cleanup = self.shutdown(true).await;
                let stderr = tokio::fs::read(self.stderr.path())
                    .await
                    .context("read extension stderr")?;
                let mut message =
                    format!("{error:#}; stderr: {}", String::from_utf8_lossy(&stderr));
                if let Err(error) = cleanup {
                    message.push_str(&format!("; cleanup: {error:#}"));
                }
                self.failed = Some(message.clone());
                bail!(message)
            }
        }
    }

    pub async fn describe(&mut self, name: &str, revision: &str) -> Result<Manifest> {
        let result: Result<Manifest> = async {
            let manifest: Manifest =
                serde_json::from_value(self.invoke("describe", Value::Null).await?)?;
            manifest.validate()?;
            ensure!(
                manifest.name == name,
                "extension name mismatch: configured {name}, received {}",
                manifest.name
            );
            ensure!(
                manifest.revision == revision,
                "extension {name} revision mismatch: expected {revision}, received {}",
                manifest.revision
            );
            Ok(manifest)
        }
        .await;
        if let Err(error) = &result {
            self.failed = Some(format!("{error:#}"));
        }
        result
    }

    fn start_kill(&mut self) -> Result<()> {
        match self.child.start_kill() {
            Ok(()) => Ok(()),
            // The group may already have exited between EOF and cleanup.
            #[cfg(unix)]
            Err(error) if error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) => Ok(()),
            Err(error) => Err(error).context("terminate extension process group"),
        }
    }

    pub async fn shutdown(&mut self, interrupted: bool) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stdin.take();
        let graceful = !interrupted && self.failed.is_none();
        let mut expired = false;
        let status = if graceful {
            match timeout(SHUTDOWN_GRACE, self.child.wait()).await {
                Ok(result) => result.context("wait for extension exit")?,
                Err(_) => {
                    expired = true;
                    self.start_kill()?;
                    timeout(REAP_TIMEOUT, self.child.wait())
                        .await
                        .context("extension did not exit after termination")??
                }
            }
        } else {
            self.start_kill()?;
            timeout(REAP_TIMEOUT, self.child.wait())
                .await
                .context("extension did not exit after termination")??
        };
        // A helper may retain the process group after its leader exits.
        self.start_kill()?;
        self.stopped = true;
        ensure!(
            !expired,
            "extension did not exit within 1 second after stdin EOF"
        );
        if graceful {
            ensure!(status.success(), "extension exited with {status}");
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Last-resort ownership guard for unwinding or a dropped operation.
        // Normal completion uses shutdown().await and reports failures.
        if !self.stopped {
            let _ = self.start_kill();
        }
    }
}
