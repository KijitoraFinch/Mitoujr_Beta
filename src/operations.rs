use crate::editing::{self, Change, Destination, Patch};
use crate::engine::Engine;
use crate::extension_test;
use crate::links::Endpoint;
use crate::model::{Diagnostic, Link, Observation, RegionSelector, Relation, Target};
use crate::query::Expression;
use crate::sidecar::Origin;
use crate::workspace::{Session, Workspace};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Init,
    Show {
        target: Target,
    },
    Links {
        #[serde(default, rename = "where")]
        filter: Option<Expression>,
    },
    Check {
        #[serde(default, rename = "where")]
        filter: Option<Expression>,
        #[serde(default)]
        duplicates: bool,
    },
    ExtensionList,
    ExtensionShow {
        name: String,
    },
    ExtensionTest {
        selectors: Vec<RegionSelector>,
    },
    Edit {
        change: Change,
        #[serde(default)]
        patch_out: Option<std::path::PathBuf>,
    },
    Import {
        source: Destination,
        #[serde(default)]
        patch_out: Option<std::path::PathBuf>,
    },
    Generate {
        destination: Destination,
        #[serde(default, rename = "where")]
        filter: Option<Expression>,
        #[serde(default)]
        patch_out: Option<std::path::PathBuf>,
    },
    Apply {
        patch: Patch,
    },
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub schema: u32,
    pub ok: bool,
    pub complete: bool,
    pub observations: BTreeMap<String, Observation>,
    pub diagnostics: Vec<Diagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip)]
    pub exit_code: u8,
}

impl Response {
    pub fn error(error: impl std::fmt::Display) -> Self {
        Self {
            schema: 1,
            ok: false,
            complete: false,
            observations: BTreeMap::new(),
            diagnostics: Vec::new(),
            data: None,
            error: Some(error.to_string()),
            exit_code: 2,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct UnknownMatch {
    pub link: Link,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Selection {
    pub links: Vec<Link>,
    pub origins: Vec<Origin>,
    pub observations: BTreeMap<String, Observation>,
    pub undetermined: Vec<UnknownMatch>,
    pub complete: bool,
    pub diagnostics: Vec<Diagnostic>,
}

pub async fn select(engine: &mut Engine<'_>, filter: Option<&Expression>) -> Result<Selection> {
    if let Some(filter) = filter {
        filter.validate()?;
    }
    let sidecar = engine.session.sidecar().await?.data.clone();
    let mut result = Selection {
        links: Vec::new(),
        origins: Vec::new(),
        observations: BTreeMap::new(),
        undetermined: Vec::new(),
        complete: true,
        diagnostics: Vec::new(),
    };
    for link in sidecar.links {
        tokio::task::yield_now().await;
        match match filter {
            Some(filter) => filter.evaluate(engine, &link).await,
            None => Ok(true),
        } {
            Ok(true) => result.links.push(link),
            Ok(false) => {}
            Err(error) => result.undetermined.push(UnknownMatch {
                link,
                reason: format!("{error:#}"),
            }),
        }
    }
    let ids: BTreeSet<_> = result
        .links
        .iter()
        .map(|link| &link.id)
        .chain(result.undetermined.iter().map(|item| &item.link.id))
        .collect();
    result.origins = sidecar
        .origins
        .into_iter()
        .filter(|origin| ids.contains(&origin.link))
        .collect();
    for origin in &result.origins {
        result
            .observations
            .entry(origin.observation.clone())
            .or_insert_with(|| sidecar.observations[&origin.observation].clone());
    }
    result.complete = result.undetermined.is_empty();
    Ok(result)
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
pub struct LinkFindings {
    pub id: String,
    pub extension: String,
    pub findings: Vec<CheckFinding>,
}

#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub valid: bool,
    pub selection: Selection,
    pub findings: Vec<LinkFindings>,
    pub duplicates: Vec<[String; 2]>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRegionInput {
    name: String,
    target: Target,
}

pub async fn check(
    engine: &mut Engine<'_>,
    filter: Option<&Expression>,
    inspect_duplicates: bool,
) -> Result<CheckResult> {
    let mut selection = select(engine, filter).await?;
    let mut findings = Vec::new();
    let checkers = engine.session.workspace.config.checks.clone();
    for link in &selection.links {
        let source = Endpoint::resolve(engine, link.source.clone()).await;
        let target = match link
            .reference
            .target
            .expand(&engine.session.workspace.config.names)
        {
            Ok(selector) => Endpoint::resolve(engine, selector).await,
            Err(error) => Endpoint::Unresolved {
                selector: None,
                error: format!("{error:#}"),
            },
        };
        let mut endpoints = Vec::new();
        for (name, endpoint) in [("source", &source), ("target", &target)] {
            match endpoint {
                Endpoint::Resolved { .. } => endpoints.push(CheckFinding {
                    code: format!("{name}_resolved"),
                    message: format!("{name} resolved"),
                    status: CheckStatus::Pass,
                }),
                Endpoint::Unresolved { error, .. } => {
                    selection.diagnostics.push(Diagnostic {
                        stage: "resolve".into(),
                        subject: format!("{}:{name}", link.id),
                        message: error.clone(),
                    });
                }
            }
        }
        findings.push(LinkFindings {
            id: link.id.clone(),
            extension: "core".into(),
            findings: endpoints,
        });
        if source.region().is_err() || target.region().is_err() {
            continue;
        }
        for checker in &checkers {
            let result = (async {
                let plan: Vec<CheckRegionInput> = serde_json::from_value(engine.call(
                    &checker.extension,
                    "check_plan",
                    json!({"link": link, "source": source, "target": target, "options": checker.options}),
                ).await?)
                .context("invalid check_plan response")?;
                let mut regions = BTreeMap::new();
                for input in plan {
                    ensure!(
                        !regions.contains_key(&input.name),
                        "duplicate check input name: {}",
                        input.name
                    );
                    let selector = input.target.expand(&engine.session.workspace.config.names)?;
                    let region = engine.resolve(&selector).await?;
                    regions.insert(input.name, region);
                }
                serde_json::from_value(engine.call(
                    &checker.extension,
                    "check",
                    json!({"link": link, "source": source, "target": target, "options": checker.options, "regions": regions}),
                ).await?)
                .context("invalid check response")
            }).await;
            match result {
                Ok(result) => findings.push(LinkFindings {
                    id: link.id.clone(),
                    extension: checker.extension.clone(),
                    findings: result,
                }),
                Err(error) => selection.diagnostics.push(Diagnostic {
                    stage: "check".into(),
                    subject: format!("{}:{}", checker.extension, link.id),
                    message: format!("{error:#}"),
                }),
            }
        }
    }
    let mut duplicates = Vec::new();
    if inspect_duplicates {
        let all = engine.session.sidecar().await?.data.links.clone();
        let selected: BTreeSet<_> = selection.links.iter().map(|link| &link.id).collect();
        for (index, left) in all.iter().enumerate() {
            for right in &all[index + 1..] {
                if !(selected.contains(&left.id) || selected.contains(&right.id))
                    || left.annotation != right.annotation
                {
                    continue;
                }
                let same = async {
                    let left_source = engine.resolve(&left.source).await?;
                    let right_source = engine.resolve(&right.source).await?;
                    if engine.compare(&left_source, &right_source).await? != Relation::Equal {
                        return Ok::<_, anyhow::Error>(false);
                    }
                    let names = &engine.session.workspace.config.names;
                    let left_target = left.reference.target.expand(names)?;
                    let right_target = right.reference.target.expand(names)?;
                    let left_target = engine.resolve(&left_target).await?;
                    let right_target = engine.resolve(&right_target).await?;
                    Ok(engine.compare(&left_target, &right_target).await? == Relation::Equal)
                }
                .await;
                match same {
                    Ok(true) => duplicates.push([left.id.clone(), right.id.clone()]),
                    Ok(false) => {}
                    Err(error) => selection.diagnostics.push(Diagnostic {
                        stage: "duplicate".into(),
                        subject: format!("{}:{}", left.id, right.id),
                        message: format!("{error:#}"),
                    }),
                }
            }
        }
    }
    selection.complete &= selection.diagnostics.is_empty();
    let valid = selection.complete
        && duplicates.is_empty()
        && findings.iter().all(|group| {
            group
                .findings
                .iter()
                .all(|finding| finding.status != CheckStatus::Fail)
        });
    Ok(CheckResult {
        valid,
        selection,
        findings,
        duplicates,
    })
}

pub async fn execute(
    workspace: Workspace,
    operation: Operation,
    host: &mut crate::extensions::process::Host,
) -> Response {
    if let Operation::Init = operation {
        return match (async {
            Ok::<_, anyhow::Error>(json!({"config": workspace.initialize().await?}))
        })
        .await
        {
            Ok(data) => Response {
                schema: 1,
                ok: true,
                complete: true,
                observations: BTreeMap::new(),
                diagnostics: Vec::new(),
                data: Some(data),
                error: None,
                exit_code: 0,
            },
            Err(error) => Response::error(format!("{error:#}")),
        };
    }
    let mut engine = match Engine::new(Session::new(workspace), host) {
        Ok(engine) => engine,
        Err(error) => return Response::error(format!("{error:#}")),
    };
    let result: Result<(Value, bool, u8)> = (async {
        Ok(match operation {
            Operation::Init => unreachable!(),
            Operation::Show { target } => {
                let selector = target.expand(&engine.session.workspace.config.names)?;
                let region = engine.resolve(&selector).await?;
                (json!({"selector": selector, "region": region}), true, 0)
            }
            Operation::Links { filter } => {
                let selected = select(&mut engine, filter.as_ref()).await?;
                let complete = selected.complete;
                (
                    serde_json::to_value(selected)?,
                    complete,
                    if complete { 0 } else { 2 },
                )
            }
            Operation::Check { filter, duplicates } => {
                let checked = check(&mut engine, filter.as_ref(), duplicates).await?;
                let complete = checked.selection.complete;
                let exit = if !complete {
                    2
                } else if checked.valid {
                    0
                } else {
                    1
                };
                (serde_json::to_value(checked)?, complete, exit)
            }
            Operation::ExtensionList => (serde_json::to_value(engine.manifests().await?)?, true, 0),
            Operation::ExtensionShow { name } => {
                (serde_json::to_value(engine.manifest(&name).await?)?, true, 0)
            }
            Operation::ExtensionTest { selectors } => {
                let report = extension_test::test_regions(&mut engine, &selectors).await?;
                let complete = report.complete;
                let exit = if !complete {
                    2
                } else if report.passed {
                    0
                } else {
                    1
                };
                (serde_json::to_value(report)?, complete, exit)
            }
            Operation::Edit { change, patch_out } => {
                let patch = editing::prepare(&mut engine, change).await?;
                (finish_edit(&mut engine, patch, patch_out).await?, true, 0)
            }
            Operation::Import { source, patch_out } => {
                let patch = editing::prepare_import(&mut engine, &source).await?;
                (finish_edit(&mut engine, patch, patch_out).await?, true, 0)
            }
            Operation::Generate { destination, filter, patch_out } => {
                let selected = select(&mut engine, filter.as_ref()).await?;
                ensure!(selected.complete, "cannot generate an undetermined selection: {}", serde_json::to_string(&selected.undetermined)?);
                let patch = editing::prepare_generation(&mut engine, &destination, &selected.links).await?;
                (finish_edit(&mut engine, patch, patch_out).await?, true, 0)
            }
            Operation::Apply { patch } => (
                json!({"patch": patch.id, "applied": true, "commit": editing::apply(&mut engine, &patch).await?}),
                true,
                0,
            ),
        })
    }).await;
    let mut response = match result {
        Ok((data, complete, exit_code)) => Response {
            schema: 1,
            ok: true,
            complete,
            observations: BTreeMap::new(),
            diagnostics: Vec::new(),
            data: Some(data),
            error: None,
            exit_code,
        },
        Err(error) => Response::error(format!("{error:#}")),
    };
    response.observations = engine
        .session
        .observations
        .into_values()
        .map(|observation| (observation.id.clone(), observation))
        .collect();
    response.diagnostics = engine.session.failures.into_values().collect();
    response
}

async fn finish_edit(
    engine: &mut Engine<'_>,
    patch: Patch,
    patch_out: Option<std::path::PathBuf>,
) -> Result<Value> {
    if let Some(path) = patch_out {
        let path = engine.session.workspace.root.join(path);
        let mut bytes = serde_json::to_vec_pretty(&patch)?;
        bytes.push(b'\n');
        engine
            .session
            .workspace
            .publish_new(&path, &bytes)
            .await
            .context("save patch (destination must not exist)")?;
        Ok(json!({"patch": patch, "path": path, "applied": false}))
    } else {
        Ok(
            json!({"patch": patch.id, "applied": true, "commit": editing::apply(engine, &patch).await?}),
        )
    }
}
