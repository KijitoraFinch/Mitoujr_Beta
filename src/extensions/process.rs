use crate::model::require_version;
use crate::workspace::ProcessExtension;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

/// A worker lasts for one Core session. The I/O thread keeps both a blocked
/// write and a blocked read inside the same request deadline.
pub struct Worker {
    child: Child,
    requests: Option<Sender<Vec<u8>>>,
    replies: Receiver<std::io::Result<Vec<u8>>>,
    io_thread: Option<JoinHandle<()>>,
    stderr: tempfile::NamedTempFile,
    root: PathBuf,
    timeout: Duration,
    next_id: u64,
    failed: Option<String>,
    stopped: bool,
}

impl Worker {
    pub fn spawn(root: &Path, extension: &ProcessExtension) -> Result<Self> {
        crate::cancellation::check()?;
        ensure!(!extension.command.is_empty(), "extension command is empty");
        let stderr = tempfile::NamedTempFile::new()?;
        let mut command = Command::new(&extension.command[0]);
        command
            .args(&extension.command[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr.reopen()?));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("start extension command {:?}", extension.command))?;
        let mut stdin = BufWriter::new(child.stdin.take().expect("piped stdin"));
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let (requests, jobs) = mpsc::channel::<Vec<u8>>();
        let (responses, replies) = mpsc::channel();
        let io_thread = std::thread::spawn(move || {
            for request in jobs {
                let response = (|| -> std::io::Result<Vec<u8>> {
                    stdin.write_all(&request)?;
                    stdin.flush()?;
                    let mut line = Vec::new();
                    stdout.read_until(b'\n', &mut line)?;
                    if line.last() != Some(&b'\n') {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "extension closed stdout without a complete response",
                        ));
                    }
                    Ok(line)
                })();
                let failed = response.is_err();
                if responses.send(response).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            requests: Some(requests),
            replies,
            io_thread: Some(io_thread),
            stderr,
            root: root.to_owned(),
            timeout: Duration::from_millis(extension.timeout_ms),
            next_id: 1,
            failed: None,
            stopped: false,
        })
    }

    pub fn invoke(&mut self, operation: &str, input: Value) -> Result<Value> {
        crate::cancellation::check()?;
        if let Some(error) = &self.failed {
            bail!("extension worker is unavailable: {error}");
        }
        let id = self.next_id;
        self.next_id += 1;
        let response = (|| -> Result<Response> {
            if let Some(status) = self.child.try_wait()? {
                bail!("extension exited with {status}");
            }
            let mut request = serde_json::to_vec(&json!({
                "protocol": 1, "id": id, "operation": operation, "workspace": self.root, "input": input,
            }))?;
            request.push(b'\n');
            self.requests
                .as_ref()
                .expect("live worker sender")
                .send(request)
                .context("extension input channel closed")?;
            let started = Instant::now();
            let bytes = loop {
                crate::cancellation::check()?;
                let remaining = self.timeout.saturating_sub(started.elapsed());
                ensure!(
                    !remaining.is_zero(),
                    "extension operation {operation} timed out after {} ms",
                    self.timeout.as_millis()
                );
                match self
                    .replies
                    .recv_timeout(remaining.min(Duration::from_millis(25)))
                {
                    Ok(result) => break result.context("extension I/O failed")?,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        bail!("extension output channel closed")
                    }
                }
            };
            let response: Response =
                serde_json::from_slice(&bytes).context("invalid extension response")?;
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
        })();
        match response {
            Ok(Response::Success(Success { result, .. })) => Ok(result),
            Ok(Response::Failure(Failure { error, .. })) => {
                bail!("extension operation {operation}: {error}")
            }
            Err(error) => {
                let stderr = fs::read(self.stderr.path()).context("read extension stderr")?;
                let message = format!("{error:#}; stderr: {}", String::from_utf8_lossy(&stderr));
                self.failed = Some(message.clone());
                self.stop();
                bail!(message)
            }
        }
    }

    pub fn describe(&mut self, name: &str, revision: &str) -> Result<Manifest> {
        let manifest: Manifest = serde_json::from_value(self.invoke("describe", Value::Null)?)?;
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

    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.requests.take();
        #[cfg(unix)]
        {
            // SAFETY: process_group(0) created a separate group whose ID is the
            // child PID. The negative PID addresses only that extension group.
            unsafe {
                libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
            }
        }
        // Cleanup also runs while unwinding or when the extension already exited.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self
            .io_thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            let _ = self.io_thread.take().expect("present thread").join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn invoke(
    root: &Path,
    extension: &ProcessExtension,
    operation: &str,
    input: Value,
) -> Result<Value> {
    Worker::spawn(root, extension)?.invoke(operation, input)
}

pub fn describe(root: &Path, name: &str, extension: &ProcessExtension) -> Result<Manifest> {
    Worker::spawn(root, extension)?.describe(name, &extension.revision)
}
