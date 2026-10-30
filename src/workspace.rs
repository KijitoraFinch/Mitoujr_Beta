use crate::model::{Diagnostic, Observation, ResourceState, Target, require_version};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
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
    #[serde(default)]
    pub resources: BTreeMap<String, ObservationPolicy>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationPolicy {
    #[default]
    Observe,
    Refreshed,
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
    /// A version chosen by the extension author and checked against its manifest.
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
            resources: BTreeMap::new(),
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
        let root = lexical_path(&std::env::current_dir()?.join(root))?;
        ensure!(
            fs::metadata(&root).await?.is_dir(),
            "workspace is not a directory: {}",
            root.display()
        );
        let config_path = root.join("monika.json");
        let config = match fs::read(&config_path).await {
            Ok(bytes) => serde_json::from_slice::<Config>(&bytes).context("parse monika.json")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(error) => return Err(error).context("read monika.json"),
        };
        config.validate()?;
        let mut workspace = Self { root, config };
        let mut resources = BTreeMap::new();
        for (resource, policy) in &workspace.config.resources {
            let resource = workspace.normalize_resource(resource).await?;
            ensure!(
                resources.insert(resource.clone(), *policy).is_none(),
                "duplicate Resource policy after normalization: {resource}"
            );
        }
        workspace.config.resources = resources;
        Ok(workspace)
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
        let sidecar = serde_json::to_vec_pretty(&crate::sidecar::Sidecar::default())?;
        self.publish_new(&path, &bytes).await?;
        self.publish_new(&self.root.join(crate::sidecar::FILE_NAME), &sidecar)
            .await?;
        Ok(path)
    }

    pub async fn normalize_resource(&self, input: &str) -> Result<String> {
        ensure!(!input.is_empty(), "resource is empty");
        // Recognize native absolute paths before URL parsing (including Windows drives).
        if Path::new(input).has_root() {
            return self.normalize_path(Path::new(input)).await;
        }
        let url = match Url::parse(input) {
            Ok(url) => url,
            Err(url::ParseError::RelativeUrlWithoutBase) => {
                return self.normalize_path(Path::new(input)).await;
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
            let path = url
                .to_file_path()
                .map_err(|_| anyhow::anyhow!("invalid file URL: {input}"))?;
            return self.normalize_path(&path).await;
        }
        Ok(url.into())
    }

    pub(crate) async fn normalize_path(&self, path: &Path) -> Result<String> {
        let absolute = lexical_path(&self.root.join(path))?;
        let relative = absolute.strip_prefix(&self.root).with_context(|| {
            format!(
                "Resource path is outside the project root: {}",
                path.display()
            )
        })?;
        let text = relative
            .to_str()
            .context("Resource path must be valid UTF-8")?;
        if text.is_empty() {
            return Ok(".".into());
        }
        let text = text.replace(std::path::MAIN_SEPARATOR, "/");
        // A colon in the first component must remain a local path on reuse.
        Ok(if text.split('/').next().unwrap().contains(':') {
            format!("./{text}")
        } else {
            text
        })
    }

    pub fn observation_policy(&self, resource: &str) -> ObservationPolicy {
        self.config
            .resources
            .get(resource)
            .copied()
            .unwrap_or_default()
    }

    /// Only the observation provider receives the machine-specific file URI.
    pub fn observation_uri(&self, resource: &str) -> Result<Url> {
        if let Ok(url) = Url::parse(resource) {
            return Ok(url);
        }
        Url::from_file_path(self.root.join(resource))
            .map_err(|_| anyhow::anyhow!("invalid Resource path: {resource}"))
    }

    pub fn file_path(&self, resource: &str) -> Result<PathBuf> {
        let url = self.observation_uri(resource)?;
        ensure!(url.scheme() == "file", "not a file resource: {resource}");
        url.to_file_path()
            .map_err(|_| anyhow::anyhow!("not a local file resource: {resource}"))
    }

    pub(crate) async fn replace(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let staged = tempfile::NamedTempFile::new_in(path.parent().context("file has no parent")?)?;
        match fs::symlink_metadata(path).await {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file() && !metadata.is_symlink(),
                    "expected a regular file: {}",
                    path.display()
                );
                staged.as_file().set_permissions(metadata.permissions())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut output = fs::File::from_std(staged.reopen()?);
        output.write_all(bytes).await?;
        output.sync_all().await?;
        drop(output);
        let destination = path.to_owned();
        tokio::task::spawn_blocking(move || staged.persist(destination)).await??;
        Ok(())
    }

    /// Publish a fully written file without replacing an existing destination.
    pub(crate) async fn publish_new(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let parent = path.parent().context("object has no parent directory")?;
        let staged = tempfile::NamedTempFile::new_in(parent)?;
        let mut output = fs::File::from_std(staged.reopen()?);
        output.write_all(bytes).await?;
        output.sync_all().await?;
        drop(output);
        let destination = path.to_owned();
        tokio::task::spawn_blocking(move || staged.persist_noclobber(destination))
            .await?
            .map_err(|error| error.error)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

fn lexical_path(path: &Path) -> Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(result.pop(), "path escapes its root: {}", path.display());
            }
            part => result.push(part.as_os_str()),
        }
    }
    Ok(result)
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
    pub observations: BTreeMap<String, Observation>,
    pub failures: BTreeMap<String, Diagnostic>,
    pub(crate) sidecar: Option<crate::sidecar::SidecarState>,
    pub(crate) saved_current: Option<BTreeMap<String, String>>,
    bytes: BTreeMap<String, Vec<u8>>,
    observation_resources: BTreeMap<String, String>,
}

impl Session {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            observations: BTreeMap::new(),
            failures: BTreeMap::new(),
            sidecar: None,
            saved_current: None,
            bytes: BTreeMap::new(),
            observation_resources: BTreeMap::new(),
        }
    }

    pub fn record(
        &mut self,
        resource: String,
        bytes: Vec<u8>,
        provider: String,
        state: ResourceState,
    ) -> Result<Observation> {
        ensure!(
            !self.observations.contains_key(&resource) && !self.failures.contains_key(&resource),
            "resource already observed: {resource}"
        );
        let observation = Observation::new(resource.clone(), &bytes, timestamp(), provider, state);
        observation.verify(&bytes)?;
        self.adopt(observation, bytes)
    }

    pub fn adopt(&mut self, observation: Observation, bytes: Vec<u8>) -> Result<Observation> {
        ensure!(
            !self.observations.contains_key(&observation.resource)
                && !self.failures.contains_key(&observation.resource),
            "Resource already has a current Observation: {}",
            observation.resource
        );
        observation.verify(&bytes)?;
        self.bytes.insert(observation.id.clone(), bytes);
        self.observation_resources
            .insert(observation.id.clone(), observation.resource.clone());
        self.observations
            .insert(observation.resource.clone(), observation.clone());
        Ok(observation)
    }

    pub fn observation(&self, id: &str) -> &Observation {
        &self.observations[&self.observation_resources[id]]
    }

    pub fn bytes(&self, observation: &Observation) -> Result<&[u8]> {
        ensure!(
            self.observations.get(&observation.resource) == Some(observation),
            "observation is not in this session"
        );
        Ok(&self.bytes[&observation.id])
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
            let resource = workspace
                .normalize_resource(relative.to_str().unwrap())
                .await?;
            assert_eq!(workspace.file_path(&resource)?, path);
            assert_eq!(
                workspace.normalize_resource(path.to_str().unwrap()).await?,
                resource
            );
            assert_eq!(workspace.normalize_resource(&resource).await?, resource);
        }
        let saved = workspace.normalize_resource("資料/new.links").await?;
        fs::remove_dir(&parent).await?;
        assert_eq!(workspace.normalize_resource(&saved).await?, saved);
        // Normalization is lexical even when the directory does not exist.
        assert!(
            workspace
                .normalize_resource("absent/../victim.links")
                .await
                .is_ok()
        );
        assert!(workspace.normalize_resource("").await.is_err());
        assert!(workspace.normalize_resource("https://[").await.is_err());
        assert!(
            workspace
                .normalize_resource("file:///tmp/a?query")
                .await
                .is_err()
        );
        assert!(
            workspace
                .normalize_resource("file:///tmp/a#fragment")
                .await
                .is_err()
        );
        assert!(workspace.file_path("https://localhost/a").is_err());
        assert_eq!(
            workspace.normalize_resource("fixture:example").await?,
            "fixture:example"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn normalization_is_lexical_across_symlinks_and_rejects_outside_paths() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let workspace = Workspace::open(dir.path()).await?;
        std::os::unix::fs::symlink(outside.path(), workspace.root.join("alias"))?;
        assert_eq!(
            workspace.normalize_resource("alias/../rows.json").await?,
            "rows.json"
        );
        assert_eq!(
            workspace.normalize_resource("alias/missing.json").await?,
            "alias/missing.json"
        );
        assert!(
            workspace
                .normalize_resource("../outside.json")
                .await
                .is_err()
        );
        assert!(
            workspace
                .normalize_resource(outside.path().to_str().unwrap())
                .await
                .is_err()
        );
        let path = workspace.root.join("資料 #?%/rows.json");
        let uri = Url::from_file_path(path).unwrap();
        assert_eq!(
            workspace.normalize_resource(uri.as_str()).await?,
            "資料 #?%/rows.json"
        );
        assert_eq!(
            workspace.normalize_resource("./colon:name").await?,
            "./colon:name"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_explicit_workspace_root_keeps_its_lexical_path() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::create_dir(dir.path().join("actual")).await?;
        std::os::unix::fs::symlink(dir.path().join("actual"), dir.path().join("alias"))?;
        let root = dir.path().join("alias");
        let workspace = Workspace::open(&root).await?;
        assert_eq!(workspace.root, root);
        assert_eq!(
            workspace
                .normalize_resource(root.join("missing/file").to_str().unwrap())
                .await?,
            "missing/file"
        );
        Ok(())
    }

    #[tokio::test]
    async fn observations_are_fixed_inputs_owned_by_the_session() -> Result<()> {
        let state = || ResourceState {
            exists: true,
            revision: serde_json::Value::Null,
        };
        let dir = tempfile::tempdir()?;
        let workspace = Workspace::open(dir.path()).await?;
        let mut session = Session::new(workspace.clone());
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
        assert_eq!(session.bytes(&observed)?, b"old");
        let mut invalid = observed.clone();
        invalid.size += 1;
        assert!(session.bytes(&invalid).is_err());
        assert!(
            session
                .record(
                    "example:absent".into(),
                    b"unexpected".to_vec(),
                    "example".into(),
                    ResourceState {
                        exists: false,
                        revision: serde_json::Value::Null
                    }
                )
                .is_err()
        );
        let mut next = Session::new(workspace);
        assert!(next.bytes(&observed).is_err());
        let current = next.record(
            "example:resource".into(),
            b"new".to_vec(),
            "example".into(),
            state(),
        )?;
        assert_eq!(next.bytes(&current)?, b"new");
        assert!(!dir.path().join(".monika").exists());
        Ok(())
    }
}
