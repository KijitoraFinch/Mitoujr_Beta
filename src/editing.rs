use crate::engine::Engine;
use crate::extensions::{Extraction, Occurrence};
use crate::model::{Declaration, Observation, ResourceState, digest, require_version};
use crate::workspace::{Session, timestamp};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub resource: String,
    pub extension: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pick {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub location: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Add {
        declaration: Declaration,
    },
    Set {
        pick: Pick,
        declaration: Declaration,
    },
    Remove {
        pick: Pick,
    },
    Copy {
        from: Destination,
        pick: Pick,
    },
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
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub version: u32,
    pub id: String,
    pub root: String,
    pub provider_config_hash: String,
    pub before: Observation,
    pub replacement: Content,
    pub after_hash: String,
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
        ensure!(
            digest(self.replacement.bytes()) == self.after_hash,
            "patch replacement hash mismatch"
        );
        Ok(())
    }
}

async fn extract(
    engine: &mut Engine<'_>,
    destination: &Destination,
    observation: &Observation,
) -> Result<Vec<Occurrence>> {
    if !observation.state.exists {
        return Ok(Vec::new());
    }
    let input = json!({"observation": observation, "content_path": engine.session.workspace.blob_path(observation)?});
    let extracted: Extraction = serde_json::from_value(
        engine
            .call(&destination.extension, "extract", input)
            .await?,
    )?;
    ensure!(
        extracted.diagnostics.is_empty(),
        "cannot edit incompletely interpreted declarations: {}",
        serde_json::to_string(&extracted.diagnostics)?
    );
    for occurrence in &extracted.occurrences {
        occurrence.declaration.validate()?;
        ensure!(
            occurrence.evidence.observation == observation.id
                && occurrence.evidence.resource == observation.resource
                && occurrence.evidence.extension == destination.extension,
            "editor extraction returned evidence outside the supplied observation"
        );
    }
    Ok(extracted.occurrences)
}

fn pick<'a>(occurrences: &'a [Occurrence], pick: &Pick) -> Result<&'a Occurrence> {
    ensure!(
        pick.id.is_some() != pick.location.is_some(),
        "choose exactly one declaration ID or evidence location"
    );
    let matches: Vec<_> = occurrences
        .iter()
        .filter(|occurrence| {
            if let Some(id) = &pick.id {
                occurrence.declaration.id.as_ref() == Some(id)
            } else {
                Some(&occurrence.evidence.location) == pick.location.as_ref()
            }
        })
        .collect();
    ensure!(!matches.is_empty(), "declaration not found");
    ensure!(
        matches.len() == 1,
        "multiple declarations match; choose an evidence location"
    );
    Ok(matches[0])
}

pub async fn prepare(
    engine: &mut Engine<'_>,
    destination: &Destination,
    change: &Change,
) -> Result<Patch> {
    let before = engine.observe(&destination.resource).await?;
    ensure!(
        engine.manifest(&before.provider).await?.supports("commit"),
        "resource provider {} cannot commit patches",
        before.provider
    );
    let occurrences = extract(engine, destination, &before).await?;
    let (request, expected, removed) = match change {
        Change::Add { declaration } => (
            json!({"op":"add", "declaration": declaration}),
            Some(declaration.clone()),
            None,
        ),
        Change::Set {
            pick: selection,
            declaration,
        } => {
            let occurrence = pick(&occurrences, selection)?;
            ensure!(
                occurrence.evidence.writable,
                "this declaration is read-only in its source format"
            );
            (
                json!({"op":"set", "location": occurrence.evidence.location, "declaration": declaration}),
                Some(declaration.clone()),
                None,
            )
        }
        Change::Remove { pick: selection } => {
            let occurrence = pick(&occurrences, selection)?;
            ensure!(
                occurrence.evidence.writable,
                "this declaration is read-only in its source format"
            );
            (
                json!({"op":"remove", "location": occurrence.evidence.location}),
                None,
                Some(occurrence.declaration.clone()),
            )
        }
        Change::Copy {
            from,
            pick: selection,
        } => {
            let source = engine.observe(&from.resource).await?;
            let extracted = extract(engine, from, &source).await?;
            let declaration = pick(&extracted, selection)?.declaration.clone();
            (
                json!({"op":"add", "declaration": declaration}),
                Some(declaration),
                None,
            )
        }
    };
    if let Some(declaration) = &expected {
        declaration.validate()?;
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EditResult {
        bytes: Vec<u8>,
        location: Option<Value>,
    }
    let result: EditResult = serde_json::from_value(engine.call(&destination.extension, "edit", json!({
        "observation": before, "content_path": engine.session.workspace.blob_path(&before)?, "change": request,
    })).await?).context("invalid edit result")?;
    let proposed = Observation::new(
        before.resource.clone(),
        &result.bytes,
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
        .save_observation(&proposed, &result.bytes)
        .await?;
    let after = extract(engine, destination, &proposed).await?;
    if let Some(expected) = expected {
        let location = result
            .location
            .context("editor must return the added or changed declaration location")?;
        let selected = pick(
            &after,
            &Pick {
                id: None,
                location: Some(location),
            },
        )?;
        ensure!(
            selected.declaration == expected,
            "editor changed the requested declaration, target, or annotation"
        );
        let expected_count = occurrences.len()
            + usize::from(matches!(change, Change::Add { .. } | Change::Copy { .. }));
        ensure!(
            after.len() == expected_count,
            "editor added or removed unexpected declarations"
        );
    }
    if let Some(removed) = removed {
        let old_count = occurrences
            .iter()
            .filter(|occurrence| occurrence.declaration == removed)
            .count();
        let new_count = after
            .iter()
            .filter(|occurrence| occurrence.declaration == removed)
            .count();
        ensure!(
            old_count == new_count + 1 && occurrences.len() == after.len() + 1,
            "editor did not remove exactly the selected declaration"
        );
    }
    let config = &engine.session.workspace.config.extensions[&before.provider];
    let mut patch = Patch {
        version: 1,
        id: String::new(),
        root: engine.session.snapshot.root.clone(),
        provider_config_hash: digest(&serde_json::to_vec(config)?),
        before,
        after_hash: digest(&result.bytes),
        replacement: Content::from_bytes(result.bytes),
    };
    patch.seal()?;
    Ok(patch)
}

pub async fn apply(engine: &mut Engine<'_>, patch: &Patch) -> Result<Value> {
    patch.validate()?;
    ensure!(
        !engine.session.frozen,
        "patch application requires current observations; omit --snapshot"
    );
    ensure!(
        patch.root == engine.session.snapshot.root,
        "patch belongs to a different workspace"
    );
    let config = engine
        .session
        .workspace
        .config
        .extensions
        .get(&patch.before.provider)
        .context("patch resource provider is not configured")?;
    ensure!(
        digest(&serde_json::to_vec(config)?) == patch.provider_config_hash,
        "resource provider configuration changed since the patch was prepared"
    );
    engine
        .session
        .workspace
        .read_observation(&patch.before)
        .await?;
    // A separate session deliberately bypasses observations cached while preparing
    // the patch. The provider must also perform conditional commit atomically.
    let mut current = Engine::new(Session::new(engine.session.workspace.clone())?, engine.host)?;
    let observed = current.observe(&patch.before.resource).await?;
    ensure!(
        observed.provider == patch.before.provider
            && observed.content_hash == patch.before.content_hash
            && observed.state == patch.before.state,
        "resource changed since the patch was prepared: {}",
        patch.before.resource
    );
    let result = current.call(
        &patch.before.provider,
        "commit",
        json!({
            "resource": patch.before.resource,
            "expected": {"content_hash": patch.before.content_hash, "state": patch.before.state},
            "replacement": {"content_hash": patch.after_hash, "bytes": patch.replacement.bytes()},
            "patch_id": patch.id,
        }),
    ).await?;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Committed {
        content_hash: String,
        state: ResourceState,
    }
    let committed: Committed = serde_json::from_value(result)
        .context("invalid commit result; inspect resource state before retrying")?;
    ensure!(
        committed.content_hash == patch.after_hash && committed.state.exists,
        "commit result does not confirm the proposed content; inspect resource state before retrying"
    );
    // Read back the resource through its provider, independently of the commit
    // response. Never turn an unconfirmed write into a successful patch result.
    let mut after = Engine::new(Session::new(engine.session.workspace.clone())?, engine.host)?;
    let observation = after.observe(&patch.before.resource).await?;
    ensure!(
        observation.content_hash == patch.after_hash && observation.state == committed.state,
        "resource does not match the committed patch; inspect resource state before retrying"
    );
    Ok(
        json!({"patch": patch.id, "resource": patch.before.resource, "observation": observation, "snapshot": after.session.save().await?}),
    )
}
