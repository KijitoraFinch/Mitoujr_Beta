use crate::model::{
    Observation, ResourceState, Snapshot, SnapshotSource, Target, digest, require_version,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub sources: Vec<Source>,
    #[serde(default)]
    pub resources: Vec<SnapshotSource>,
    #[serde(default)]
    pub names: BTreeMap<String, Target>,
    #[serde(default)]
    pub extensions: BTreeMap<String, ProcessExtension>,
    #[serde(default)]
    pub comparators: BTreeMap<String, String>,
    #[serde(default)]
    pub checks: Vec<CheckSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSpec {
    pub extension: String,
    #[serde(default)]
    pub options: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub pattern: String,
    pub extension: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessExtension {
    pub command: Vec<String>,
    /// A version chosen by the extension author, retained in interpretation snapshots.
    pub revision: String,
    #[serde(default)]
    pub schemes: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    30_000
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            sources: Vec::new(),
            resources: Vec::new(),
            names: BTreeMap::new(),
            extensions: BTreeMap::new(),
            comparators: BTreeMap::new(),
            checks: Vec::new(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        require_version(self.version)?;
        for (name, extension) in &self.extensions {
            ensure!(!name.is_empty(), "empty extension name");
            ensure!(
                !extension.command.is_empty() && !extension.command[0].is_empty(),
                "extension {name} has no command"
            );
            ensure!(
                !extension.revision.is_empty(),
                "extension {name} has no revision"
            );
            ensure!(
                extension.timeout_ms > 0,
                "extension {name} has a zero timeout"
            );
        }
        for (domain, extension) in &self.comparators {
            ensure!(!domain.is_empty(), "empty comparison domain");
            ensure!(
                self.extensions.contains_key(extension),
                "unknown comparator extension: {extension}"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub config: Config,
}

impl Workspace {
    pub fn open(root: &Path) -> Result<Self> {
        let workspace = Self::for_replay(root)?;
        let root = workspace.root;
        let config_path = root.join("monika.json");
        let config = match fs::read(&config_path) {
            Ok(bytes) => serde_json::from_slice::<Config>(&bytes).context("parse monika.json")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(error) => return Err(error).context("read monika.json"),
        };
        config.validate()?;
        Ok(Self { root, config })
    }

    /// Replay obtains its configuration from the snapshot, independently of
    /// the current monika.json (which may have changed or be under editing).
    pub fn for_replay(root: &Path) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("open workspace {}", root.display()))?;
        ensure!(
            root.is_dir(),
            "workspace is not a directory: {}",
            root.display()
        );
        Ok(Self {
            root,
            config: Config::default(),
        })
    }

    pub fn discover(start: &Path) -> Result<Self> {
        Self::open(&Self::discover_root(start)?)
    }

    pub fn discover_root(start: &Path) -> Result<PathBuf> {
        let start = start.canonicalize()?;
        for path in start.ancestors() {
            if path.join("monika.json").exists() || path.join(".git").exists() {
                return Ok(path.to_owned());
            }
        }
        Ok(start)
    }

    pub fn initialize(&self) -> Result<PathBuf> {
        let path = self.root.join("monika.json");
        ensure!(!path.exists(), "{} already exists", path.display());
        let mut bytes = serde_json::to_vec_pretty(&self.config)?;
        bytes.push(b'\n');
        self.publish_new(&path, &bytes, false)?;
        Ok(path)
    }

    pub fn resource(&self, input: &str) -> Result<String> {
        if let Ok(url) = Url::parse(input) {
            ensure!(
                url.fragment().is_none(),
                "resource URL cannot contain a fragment; use the selector query"
            );
            if url.scheme() != "file" {
                return Ok(url.to_string());
            }
            ensure!(
                url.query().is_none(),
                "file resource URL cannot contain a query"
            );
            let path = url
                .to_file_path()
                .map_err(|_| anyhow::anyhow!("invalid file URL: {input}"))?;
            return self.path_resource(&path);
        }
        self.path_resource(&self.root.join(input))
    }

    fn path_resource(&self, path: &Path) -> Result<String> {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    normalized.pop();
                }
                other => normalized.push(other.as_os_str()),
            }
        }
        Url::from_file_path(&normalized)
            .map(|url| url.to_string())
            .map_err(|_| anyhow::anyhow!("invalid resource path: {}", path.display()))
    }

    pub fn file_path(&self, resource: &str) -> Result<PathBuf> {
        Url::parse(resource)?
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("not a local file resource: {resource}"))
    }

    fn store_dir(&self, kind: &str) -> Result<PathBuf> {
        let path = self.root.join(".monika").join(kind);
        fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
        Ok(path)
    }

    fn store_path(&self, kind: &str, id: &str) -> Result<PathBuf> {
        ensure!(
            id.len() == 64
                && id
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "invalid object ID: {id}"
        );
        Ok(self.root.join(".monika").join(kind).join(id))
    }

    /// Publish only fully written immutable objects. Existing content is verified.
    fn publish_new(&self, path: &Path, bytes: &[u8], allow_identical: bool) -> Result<()> {
        let parent = path.parent().context("object has no parent directory")?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        staged.write_all(bytes)?;
        staged.as_file().sync_all()?;
        match staged.persist_noclobber(path) {
            Ok(_) => Ok(()),
            Err(error)
                if error.error.kind() == std::io::ErrorKind::AlreadyExists && allow_identical =>
            {
                ensure!(
                    fs::read(path)? == bytes,
                    "stored object is corrupt: {}",
                    path.display()
                );
                Ok(())
            }
            Err(error) => Err(error.error).with_context(|| format!("write {}", path.display())),
        }
    }

    pub fn save_observation(&self, observation: &Observation, bytes: &[u8]) -> Result<()> {
        observation.verify(bytes)?;
        let dir = self.store_dir("blobs")?;
        self.publish_new(&dir.join(&observation.content_hash), bytes, true)
    }

    pub fn read_observation(&self, observation: &Observation) -> Result<Vec<u8>> {
        let bytes = fs::read(self.store_path("blobs", &observation.content_hash)?)
            .with_context(|| format!("read saved observation {}", observation.id))?;
        observation.verify(&bytes)?;
        Ok(bytes)
    }

    pub fn blob_path(&self, observation: &Observation) -> Result<PathBuf> {
        self.store_path("blobs", &observation.content_hash)
    }

    pub fn save_snapshot(&self, snapshot: &Snapshot) -> Result<String> {
        let bytes = serde_json::to_vec(snapshot)?;
        let id = digest(&bytes);
        let dir = self.store_dir("snapshots")?;
        self.publish_new(&dir.join(&id), &bytes, true)?;
        Ok(id)
    }

    pub fn read_snapshot(&self, id: &str) -> Result<Snapshot> {
        let bytes = fs::read(self.store_path("snapshots", id)?)
            .with_context(|| format!("read snapshot {id}"))?;
        ensure!(digest(&bytes) == id, "corrupt snapshot {id}");
        let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
        require_version(snapshot.version)?;
        ensure!(
            snapshot.interpreter == env!("CARGO_PKG_VERSION"),
            "snapshot uses a different Monika interpreter version"
        );
        ensure!(
            snapshot.root == self.root.to_string_lossy(),
            "snapshot belongs to a different workspace"
        );
        for (resource, observation) in &snapshot.observations {
            ensure!(
                resource == &observation.resource,
                "snapshot resource mismatch"
            );
            ensure!(
                !snapshot.failures.contains_key(resource),
                "snapshot has both success and failure for {resource}"
            );
        }
        Ok(snapshot)
    }
}

pub fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock predates epoch")
        .as_nanos()
        .to_string()
}

pub struct Session {
    pub workspace: Workspace,
    pub snapshot: Snapshot,
    pub frozen: bool,
    bytes: BTreeMap<String, Vec<u8>>,
}

impl Session {
    pub fn new(workspace: Workspace) -> Result<Self> {
        let snapshot = Snapshot {
            version: 1,
            interpreter: env!("CARGO_PKG_VERSION").into(),
            created_at: timestamp(),
            root: workspace.root.to_string_lossy().into_owned(),
            interpretation: serde_json::to_value(&workspace.config)?,
            observations: BTreeMap::new(),
            failures: BTreeMap::new(),
            sources: None,
            diagnostics: Vec::new(),
            resolution_inputs: BTreeMap::new(),
        };
        Ok(Self {
            workspace,
            snapshot,
            frozen: false,
            bytes: BTreeMap::new(),
        })
    }

    pub fn replay(mut workspace: Workspace, id: &str) -> Result<Self> {
        let snapshot = workspace.read_snapshot(id)?;
        workspace.config = serde_json::from_value(snapshot.interpretation.clone())
            .context("invalid saved interpretation")?;
        workspace.config.validate()?;
        Ok(Self {
            workspace,
            snapshot,
            frozen: true,
            bytes: BTreeMap::new(),
        })
    }

    pub fn record(
        &mut self,
        resource: String,
        bytes: Vec<u8>,
        provider: String,
        state: ResourceState,
    ) -> Result<Observation> {
        ensure!(!self.frozen, "cannot add observations to a saved snapshot");
        ensure!(
            !self.snapshot.observations.contains_key(&resource)
                && !self.snapshot.failures.contains_key(&resource),
            "resource already observed: {resource}"
        );
        let observation = Observation::new(resource.clone(), &bytes, timestamp(), provider, state);
        self.workspace.save_observation(&observation, &bytes)?;
        self.bytes.insert(observation.id.clone(), bytes);
        self.snapshot
            .observations
            .insert(resource, observation.clone());
        Ok(observation)
    }

    pub fn bytes(&mut self, observation: &Observation) -> Result<&[u8]> {
        ensure!(
            self.snapshot.observations.get(&observation.resource) == Some(observation),
            "observation is not in this session"
        );
        if !self.bytes.contains_key(&observation.id) {
            self.bytes.insert(
                observation.id.clone(),
                self.workspace.read_observation(observation)?,
            );
        }
        Ok(&self.bytes[&observation.id])
    }

    pub fn text(&mut self, observation: &Observation) -> Result<&str> {
        std::str::from_utf8(self.bytes(observation)?).context("resource is not UTF-8 text")
    }

    pub fn save(&self) -> Result<String> {
        self.workspace.save_snapshot(&self.snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_observations_are_immutable_and_corruption_is_detected() -> Result<()> {
        let state = || ResourceState {
            exists: true,
            revision: serde_json::Value::Null,
        };
        let dir = tempfile::tempdir()?;
        let mut session = Session::new(Workspace::open(dir.path())?)?;
        let observed = session.record(
            "example:resource".into(),
            b"old".to_vec(),
            "example".into(),
            state(),
        )?;
        assert!(
            session
                .record(
                    "example:resource".into(),
                    b"new".to_vec(),
                    "example".into(),
                    state()
                )
                .is_err()
        );
        let snapshot = session.save()?;
        let mut replay = Session::replay(Workspace::open(dir.path())?, &snapshot)?;
        assert_eq!(replay.bytes(&observed)?, b"old");
        assert!(
            replay
                .record("example:new".into(), vec![], "example".into(), state())
                .is_err()
        );
        fs::write(
            replay
                .workspace
                .store_path("blobs", &observed.content_hash)?,
            "tampered",
        )?;
        assert!(replay.workspace.read_observation(&observed).is_err());
        assert!(replay.workspace.read_snapshot("../../escape").is_err());
        Ok(())
    }
}
