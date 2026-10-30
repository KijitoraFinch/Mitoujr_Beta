//! Persistent content-addressed Capture and Observation objects.
use crate::model::{Capture, Observation, digest};
use crate::workspace::{ObservationPolicy, Workspace};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use tokio::fs;

fn object_path(workspace: &Workspace, kind: &str, id: &str) -> Result<PathBuf> {
    ensure!(
        id.len() == 64
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid {kind} ID: {id}"
    );
    Ok(workspace.root.join(".monika").join(kind).join(id))
}

async fn save_object(workspace: &Workspace, kind: &str, id: &str, bytes: &[u8]) -> Result<()> {
    let path = object_path(workspace, kind, id)?;
    fs::create_dir_all(path.parent().unwrap()).await?;
    match fs::read(&path).await {
        Ok(existing) => ensure!(existing == bytes, "corrupt stored {kind} object: {id}"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            workspace.publish_new(&path, bytes).await?
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub async fn save_capture(workspace: &Workspace, capture: &Capture) -> Result<String> {
    let mut bytes = serde_json::to_vec_pretty(capture)?;
    bytes.push(b'\n');
    let id = digest(&bytes);
    save_object(workspace, "captures", &id, &bytes).await?;
    Ok(id)
}

pub async fn capture(workspace: &Workspace, id: &str) -> Result<Capture> {
    let bytes = fs::read(object_path(workspace, "captures", id)?)
        .await
        .with_context(|| format!("read Capture {id}"))?;
    ensure!(digest(&bytes) == id, "corrupt Capture: {id}");
    let capture: Capture = serde_json::from_slice(&bytes)?;
    let ids: BTreeSet<_> = capture.region.observations.iter().collect();
    ensure!(
        ids == capture.observations.keys().collect(),
        "Capture Observation references are inconsistent: {id}"
    );
    for (key, observation) in &capture.observations {
        observation.validate_metadata()?;
        ensure!(
            key == &observation.id,
            "Capture Observation ID mismatch: {id}"
        );
    }
    Ok(capture)
}

/// One object: a JSON metadata line, then the exact binary Resource content.
pub async fn save_observation(
    workspace: &Workspace,
    observation: &Observation,
    content: &[u8],
) -> Result<()> {
    observation.verify(content)?;
    let mut bytes = serde_json::to_vec(observation)?;
    bytes.push(b'\n');
    bytes.extend_from_slice(content);
    save_object(workspace, "observations", &observation.id, &bytes).await
}

pub async fn observation(workspace: &Workspace, id: &str) -> Result<(Observation, Vec<u8>)> {
    let bytes = fs::read(object_path(workspace, "observations", id)?)
        .await
        .with_context(|| format!("read saved Observation {id}"))?;
    let boundary = bytes
        .iter()
        .position(|b| *b == b'\n')
        .context("invalid Observation object")?;
    let observation: Observation = serde_json::from_slice(&bytes[..boundary])?;
    let content = bytes[boundary + 1..].to_vec();
    ensure!(observation.id == id, "saved Observation ID mismatch: {id}");
    observation.verify(&content)?;
    Ok((observation, content))
}

pub async fn current(workspace: &Workspace) -> Result<BTreeMap<String, String>> {
    match fs::read(workspace.root.join(".monika/current.json")).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("parse saved Observation index")?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error.into()),
    }
}

pub async fn adopt(workspace: &Workspace, observations: &[Observation]) -> Result<()> {
    let mut index = current(workspace).await?;
    for observation in observations {
        index.insert(observation.resource.clone(), observation.id.clone());
    }
    fs::create_dir_all(workspace.root.join(".monika")).await?;
    let mut bytes = serde_json::to_vec_pretty(&index)?;
    bytes.push(b'\n');
    workspace
        .replace(&workspace.root.join(".monika/current.json"), &bytes)
        .await
}

#[derive(Debug, Serialize)]
pub struct Garbage {
    pub captures: Vec<String>,
    pub observations: Vec<String>,
    pub dry_run: bool,
}

pub async fn gc(workspace: &Workspace, keep: &[String], dry_run: bool) -> Result<Garbage> {
    let sidecar = crate::sidecar::read(workspace).await?;
    let mut captures: BTreeSet<_> = keep.iter().cloned().collect();
    for link in sidecar.data.links {
        captures.extend(link.source.capture);
        captures.extend(link.target.capture);
    }
    // Validate live references before removing anything.
    for id in &captures {
        capture(workspace, id).await?;
    }
    let index = current(workspace).await?;
    let mut live_index = BTreeMap::new();
    for (resource, id) in &index {
        let canonical = workspace.normalize_resource(resource).await?;
        ensure!(
            canonical == *resource,
            "noncanonical Resource in saved Observation index: {resource}"
        );
        if workspace.observation_policy(resource) == ObservationPolicy::Refreshed {
            let (observation, _) = observation(workspace, id).await?;
            ensure!(
                observation.resource == *resource,
                "saved Observation Resource mismatch: {resource}"
            );
            live_index.insert(resource.clone(), id.clone());
        }
    }
    let observations: BTreeSet<_> = live_index.values().cloned().collect();
    let garbage = Garbage {
        captures: unreachable(workspace, "captures", &captures).await?,
        observations: unreachable(workspace, "observations", &observations).await?,
        dry_run,
    };
    if !dry_run {
        if index != live_index {
            workspace
                .replace(
                    &workspace.root.join(".monika/current.json"),
                    &serde_json::to_vec_pretty(&live_index)?,
                )
                .await?;
        }
        for (kind, ids) in [
            ("captures", &garbage.captures),
            ("observations", &garbage.observations),
        ] {
            for id in ids {
                fs::remove_file(object_path(workspace, kind, id)?).await?;
            }
        }
    }
    Ok(garbage)
}

async fn unreachable(
    workspace: &Workspace,
    kind: &str,
    live: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let mut directory = match fs::read_dir(workspace.root.join(".monika").join(kind)).await {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut ids = Vec::new();
    while let Some(entry) = directory.next_entry().await? {
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid object filename"))?;
        object_path(workspace, kind, &id)?;
        ensure!(
            entry.file_type().await?.is_file(),
            "stored object must be a regular file: {id}"
        );
        if !live.contains(&id) {
            ids.push(id);
        }
    }
    ids.sort();
    Ok(ids)
}
