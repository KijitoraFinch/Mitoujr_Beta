use crate::model::{Link, Target, require_version};
use crate::workspace::{Session, Workspace};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, BTreeSet};

pub const FILE_NAME: &str = "monika.links.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sidecar {
    pub version: u32,
    pub links: Vec<Link>,
}

impl Default for Sidecar {
    fn default() -> Self {
        Self {
            version: 1,
            links: Vec::new(),
        }
    }
}

impl Sidecar {
    pub fn validate(&self) -> Result<()> {
        require_version(self.version)?;
        let mut ids = BTreeSet::new();
        for link in &self.links {
            link.validate()?;
            ensure!(ids.insert(&link.id), "duplicate Link ID: {}", link.id);
        }
        Ok(())
    }
}

pub fn definition_key(link: &Link) -> Result<String> {
    // Captures record confirmation times; they do not change a Link's meaning.
    Ok(serde_json::to_string(&(
        &link.source.target,
        &link.target.target,
        &link.annotation,
    ))?)
}

/// Expand names and normalize Resources without observing them.
pub async fn canonical_link(workspace: &Workspace, mut link: Link) -> Result<Link> {
    link.validate()?;
    for endpoint in [&mut link.source, &mut link.target] {
        endpoint.target =
            Target::RegionSelector(canonical_selector(workspace, &endpoint.target).await?);
    }
    Ok(link)
}

pub async fn canonical_selector(
    workspace: &Workspace,
    target: &Target,
) -> Result<crate::model::RegionSelector> {
    let mut selector = target.expand(&workspace.config.names)?;
    ensure!(!selector.extension.is_empty(), "empty selector extension");
    if let Some(resource) = &selector.resource {
        selector.resource = Some(workspace.normalize_resource(resource).await?);
    }
    Ok(selector)
}

pub async fn validate(workspace: &Workspace, data: &Sidecar) -> Result<()> {
    data.validate()?;
    let mut definitions = BTreeSet::new();
    for link in &data.links {
        let canonical = canonical_link(workspace, link.clone()).await?;
        ensure!(
            definitions.insert(definition_key(&canonical)?),
            "duplicate Link definition after expanding names and normalizing Resources: {}",
            link.id
        );
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SidecarState {
    pub text: String,
    pub data: Sidecar,
}

pub async fn read(workspace: &Workspace) -> Result<SidecarState> {
    let text = tokio::fs::read_to_string(workspace.root.join(FILE_NAME)).await
        .context("read monika.links.json; initialize a new workspace with monika init, or restore its Sidecar")?;
    let data = serde_json::from_str(&text).context("parse monika.links.json")?;
    validate(workspace, &data).await?;
    Ok(SidecarState { text, data })
}

impl Session {
    pub async fn sidecar(&mut self) -> Result<&SidecarState> {
        if self.sidecar.is_none() {
            self.sidecar = Some(read(&self.workspace).await?);
        }
        Ok(self.sidecar.as_ref().expect("Sidecar loaded"))
    }
}

/// Edit only Capture values, retaining all other bytes, including escaped names,
/// number spellings and whitespace. serde_json supplies validated source spans.
pub async fn record_captures(
    workspace: &Workspace,
    before: &SidecarState,
    link_index: usize,
    captures: &[(String, String)],
) -> Result<()> {
    let root: BTreeMap<String, &RawValue> = serde_json::from_str(&before.text)?;
    let links: Vec<&RawValue> = serde_json::from_str(root["links"].get())?;
    let link: BTreeMap<String, &RawValue> = serde_json::from_str(links[link_index].get())?;
    let mut edits = Vec::new();
    for (end, id) in captures {
        let endpoint = link[end].get();
        let fields: BTreeMap<String, &RawValue> = serde_json::from_str(endpoint)?;
        let encoded = serde_json::to_string(id)?;
        let (start, len, replacement) = if let Some(value) = fields.get("capture") {
            (
                value.get().as_ptr() as usize - before.text.as_ptr() as usize,
                value.get().len(),
                encoded,
            )
        } else {
            (
                endpoint.as_ptr() as usize - before.text.as_ptr() as usize + endpoint.len() - 1,
                0,
                format!(",\"capture\":{encoded}"),
            )
        };
        edits.push((start, len, replacement));
    }
    edits.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    let mut text = before.text.clone();
    for (start, len, replacement) in edits {
        text.replace_range(start..start + len, &replacement);
    }
    validate(workspace, &serde_json::from_str(&text)?).await?;
    let path = workspace.root.join(FILE_NAME);
    ensure!(
        tokio::fs::read_to_string(&path).await? == before.text,
        "Sidecar changed while capturing; retry after reviewing it"
    );
    workspace.replace(&path, text.as_bytes()).await
}
