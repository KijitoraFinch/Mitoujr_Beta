use crate::model::{Observation, ResourceState, Snapshot, Target, digest, require_version};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use url::Url;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
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
    pub async fn open(root: &Path) -> Result<Self> {
        let workspace = Self::for_replay(root).await?;
        let root = workspace.root;
        let config_path = root.join("monika.json");
        let config = match fs::read(&config_path).await {
            Ok(bytes) => serde_json::from_slice::<Config>(&bytes).context("parse monika.json")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(error) => return Err(error).context("read monika.json"),
        };
        config.validate()?;
        Ok(Self { root, config })
    }

    /// Replay obtains its configuration from the snapshot, independently of
    /// the current monika.json (which may have changed or be under editing).
    pub async fn for_replay(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)
            .await
            .with_context(|| format!("open workspace {}", root.display()))?;
        ensure!(
            fs::metadata(&root).await?.is_dir(),
            "workspace is not a directory: {}",
            root.display()
        );
        Ok(Self {
            root,
            config: Config::default(),
        })
    }

    pub async fn discover(start: &Path) -> Result<Self> {
        Self::open(&Self::discover_root(start).await?).await
    }

    pub async fn discover_root(start: &Path) -> Result<PathBuf> {
        let start = fs::canonicalize(start).await?;
        for path in start.ancestors() {
            if fs::try_exists(path.join("monika.json")).await?
                || fs::try_exists(path.join(".git")).await?
            {
                return Ok(path.to_owned());
            }
        }
        Ok(start)
    }

    pub async fn initialize(&self) -> Result<PathBuf> {
        let path = self.root.join("monika.json");
        ensure!(
            !fs::try_exists(&path).await?,
            "{} already exists",
            path.display()
        );
        let mut bytes = serde_json::to_vec_pretty(&self.config)?;
        bytes.push(b'\n');
        ensure!(
            !fs::try_exists(self.root.join(crate::sidecar::FILE_NAME)).await?,
            "Sidecar already exists"
        );
        let sidecar = crate::sidecar::SidecarState {
            content_hash: None,
            data: crate::sidecar::Sidecar::default(),
        };
        self.publish_new(&path, &bytes, false).await?;
        let mut initial = sidecar.data.clone();
        initial.revision = 1;
        crate::sidecar::commit(self, &sidecar, &initial).await?;
        Ok(path)
    }

    pub async fn resource(&self, input: &str) -> Result<String> {
        ensure!(!input.is_empty(), "resource is empty");
        // Recognize native absolute paths before URL parsing (including Windows drives).
        if Path::new(input).has_root() {
            return self.path_resource(Path::new(input)).await;
        }
        let url = match Url::parse(input) {
            Ok(url) => url,
            Err(url::ParseError::RelativeUrlWithoutBase) => {
                return self.path_resource(Path::new(input)).await;
            }
            Err(error) => return Err(error).context("invalid resource URL"),
        };
        ensure!(
            url.fragment().is_none(),
            "resource URL cannot contain a fragment; use the selector query"
        );
        if url.scheme() == "file" {
            ensure!(
                url.query().is_none(),
                "file resource URL cannot contain a query"
            );
            url.to_file_path()
                .map_err(|_| anyhow::anyhow!("invalid file URL: {input}"))?;
        }
        // A saved URI is already an address. Do not consult the live filesystem
        // when it is reused, including after its parent directory has disappeared.
        Ok(url.into())
    }

    pub(crate) async fn path_resource(&self, path: &Path) -> Result<String> {
        let path = self.root.join(path);
        // Resolve parents through the OS: lexically removing `..` changes the
        // destination after a symlink. Keep the leaf so providers can create a
        // missing file or apply their own policy to a symlink at the destination.
        let resolved = match path.file_name() {
            Some(name) => fs::canonicalize(path.parent().context("resource path has no parent")?)
                .await
                .with_context(|| format!("resolve resource parent: {}", path.display()))?
                .join(name),
            None => fs::canonicalize(&path)
                .await
                .with_context(|| format!("resolve resource path: {}", path.display()))?,
        };
        Url::from_file_path(&resolved)
            .map(|url| url.to_string())
            .map_err(|_| anyhow::anyhow!("invalid resource path: {}", path.display()))
    }

    pub fn file_path(&self, resource: &str) -> Result<PathBuf> {
        let url = Url::parse(resource)?;
        ensure!(url.scheme() == "file", "not a file resource: {resource}");
        url.to_file_path()
            .map_err(|_| anyhow::anyhow!("not a local file resource: {resource}"))
    }

    async fn store_dir(&self, kind: &str) -> Result<PathBuf> {
        let path = self.root.join(".monika").join(kind);
        fs::create_dir_all(&path)
            .await
            .with_context(|| format!("create {}", path.display()))?;
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
    pub(crate) async fn publish_new(
        &self,
        path: &Path,
        bytes: &[u8],
        allow_identical: bool,
    ) -> Result<()> {
        let parent = path.parent().context("object has no parent directory")?;
        let staged = tempfile::NamedTempFile::new_in(parent)?;
        let mut output = fs::File::from_std(staged.reopen()?);
        output.write_all(bytes).await?;
        output.sync_all().await?;
        drop(output);
        let destination = path.to_owned();
        match tokio::task::spawn_blocking(move || staged.persist_noclobber(destination)).await? {
            Ok(_) => Ok(()),
            Err(error)
                if error.error.kind() == std::io::ErrorKind::AlreadyExists && allow_identical =>
            {
                ensure!(
                    fs::read(path).await? == bytes,
                    "stored object is corrupt: {}",
                    path.display()
                );
                Ok(())
            }
            Err(error) => Err(error.error).with_context(|| format!("write {}", path.display())),
        }
    }

    pub async fn save_observation(&self, observation: &Observation, bytes: &[u8]) -> Result<()> {
        observation.verify(bytes)?;
        let dir = self.store_dir("blobs").await?;
        self.publish_new(&dir.join(&observation.content_hash), bytes, true)
            .await
    }

    pub async fn read_observation(&self, observation: &Observation) -> Result<Vec<u8>> {
        let bytes = fs::read(self.store_path("blobs", &observation.content_hash)?)
            .await
            .with_context(|| format!("read saved observation {}", observation.id))?;
        observation.verify(&bytes)?;
        Ok(bytes)
    }

    pub fn blob_path(&self, observation: &Observation) -> Result<PathBuf> {
        self.store_path("blobs", &observation.content_hash)
    }

    pub async fn save_snapshot(&self, snapshot: &Snapshot) -> Result<String> {
        let bytes = serde_json::to_vec(snapshot)?;
        let id = digest(&bytes);
        let dir = self.store_dir("snapshots").await?;
        self.publish_new(&dir.join(&id), &bytes, true).await?;
        Ok(id)
    }

    pub async fn read_snapshot(&self, id: &str) -> Result<Snapshot> {
        let bytes = fs::read(self.store_path("snapshots", id)?)
            .await
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
        if let Some(sidecar) = &snapshot.sidecar {
            sidecar.data.validate()?;
        }
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
        for resources in snapshot.resolution_reads.values() {
            for resource in resources {
                ensure!(
                    snapshot.observations.contains_key(resource)
                        || snapshot.failures.contains_key(resource),
                    "resolution refers to an unrecorded resource: {resource}"
                );
            }
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
            sidecar: None,
            resolution_reads: BTreeMap::new(),
        };
        Ok(Self {
            workspace,
            snapshot,
            frozen: false,
            bytes: BTreeMap::new(),
        })
    }

    pub async fn replay(mut workspace: Workspace, id: &str) -> Result<Self> {
        let snapshot = workspace.read_snapshot(id).await?;
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

    pub async fn record(
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
        self.workspace
            .save_observation(&observation, &bytes)
            .await?;
        self.bytes.insert(observation.id.clone(), bytes);
        self.snapshot
            .observations
            .insert(resource, observation.clone());
        Ok(observation)
    }

    pub async fn bytes(&mut self, observation: &Observation) -> Result<&[u8]> {
        ensure!(
            self.snapshot.observations.get(&observation.resource) == Some(observation),
            "observation is not in this session"
        );
        if !self.bytes.contains_key(&observation.id) {
            self.bytes.insert(
                observation.id.clone(),
                self.workspace.read_observation(observation).await?,
            );
        }
        Ok(&self.bytes[&observation.id])
    }

    pub async fn text(&mut self, observation: &Observation) -> Result<&str> {
        std::str::from_utf8(self.bytes(observation).await?).context("resource is not UTF-8 text")
    }

    pub async fn save(&self) -> Result<String> {
        self.workspace.save_snapshot(&self.snapshot).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resource_addresses_use_native_paths_and_url_encoding() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let workspace = Workspace::open(dir.path()).await?;
        let parent = workspace.root.join("資料");
        fs::create_dir(&parent).await?;
        for name in ["new file.links", "日本語#100%?.links", "colon:name.links"] {
            let path = parent.join(name);
            let relative = Path::new("資料").join(name);
            let resource = workspace.resource(relative.to_str().unwrap()).await?;
            assert_eq!(workspace.file_path(&resource)?, path);
            assert_eq!(workspace.resource(path.to_str().unwrap()).await?, resource);
            assert_eq!(workspace.resource(&resource).await?, resource);
        }
        let saved = workspace.resource("資料/new.links").await?;
        fs::remove_dir(&parent).await?;
        assert_eq!(workspace.resource(&saved).await?, saved);
        // A missing directory must not be erased to select a different file.
        assert!(workspace.resource("absent/../victim.links").await.is_err());
        assert!(workspace.resource("").await.is_err());
        assert!(workspace.resource("https://[").await.is_err());
        assert!(workspace.resource("file:///tmp/a?query").await.is_err());
        assert!(workspace.resource("file:///tmp/a#fragment").await.is_err());
        assert!(workspace.file_path("https://localhost/a").is_err());
        assert_eq!(
            workspace.resource("fixture:example").await?,
            "fixture:example"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_uri_preserves_non_utf8_path_bytes() -> Result<()> {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir()?;
        let workspace = Workspace::open(dir.path()).await?;
        let name = std::ffi::OsString::from_vec(b"note-\xff.links".to_vec());
        let resource = workspace.path_resource(Path::new(&name)).await?;
        assert!(resource.contains("%FF"));
        assert_eq!(workspace.file_path(&resource)?, workspace.root.join(name));
        assert_eq!(workspace.resource(&resource).await?, resource);
        Ok(())
    }

    #[tokio::test]
    async fn stored_observations_are_immutable_and_corruption_is_detected() -> Result<()> {
        let state = || ResourceState {
            exists: true,
            revision: serde_json::Value::Null,
        };
        let dir = tempfile::tempdir()?;
        let mut session = Session::new(Workspace::open(dir.path()).await?)?;
        let observed = session
            .record(
                "example:resource".into(),
                b"old".to_vec(),
                "example".into(),
                state(),
            )
            .await?;
        assert!(
            session
                .record(
                    "example:resource".into(),
                    b"new".to_vec(),
                    "example".into(),
                    state()
                )
                .await
                .is_err()
        );
        let snapshot = session.save().await?;
        let mut replay = Session::replay(Workspace::open(dir.path()).await?, &snapshot).await?;
        assert_eq!(replay.bytes(&observed).await?, b"old");
        assert!(
            replay
                .record("example:new".into(), vec![], "example".into(), state())
                .await
                .is_err()
        );
        fs::write(
            replay
                .workspace
                .store_path("blobs", &observed.content_hash)?,
            "tampered",
        )
        .await?;
        assert!(replay.workspace.read_observation(&observed).await.is_err());
        assert!(
            replay
                .workspace
                .read_snapshot("../../escape")
                .await
                .is_err()
        );
        Ok(())
    }
}
