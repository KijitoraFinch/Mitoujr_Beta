use crate::model::{Observation, require_version};
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
use tokio::time::{Instant, timeout, timeout_at};
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
                self.supports("compare"),
                "a region extension must provide resolve and compare"
            );
            ensure!(
                !self.domains.is_empty(),
                "a region extension must declare its comparison domains"
            );
        }
        ensure!(
            self.operations
                .iter()
                .all(|op| matches!(op.as_str(), "observe" | "resolve" | "compare" | "check")),
            "unsupported extension operation in manifest"
        );
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
    Read(ReadRequest),
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    protocol: u32,
    id: u64,
    read: Vec<String>,
}

pub(crate) enum ResolutionEvent {
    Read(Vec<String>),
    Complete(Value),
}

enum State {
    Ready,
    Resolving { id: u64, deadline: Instant },
    Unavailable(String),
}

/// Owns the extension processes and their input files for one CLI operation.
/// The caller always awaits shutdown before the input files are removed.
#[derive(Default)]
pub struct Host {
    workers: BTreeMap<(PathBuf, String, String), Worker>,
    inputs: BTreeMap<String, tempfile::NamedTempFile>,
    outputs: Vec<tempfile::NamedTempFile>,
}

impl Host {
    pub(crate) fn observation_output(&mut self) -> Result<PathBuf> {
        let file = tempfile::Builder::new()
            .prefix("monika-observe-")
            .tempfile()?;
        let path = file.path().to_owned();
        self.outputs.push(file);
        Ok(path)
    }

    pub(crate) async fn observation_input(
        &mut self,
        observation: &Observation,
        bytes: &[u8],
    ) -> Result<Value> {
        if !self.inputs.contains_key(&observation.id) {
            observation.verify(bytes)?;
            let file = tempfile::Builder::new()
                .prefix("monika-observation-")
                .tempfile()?;
            let mut output = tokio::fs::File::from_std(file.reopen()?);
            output.write_all(bytes).await?;
            output.flush().await?;
            self.inputs.insert(observation.id.clone(), file);
        }
        Ok(json!({"observation": observation, "content_path": self.inputs[&observation.id].path()}))
    }

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
        let mut errors: Vec<_> = results
            .into_iter()
            .filter_map(Result::err)
            .map(|error| format!("{error:#}"))
            .collect();
        for file in std::mem::take(&mut self.inputs)
            .into_values()
            .chain(std::mem::take(&mut self.outputs))
        {
            if let Err(error) = file.close() {
                errors.push(format!("remove Observation input: {error}"));
            }
        }
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
    state: State,
    termination_started: bool,
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
            state: State::Ready,
            termination_started: false,
            stopped: false,
        })
    }

    fn ready(&self) -> Result<()> {
        self.available()?;
        ensure!(
            matches!(self.state, State::Ready),
            "extension has a pending resolution"
        );
        Ok(())
    }

    fn available(&self) -> Result<()> {
        if let State::Unavailable(error) = &self.state {
            bail!("extension worker is unavailable: {error}");
        }
        ensure!(!self.stopped, "extension worker is stopped");
        ensure!(self.stdin.is_some(), "extension worker is shutting down");
        Ok(())
    }

    fn request(&mut self, operation: &str, input: Value) -> (u64, Value) {
        let id = self.next_id;
        self.next_id += 1;
        (
            id,
            json!({
                "protocol": 1, "id": id, "operation": operation, "workspace": self.root, "input": input,
            }),
        )
    }

    pub async fn invoke(&mut self, operation: &str, input: Value) -> Result<Value> {
        self.ready()?;
        self.invoke_inner(operation, input).await
    }

    // A resolver awaiting a read must also serve observe when it is the provider.
    pub(crate) async fn observe(&mut self, resource: &str, content_path: &Path) -> Result<Value> {
        self.available()?;
        self.invoke_inner(
            "observe",
            json!({"resource": resource, "content_path": content_path}),
        )
        .await
    }

    async fn invoke_inner(&mut self, operation: &str, input: Value) -> Result<Value> {
        let (id, request) = self.request(operation, input);
        let mut deadline = Instant::now() + self.timeout;
        if let State::Resolving {
            deadline: outer, ..
        } = self.state
        {
            deadline = deadline.min(outer);
        }
        match self
            .exchange(operation, request, id, deadline, false)
            .await?
        {
            Response::Success(result) => Ok(result.result),
            Response::Failure(error) => bail!("extension operation {operation}: {}", error.error),
            Response::Read(_) => unreachable!("read requests are rejected outside resolve"),
        }
    }

    pub(crate) async fn resolve(&mut self, selector: Value) -> Result<ResolutionEvent> {
        self.ready()?;
        let (id, request) = self.request("resolve", json!({"selector": selector}));
        let deadline = Instant::now() + self.timeout;
        self.state = State::Resolving { id, deadline };
        let response = self
            .exchange("resolve", request, id, deadline, true)
            .await?;
        self.resolution_event(response)
    }

    pub(crate) async fn resume(
        &mut self,
        observations: std::result::Result<Vec<Value>, String>,
    ) -> Result<ResolutionEvent> {
        self.available()?;
        let State::Resolving { id, deadline } = self.state else {
            bail!("extension has no pending observation request");
        };
        let response = match observations {
            Ok(observations) => json!({"protocol": 1, "id": id, "read_result": observations}),
            Err(error) => json!({"protocol": 1, "id": id, "read_error": error}),
        };
        let response = self
            .exchange("resolve", response, id, deadline, true)
            .await?;
        self.resolution_event(response)
    }

    fn resolution_event(&mut self, response: Response) -> Result<ResolutionEvent> {
        match response {
            Response::Read(request) => Ok(ResolutionEvent::Read(request.read)),
            Response::Success(response) => {
                self.state = State::Ready;
                Ok(ResolutionEvent::Complete(response.result))
            }
            Response::Failure(response) => {
                self.state = State::Ready;
                bail!("extension operation resolve: {}", response.error)
            }
        }
    }

    async fn exchange(
        &mut self,
        operation: &str,
        message: Value,
        id: u64,
        deadline: Instant,
        allow_reads: bool,
    ) -> Result<Response> {
        let mut request = serde_json::to_vec(&message)?;
        request.push(b'\n');
        // Only a complete, validated frame restores the previous state. Dropping
        // this future leaves the worker unavailable, including in a nested call.
        let previous = std::mem::replace(
            &mut self.state,
            State::Unavailable(format!(
                "extension operation {operation} was cancelled during communication"
            )),
        );
        let response = timeout_at(deadline, async {
            self.stdin
                .as_mut()
                .expect("live worker stdin")
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
                Response::Read(result) => (result.protocol, result.id),
            };
            require_version(protocol)?;
            ensure!(
                response_id == id,
                "extension response ID mismatch: expected {id}, received {response_id}"
            );
            ensure!(
                allow_reads || !matches!(response, Response::Read(_)),
                "observation requests are only valid during resolve"
            );
            Ok::<_, anyhow::Error>(response)
        })
        .await
        .with_context(|| format!("extension operation {operation} timed out"));
        match response.and_then(|response| response) {
            Ok(response) => {
                self.state = previous;
                Ok(response)
            }
            Err(error) => Err(self.invalidate(error).await),
        }
    }

    pub(crate) async fn invalidate(&mut self, error: anyhow::Error) -> anyhow::Error {
        self.state = State::Unavailable(format!("{error:#}"));
        let cleanup = self.shutdown(true).await;
        let mut message = format!("{error:#}");
        match tokio::fs::read(self.stderr.path()).await {
            Ok(stderr) => {
                message.push_str(&format!("; stderr: {}", String::from_utf8_lossy(&stderr)))
            }
            Err(error) => message.push_str(&format!("; read extension stderr: {error}")),
        }
        if let Err(error) = cleanup {
            message.push_str(&format!("; cleanup: {error:#}"));
        }
        self.state = State::Unavailable(message.clone());
        anyhow::anyhow!(message)
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
            self.state = State::Unavailable(format!("{error:#}"));
        }
        result
    }

    fn start_kill(&mut self) -> Result<()> {
        // Cleanup can be cancelled by the enclosing operation's deadline and
        // resumed by the host. Signal the owned group once, then only reap it.
        if self.termination_started {
            return Ok(());
        }
        match self.child.start_kill() {
            Ok(()) => Ok(()),
            // The group may already have exited between EOF and cleanup.
            #[cfg(unix)]
            Err(error) if error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) => Ok(()),
            Err(error) => Err(error).context("terminate extension process group"),
        }?;
        self.termination_started = true;
        Ok(())
    }

    pub async fn shutdown(&mut self, interrupted: bool) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stdin.take();
        let graceful = !interrupted && matches!(self.state, State::Ready);
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
