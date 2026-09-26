use crate::model::{Link, Observation, Target, digest, require_version};
use crate::workspace::{Session, Workspace};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

pub const FILE_NAME: &str = "monika.links.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub link: String,
    pub observation: String,
    pub extension: String,
    pub revision: String,
    pub location: Value,
}

impl Origin {
    pub fn input_key(&self, observation: &Observation) -> Result<String> {
        Ok(serde_json::to_string(&(
            &self.link,
            &self.extension,
            &self.revision,
            &self.location,
            &observation.resource,
            &observation.content_hash,
            &observation.state,
        ))?)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sidecar {
    pub version: u32,
    pub revision: u64,
    pub links: Vec<Link>,
    pub origins: Vec<Origin>,
    #[serde(default)]
    pub observations: BTreeMap<String, Observation>,
}

impl Default for Sidecar {
    fn default() -> Self {
        Self {
            version: 1,
            revision: 0,
            links: Vec::new(),
            origins: Vec::new(),
            observations: BTreeMap::new(),
        }
    }
}

impl Sidecar {
    pub fn validate(&self) -> Result<()> {
        require_version(self.version)?;
        let mut ids = BTreeSet::new();
        let mut values = BTreeSet::new();
        for link in &self.links {
            link.validate()?;
            ensure!(ids.insert(&link.id), "duplicate Link ID: {}", link.id);
            ensure!(
                matches!(link.reference.target, Target::RegionSelector(_)),
                "Sidecar must contain expanded RegionSelectors: {}",
                link.id
            );
            ensure!(
                values.insert(definition_key(link)?),
                "duplicate Link definition: {}; use the existing Link",
                link.id
            );
        }
        let mut origins = BTreeSet::new();
        for (id, observation) in &self.observations {
            ensure!(id == &observation.id, "Sidecar Observation ID mismatch");
            observation.validate_metadata()?;
        }
        for origin in &self.origins {
            ensure!(
                self.observations.contains_key(&origin.observation),
                "origin refers to missing Observation: {}",
                origin.observation
            );
            ensure!(
                ids.contains(&origin.link),
                "origin refers to missing Link: {}",
                origin.link
            );
            ensure!(
                !origin.extension.is_empty() && !origin.revision.is_empty(),
                "origin needs an extension and revision"
            );
            ensure!(
                origins.insert(origin.input_key(&self.observations[&origin.observation])?),
                "duplicate origin for {}",
                origin.link
            );
        }
        let used: BTreeSet<_> = self
            .origins
            .iter()
            .map(|origin| &origin.observation)
            .collect();
        ensure!(
            used.len() == self.observations.len(),
            "unreferenced Observation metadata in Sidecar"
        );
        Ok(())
    }

    pub fn order(&mut self) {
        let used: BTreeSet<_> = self
            .origins
            .iter()
            .map(|origin| &origin.observation)
            .collect();
        self.observations.retain(|id, _| used.contains(id));
        self.links.sort_by(|a, b| a.id.cmp(&b.id));
        self.origins
            .sort_by_key(|origin| serde_json::to_string(origin).expect("serializable origin"));
    }
}

pub fn definition_key(link: &Link) -> Result<String> {
    Ok(serde_json::to_string(&(
        &link.source,
        &link.reference,
        &link.annotation,
    ))?)
}

pub async fn normalize(workspace: &Workspace, mut link: Link) -> Result<Link> {
    link.validate()?;
    let mut target = link.reference.target.expand(&workspace.config.names)?;
    for selector in [&mut link.source, &mut target] {
        if let Some(resource) = &selector.resource {
            selector.resource = Some(workspace.resource(resource).await?);
        }
    }
    link.reference.target = Target::RegionSelector(target);
    Ok(link)
}

/// The dedicated Sidecar state is saved independently of Resource Observations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarState {
    pub content_hash: Option<String>,
    pub data: Sidecar,
}

pub async fn read(workspace: &Workspace) -> Result<SidecarState> {
    let path = workspace.root.join(FILE_NAME);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let data: Sidecar =
                serde_json::from_slice(&bytes).context("parse monika.links.json")?;
            data.validate()?;
            for link in &data.links {
                ensure!(
                    normalize(workspace, link.clone()).await? == *link,
                    "Sidecar contains an unnormalized Link: {}",
                    link.id
                );
            }
            Ok(SidecarState {
                content_hash: Some(digest(&bytes)),
                data,
            })
        }
        Err(error) => Err(error).context("read monika.links.json; initialize a new workspace with monika init, or restore its Sidecar"),
    }
}

impl Session {
    pub async fn sidecar(&mut self) -> Result<&SidecarState> {
        if self.snapshot.sidecar.is_none() {
            ensure!(!self.frozen, "Sidecar was not read in this snapshot");
            self.snapshot.sidecar = Some(read(&self.workspace).await?);
        }
        Ok(self.snapshot.sidecar.as_ref().expect("Sidecar loaded"))
    }
}

/// Compare and replace under the standard library's OS lock. The temporary file
/// is persisted atomically; the lock file itself stays in place between writes.
pub async fn commit(workspace: &Workspace, before: &SidecarState, after: &Sidecar) -> Result<()> {
    after.validate()?;
    let unchanged = *after == before.data;
    ensure!(
        unchanged
            || after.revision
                == before
                    .data
                    .revision
                    .checked_add(1)
                    .context("Sidecar revision overflow")?,
        "invalid Sidecar revision transition"
    );
    for link in &after.links {
        ensure!(
            normalize(workspace, link.clone()).await? == *link,
            "patch contains an unnormalized Link: {}",
            link.id
        );
    }
    let path = workspace.root.join(FILE_NAME);
    let lock_path = workspace.root.join(".monika-sidecar.lock");
    let before = before.clone();
    let mut bytes = serde_json::to_vec_pretty(after)?;
    bytes.push(b'\n');
    tokio::task::spawn_blocking(move || -> Result<()> {
        let lock = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        lock.try_lock()
            .context("another Sidecar write is in progress; retry after it finishes")?;
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file() && !metadata.is_symlink(),
                    "Sidecar must be a regular file"
                );
                Some(metadata)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("inspect Sidecar"),
        };
        let current = if metadata.is_some() {
            Some(digest(&std::fs::read(&path)?))
        } else {
            None
        };
        ensure!(
            current == before.content_hash,
            "Sidecar changed since the patch was prepared"
        );
        if unchanged {
            return Ok(());
        }
        let mut staged =
            tempfile::NamedTempFile::new_in(path.parent().context("Sidecar has no parent")?)?;
        if let Some(metadata) = metadata {
            staged.as_file().set_permissions(metadata.permissions())?;
        }
        staged.write_all(&bytes)?;
        staged.as_file().sync_all()?;
        staged.persist(&path).context("replace Sidecar")?;
        #[cfg(unix)]
        std::fs::File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    })
    .await?
}
