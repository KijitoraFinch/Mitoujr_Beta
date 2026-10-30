use crate::engine::Engine;
use crate::links::Endpoint;
use crate::model::{Capture, Link, LinkEnd, Observation, Region, RegionSelector, Relation, Target};
use crate::sidecar;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub fn at<T: serde::de::DeserializeOwned>(input: &Value, pointer: &str) -> Result<T> {
    let value = input
        .pointer(pointer)
        .with_context(|| format!("JSON Pointer {pointer:?} does not select a value"))?;
    serde_json::from_value(value.clone())
        .with_context(|| format!("invalid value at JSON Pointer {pointer:?}"))
}

pub async fn links(engine: &mut Engine<'_>) -> Result<Vec<Link>> {
    let links = engine.session.sidecar().await?.data.links.clone();
    let mut canonical = Vec::with_capacity(links.len());
    for link in links {
        canonical.push(sidecar::canonical_link(&engine.session.workspace, link).await?);
    }
    Ok(canonical)
}

/// Expand and normalize a Target, then resolve its RegionSelector to a Region.
async fn resolve_target(
    engine: &mut Engine<'_>,
    target: &Target,
) -> Result<(RegionSelector, Region)> {
    let selector = sidecar::canonical_selector(&engine.session.workspace, target).await?;
    let region = engine.resolve(&selector).await?;
    Ok((selector, region))
}

fn observations<'a>(
    engine: &Engine<'_>,
    regions: impl IntoIterator<Item = &'a Region>,
) -> BTreeMap<String, Observation> {
    let ids: BTreeSet<_> = regions
        .into_iter()
        .flat_map(|region| &region.observations)
        .collect();
    ids.into_iter()
        .map(|id| (id.clone(), engine.session.observation(id).clone()))
        .collect()
}

pub async fn read(engine: &mut Engine<'_>, input: Value, pointer: &str) -> Result<Value> {
    let endpoint: LinkEnd = at(&input, pointer)?;
    let (selector, region) = resolve_target(engine, &endpoint.target).await?;
    Ok(
        json!({"input":input, "selector":selector, "observations":observations(engine, [&region]), "region":region}),
    )
}

pub async fn compare(
    engine: &mut Engine<'_>,
    input: Value,
    left: &str,
    right: &str,
) -> Result<Value> {
    let left: LinkEnd = at(&input, left)?;
    let right: LinkEnd = at(&input, right)?;
    let (_, left) = resolve_target(engine, &left.target)
        .await
        .context("left Target")?;
    let (_, right) = resolve_target(engine, &right.target)
        .await
        .context("right Target")?;
    let relation = engine.compare(&left, &right).await?;
    Ok(
        json!({"input":input, "relation":relation, "observations":observations(engine, [&left, &right])}),
    )
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckFinding {
    pub code: String,
    pub message: String,
    pub status: CheckStatus,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
    NotApplicable,
}

#[derive(Debug, Serialize)]
pub struct Findings {
    pub extension: String,
    pub findings: Vec<CheckFinding>,
}

#[derive(Serialize)]
pub struct CaptureCheck {
    pub id: String,
    pub changed: bool,
}

#[derive(Serialize)]
pub struct Checked {
    pub input: Value,
    pub valid: bool,
    pub source: Endpoint,
    pub target: Endpoint,
    pub findings: Vec<Findings>,
    pub duplicates: Vec<String>,
    pub captures: BTreeMap<String, CaptureCheck>,
    pub observations: BTreeMap<String, Observation>,
    #[serde(skip)]
    pub link: Link,
}

pub async fn check(engine: &mut Engine<'_>, input: Value, pointer: &str) -> Result<Checked> {
    let link = sidecar::canonical_link(&engine.session.workspace, at(&input, pointer)?).await?;
    // Validate every baseline before interpreting either endpoint.
    let mut baselines = BTreeMap::new();
    for (name, endpoint) in [("source", &link.source), ("target", &link.target)] {
        if let Some(id) = &endpoint.capture {
            let capture = crate::storage::capture(&engine.session.workspace, id).await?;
            let selector =
                sidecar::canonical_selector(&engine.session.workspace, &endpoint.target).await?;
            ensure!(
                capture.selector == selector,
                "Link {} {name} selector does not match Capture {id}; review and capture this endpoint again",
                link.id
            );
            baselines.insert(name, (id.clone(), capture));
        }
    }
    let (selector, region) = resolve_target(engine, &link.source.target)
        .await
        .with_context(|| format!("Link {} source", link.id))?;
    let source = Endpoint { selector, region };
    let (selector, region) = resolve_target(engine, &link.target.target)
        .await
        .with_context(|| format!("Link {} target", link.id))?;
    let target = Endpoint { selector, region };
    let observed = observations(engine, [&source.region, &target.region]);
    let mut captures = BTreeMap::new();
    for (name, endpoint) in [("source", &source), ("target", &target)] {
        if let Some((id, capture)) = baselines.remove(name) {
            captures.insert(
                name.to_owned(),
                CaptureCheck {
                    id,
                    changed: capture.region.content != endpoint.region.content,
                },
            );
        }
    }
    let mut findings = Vec::new();
    for checker in engine.session.workspace.config.checks.clone() {
        let result = serde_json::from_value(
            engine
                .call(
                    &checker.extension,
                    "check",
                    json!({"link":link,"source":source,"target":target,"options":checker.options}),
                )
                .await?,
        )
        .context("invalid check response")?;
        findings.push(Findings {
            extension: checker.extension,
            findings: result,
        });
    }
    Ok(Checked {
        valid: captures.values().all(|capture| !capture.changed)
            && findings.iter().all(|group| {
                group
                    .findings
                    .iter()
                    .all(|finding| finding.status != CheckStatus::Fail)
            }),
        input,
        source,
        target,
        findings,
        duplicates: Vec::new(),
        captures,
        observations: observed,
        link,
    })
}

/// Only compare supplied Links. Each endpoint has already been interpreted once.
/// O(n²) comparator calls in the worst case; annotation groups skip irrelevant pairs.
pub async fn duplicates(engine: &mut Engine<'_>, checked: &mut [Checked]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, item) in checked.iter().enumerate() {
        ensure!(
            ids.insert(&item.link.id),
            "duplicate input Link ID: {}",
            item.link.id
        );
        groups
            .entry(serde_json::to_string(&item.link.annotation)?)
            .or_default()
            .push(i);
    }
    drop(ids);
    for group in groups.values() {
        for (position, &i) in group.iter().enumerate() {
            for &j in &group[position + 1..] {
                if engine
                    .compare(&checked[i].source.region, &checked[j].source.region)
                    .await?
                    == Relation::Equal
                    && engine
                        .compare(&checked[i].target.region, &checked[j].target.region)
                        .await?
                        == Relation::Equal
                {
                    let id = checked[i].link.id.clone();
                    checked[i].duplicates.push(checked[j].link.id.clone());
                    checked[j].duplicates.push(id);
                    checked[i].valid = false;
                    checked[j].valid = false;
                }
            }
        }
    }
    Ok(())
}

/// Read a stored result without expansion, observation, or extension processes.
pub async fn read_captured(engine: &Engine<'_>, input: Value, pointer: &str) -> Result<Value> {
    let value: Value = at(&input, pointer)?;
    let id = match &value {
        Value::String(id) => id.clone(),
        Value::Object(object) if object.len() == 1 && object.contains_key("capture") => {
            object["capture"]
                .as_str()
                .context("Capture ID must be a string")?
                .to_owned()
        }
        Value::Object(_) => {
            let endpoint: LinkEnd =
                serde_json::from_value(value).context("invalid captured endpoint")?;
            endpoint
                .capture
                .context("endpoint has no Capture; capture it first")?
        }
        _ => anyhow::bail!("captured read requires a Capture ID or endpoint"),
    };
    let capture = crate::storage::capture(&engine.session.workspace, &id).await?;
    Ok(
        json!({"input":input, "capture":id, "selector":capture.selector, "region":capture.region,
        "observations":capture.observations, "captured_at":capture.captured_at}),
    )
}

async fn capture_target(engine: &mut Engine<'_>, target: &Target) -> Result<(String, Capture)> {
    let (selector, region) = resolve_target(engine, target).await?;
    let capture = Capture {
        selector,
        observations: observations(engine, [&region]),
        region,
        captured_at: crate::workspace::timestamp(),
    };
    let id = crate::storage::save_capture(&engine.session.workspace, &capture).await?;
    Ok((id, capture))
}

pub async fn capture(engine: &mut Engine<'_>, input: Value, pointer: &str) -> Result<Value> {
    let endpoint: LinkEnd = at(&input, pointer)?;
    let (id, capture) = capture_target(engine, &endpoint.target).await?;
    Ok(
        json!({"input":input, "capture":id, "selector":capture.selector, "region":capture.region,
        "observations":capture.observations, "captured_at":capture.captured_at}),
    )
}

pub async fn capture_link(engine: &mut Engine<'_>, id: &str, endpoints: &[&str]) -> Result<Value> {
    let before = engine.session.sidecar().await?.clone();
    let index = before
        .data
        .links
        .iter()
        .position(|link| link.id == id)
        .with_context(|| format!("unknown Link ID: {id}"))?;
    let link = &before.data.links[index];
    let mut captures = Vec::new();
    for endpoint in endpoints {
        let end = match *endpoint {
            "source" => &link.source,
            "target" => &link.target,
            _ => anyhow::bail!("invalid endpoint: {endpoint}"),
        };
        let (capture, _) = capture_target(engine, &end.target).await?;
        captures.push(((*endpoint).to_owned(), capture));
    }
    sidecar::record_captures(&engine.session.workspace, &before, index, &captures).await?;
    engine.session.sidecar = None;
    Ok(json!({"link":id, "captures":captures.into_iter().collect::<BTreeMap<_,_>>()}))
}

pub async fn refresh(
    engine: &mut Engine<'_>,
    resources: &[String],
    all: bool,
) -> Result<Vec<Observation>> {
    use crate::workspace::ObservationPolicy;
    let mut selected = BTreeSet::new();
    if all {
        for link in links(engine).await? {
            for endpoint in [link.source, link.target] {
                let selector =
                    sidecar::canonical_selector(&engine.session.workspace, &endpoint.target)
                        .await?;
                if let Some(resource) = selector.resource
                    && engine.session.workspace.observation_policy(&resource)
                        == ObservationPolicy::Refreshed
                {
                    selected.insert(resource);
                }
            }
        }
    } else {
        for resource in resources {
            let resource = engine
                .session
                .workspace
                .normalize_resource(resource)
                .await?;
            ensure!(
                engine.session.workspace.observation_policy(&resource)
                    == ObservationPolicy::Refreshed,
                "Resource is not configured as refreshed: {resource}"
            );
            selected.insert(resource);
        }
    }
    let mut observations = Vec::new();
    for resource in selected {
        let observation = engine.observe(&resource).await?;
        crate::storage::save_observation(
            &engine.session.workspace,
            &observation,
            engine.session.bytes(&observation)?,
        )
        .await?;
        observations.push(observation);
    }
    // Adopt the batch only after every observation has succeeded.
    if !observations.is_empty() {
        crate::storage::adopt(&engine.session.workspace, &observations).await?;
    }
    Ok(observations)
}
