use crate::engine::Engine;
use crate::extensions::ImportResult;
use crate::model::{Link, Observation, ResourceState, digest, require_version};
use crate::sidecar::{self, Origin, Sidecar, SidecarState};
use crate::workspace::{Session, timestamp};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub resource: String,
    pub extension: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Add { link: Link },
    Set { link: Link },
    Remove { id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    Text(String),
    Bytes(Vec<u8>),
}

impl Content {
    fn from_bytes(bytes: Vec<u8>) -> Self {
        match String::from_utf8(bytes) {
            Ok(text) => Self::Text(text),
            Err(error) => Self::Bytes(error.into_bytes()),
        }
    }
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Bytes(bytes) => bytes,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PatchChange {
    Sidecar {
        before: SidecarState,
        after: Sidecar,
    },
    Resource {
        sidecar: SidecarState,
        provider_config_hash: String,
        before: Observation,
        replacement: Content,
        after_hash: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub version: u32,
    pub id: String,
    pub root: String,
    pub change: PatchChange,
}

impl Patch {
    fn seal(&mut self) -> Result<()> {
        self.id.clear();
        self.id = digest(&serde_json::to_vec(self)?);
        Ok(())
    }
    pub fn validate(&self) -> Result<()> {
        require_version(self.version)?;
        let mut expected = self.clone();
        expected.seal()?;
        ensure!(expected.id == self.id, "patch integrity check failed");
        match &self.change {
            PatchChange::Sidecar { before, after } => {
                before.data.validate()?;
                after.validate()?;
            }
            PatchChange::Resource {
                sidecar,
                replacement,
                after_hash,
                ..
            } => {
                sidecar.data.validate()?;
                ensure!(
                    digest(replacement.bytes()) == *after_hash,
                    "patch replacement hash mismatch"
                );
            }
        }
        Ok(())
    }
    fn new(engine: &Engine<'_>, change: PatchChange) -> Result<Self> {
        let mut patch = Self {
            version: 1,
            id: String::new(),
            root: engine.session.snapshot.root.clone(),
            change,
        };
        patch.seal()?;
        Ok(patch)
    }
}

fn sidecar_patch(engine: &Engine<'_>, before: SidecarState, mut after: Sidecar) -> Result<Patch> {
    after.order();
    if after != before.data {
        after.revision = before
            .data
            .revision
            .checked_add(1)
            .context("Sidecar revision overflow")?;
    }
    after.validate()?;
    Patch::new(engine, PatchChange::Sidecar { before, after })
}

pub async fn prepare(engine: &mut Engine<'_>, change: Change) -> Result<Patch> {
    let before = engine.session.sidecar().await?.clone();
    let mut after = before.data.clone();
    match change {
        Change::Add { link } => {
            let link = sidecar::normalize(&engine.session.workspace, link).await?;
            ensure!(
                !after.links.iter().any(|item| item.id == link.id),
                "Link ID already exists: {}",
                link.id
            );
            after.links.push(link);
        }
        Change::Set { link } => {
            let link = sidecar::normalize(&engine.session.workspace, link).await?;
            let existing = after
                .links
                .iter_mut()
                .find(|item| item.id == link.id)
                .with_context(|| format!("Link not found: {}", link.id))?;
            *existing = link;
        }
        Change::Remove { id } => {
            let index = after
                .links
                .iter()
                .position(|item| item.id == id)
                .with_context(|| format!("Link not found: {id}"))?;
            after.links.remove(index);
            after.origins.retain(|origin| origin.link != id);
        }
    }
    sidecar_patch(engine, before, after)
}

async fn interpret(
    engine: &mut Engine<'_>,
    extension: &str,
    observation: &Observation,
) -> Result<ImportResult> {
    engine
        .session
        .workspace
        .read_observation(observation)
        .await?;
    let result: ImportResult = serde_json::from_value(
        engine
            .call(
                extension,
                "import",
                json!({
                    "observation": observation,
                    "content_path": engine.session.workspace.blob_path(observation)?,
                }),
            )
            .await?,
    )
    .context("invalid import result")?;
    ensure!(
        result.diagnostics.is_empty(),
        "import was incomplete: {}",
        serde_json::to_string(&result.diagnostics)?
    );
    Ok(result)
}

pub async fn prepare_import(engine: &mut Engine<'_>, source: &Destination) -> Result<Patch> {
    let before = engine.session.sidecar().await?.clone();
    let observation = engine.observe(&source.resource).await?;
    ensure!(
        observation.state.exists,
        "import Resource does not exist: {}",
        observation.resource
    );
    let imported = interpret(engine, &source.extension, &observation).await?;
    let revision = engine.manifest(&source.extension).await?.revision;
    let mut after = before.data.clone();
    let mut links: BTreeMap<_, _> = after
        .links
        .into_iter()
        .map(|link| (link.id.clone(), link))
        .collect();
    let mut origins: BTreeSet<_> = after
        .origins
        .iter()
        .map(|origin| origin.input_key(&after.observations[&origin.observation]))
        .collect::<Result<_>>()?;
    for candidate in imported.links {
        let link = sidecar::normalize(&engine.session.workspace, candidate.link).await?;
        if let Some(existing) = links.get(&link.id) {
            ensure!(
                *existing == link,
                "import conflicts with Link ID {}; inspect and edit the Sidecar explicitly",
                link.id
            );
        } else {
            links.insert(link.id.clone(), link.clone());
        }
        let origin = Origin {
            link: link.id,
            observation: observation.id.clone(),
            extension: source.extension.clone(),
            revision: revision.clone(),
            location: candidate.location,
        };
        if origins.insert(origin.input_key(&observation)?) {
            after
                .observations
                .insert(observation.id.clone(), observation.clone());
            after.origins.push(origin);
        }
    }
    after.links = links.into_values().collect();
    sidecar_patch(engine, before, after)
}

pub async fn prepare_generation(
    engine: &mut Engine<'_>,
    destination: &Destination,
    links: &[Link],
) -> Result<Patch> {
    let sidecar = engine.session.sidecar().await?.clone();
    let before = engine.observe(&destination.resource).await?;
    // A generator must never overwrite Monika's authority or configuration.
    let workspace = &engine.session.workspace;
    for name in [sidecar::FILE_NAME, "monika.json", ".monika-sidecar.lock"] {
        ensure!(
            before.resource != workspace.resource(name).await?,
            "cannot generate over Monika management data: {name}"
        );
    }
    ensure!(
        engine.manifest(&before.provider).await?.supports("commit"),
        "Resource provider cannot commit"
    );
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Generated {
        bytes: Vec<u8>,
    }
    let generated: Generated = serde_json::from_value(engine.call(&destination.extension, "generate", json!({
        "observation": before, "content_path": engine.session.workspace.blob_path(&before)?, "links": links,
    })).await?).context("invalid generate result")?;
    let proposed = Observation::new(
        before.resource.clone(),
        &generated.bytes,
        timestamp(),
        before.provider.clone(),
        ResourceState {
            exists: true,
            revision: Value::Null,
        },
    );
    engine
        .session
        .workspace
        .save_observation(&proposed, &generated.bytes)
        .await?;
    let imported = interpret(engine, &destination.extension, &proposed).await?;
    let mut actual = Vec::new();
    for candidate in imported.links {
        actual.push(sidecar::normalize(&engine.session.workspace, candidate.link).await?);
    }
    actual.sort_by(|a, b| a.id.cmp(&b.id));
    let mut expected = links.to_vec();
    expected.sort_by(|a, b| a.id.cmp(&b.id));
    ensure!(
        actual == expected,
        "generator changed the selected Links, RegionSelectors, or annotations"
    );
    Patch::new(
        engine,
        PatchChange::Resource {
            sidecar,
            provider_config_hash: digest(&serde_json::to_vec(
                &engine.session.workspace.config.extensions[&before.provider],
            )?),
            before,
            after_hash: digest(&generated.bytes),
            replacement: Content::from_bytes(generated.bytes),
        },
    )
}

pub async fn apply(engine: &mut Engine<'_>, patch: &Patch) -> Result<Value> {
    patch.validate()?;
    ensure!(
        !engine.session.frozen,
        "patch application requires current state; omit --snapshot"
    );
    ensure!(
        patch.root == engine.session.snapshot.root,
        "patch belongs to a different workspace"
    );
    match &patch.change {
        PatchChange::Sidecar { before, after } => {
            sidecar::commit(&engine.session.workspace, before, after).await?;
            engine.session.snapshot.sidecar = Some(before.clone());
            let mut session = Session::new(engine.session.workspace.clone())?;
            session.snapshot.observations = engine.session.snapshot.observations.clone();
            session.snapshot.failures = engine.session.snapshot.failures.clone();
            session.snapshot.resolution_reads = engine.session.snapshot.resolution_reads.clone();
            let state = session.sidecar().await?.clone();
            ensure!(
                state.data == *after,
                "Sidecar changed after commit; inspect current state"
            );
            Ok(
                json!({"patch": patch.id, "sidecar": state.content_hash, "snapshot": session.save().await?}),
            )
        }
        PatchChange::Resource {
            sidecar,
            provider_config_hash,
            before,
            replacement,
            after_hash,
        } => {
            ensure!(
                sidecar::read(&engine.session.workspace).await?.content_hash
                    == sidecar.content_hash,
                "Sidecar changed since generation was prepared"
            );
            engine.session.snapshot.sidecar = Some(sidecar.clone());
            let config = engine
                .session
                .workspace
                .config
                .extensions
                .get(&before.provider)
                .context("patch Resource provider is not configured")?;
            ensure!(
                digest(&serde_json::to_vec(config)?) == *provider_config_hash,
                "Resource provider configuration changed since the patch was prepared"
            );
            engine.session.workspace.read_observation(before).await?;
            let mut current =
                Engine::new(Session::new(engine.session.workspace.clone())?, engine.host)?;
            let observed = current.observe(&before.resource).await?;
            ensure!(
                observed.provider == before.provider
                    && observed.content_hash == before.content_hash
                    && observed.state == before.state,
                "Resource changed since the patch was prepared: {}",
                before.resource
            );
            let result = current.call(&before.provider, "commit", json!({
                "resource": before.resource, "expected": {"content_hash": before.content_hash, "state": before.state},
                "replacement": {"content_hash": after_hash, "bytes": replacement.bytes()}, "patch_id": patch.id,
            })).await?;
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Committed {
                content_hash: String,
                state: ResourceState,
            }
            let committed: Committed = serde_json::from_value(result)
                .context("invalid commit result; inspect Resource state before retrying")?;
            ensure!(
                committed.content_hash == *after_hash && committed.state.exists,
                "commit result does not confirm proposed content; inspect Resource state before retrying"
            );
            let mut after =
                Engine::new(Session::new(engine.session.workspace.clone())?, engine.host)?;
            let observation = after.observe(&before.resource).await?;
            ensure!(
                observation.content_hash == *after_hash && observation.state == committed.state,
                "Resource does not match committed patch; inspect Resource state before retrying"
            );
            after.session.snapshot.sidecar = Some(sidecar.clone());
            Ok(
                json!({"patch": patch.id, "resource": before.resource, "observation": observation, "snapshot": after.session.save().await?}),
            )
        }
    }
}
