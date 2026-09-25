use crate::editing::{self, Change, Destination, Patch};
use crate::engine::Engine;
use crate::extension_test;
use crate::graph::{self, Conflict, Endpoint, Link};
use crate::model::{Diagnostic, Selector, Target};
use crate::query::Expression;
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
    },
    ExtensionList,
    ExtensionShow {
        name: String,
    },
    ExtensionTest {
        selectors: Vec<Selector>,
    },
    Edit {
        destination: Destination,
        change: Change,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
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
            snapshot: None,
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
    pub undetermined: Vec<UnknownMatch>,
    pub conflicts: Vec<Conflict>,
    pub complete: bool,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn select(engine: &mut Engine, filter: Option<&Expression>) -> Result<Selection> {
    if let Some(filter) = filter {
        filter.validate()?;
    }
    let graph = graph::build(engine)?;
    let mut result = Selection {
        links: Vec::new(),
        undetermined: Vec::new(),
        conflicts: Vec::new(),
        complete: graph.complete,
        diagnostics: graph.diagnostics,
    };
    for link in graph.links {
        match filter.map_or(Ok(true), |filter| filter.evaluate(engine, &link)) {
            Ok(true) => result.links.push(link),
            Ok(false) => {}
            Err(error) => result.undetermined.push(UnknownMatch {
                link,
                reason: format!("{error:#}"),
            }),
        }
    }
    let selected: BTreeSet<_> = result
        .links
        .iter()
        .map(|link| &link.key)
        .chain(result.undetermined.iter().map(|unknown| &unknown.link.key))
        .collect();
    result.conflicts = graph
        .conflicts
        .into_iter()
        .filter(|conflict| conflict.variants.iter().any(|key| selected.contains(key)))
        .collect();
    result.complete &= result.undetermined.is_empty();
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
    pub key: String,
    pub extension: String,
    pub findings: Vec<CheckFinding>,
}

#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub valid: bool,
    pub selection: Selection,
    pub findings: Vec<LinkFindings>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRegionInput {
    name: String,
    target: Target,
    #[serde(default)]
    snapshot: Option<String>,
}

pub fn check(engine: &mut Engine, filter: Option<&Expression>) -> Result<CheckResult> {
    let mut selection = select(engine, filter)?;
    let mut findings = Vec::new();
    let checkers = engine.session.workspace.config.checks.clone();
    for link in &selection.links {
        let mut endpoints = Vec::new();
        for (name, endpoint) in [("source", &link.source), ("target", &link.target)] {
            match endpoint {
                Endpoint::Resolved { .. } => endpoints.push(CheckFinding {
                    code: format!("{name}_resolved"),
                    message: format!("{name} resolved"),
                    status: CheckStatus::Pass,
                }),
                Endpoint::Unresolved { error, .. } => endpoints.push(CheckFinding {
                    code: format!("{name}_unresolved"),
                    message: error.clone(),
                    status: CheckStatus::Fail,
                }),
            }
        }
        findings.push(LinkFindings {
            key: link.key.clone(),
            extension: "core".into(),
            findings: endpoints,
        });
        for checker in &checkers {
            let result = (|| -> Result<Vec<CheckFinding>> {
                let plan: Vec<CheckRegionInput> = serde_json::from_value(engine.call(
                    &checker.extension,
                    "check_plan",
                    json!({"link": link.extension_input(), "options": checker.options}),
                )?)
                .context("invalid check_plan response")?;
                let mut regions = BTreeMap::new();
                for input in plan {
                    ensure!(
                        !regions.contains_key(&input.name),
                        "duplicate check input name: {}",
                        input.name
                    );
                    let region = if let Some(snapshot) = input.snapshot {
                        let mut historical = Engine::new(Session::replay(
                            engine.session.workspace.clone(),
                            &snapshot,
                        )?)?;
                        let selector = input
                            .target
                            .expand(&historical.session.workspace.config.names)?;
                        historical.resolve(&selector)?
                    } else {
                        let selector = input
                            .target
                            .expand(&engine.session.workspace.config.names)?;
                        engine.resolve(&selector)?
                    };
                    regions.insert(input.name, region);
                }
                serde_json::from_value(engine.call(
                    &checker.extension,
                    "check",
                    json!({"link": link.extension_input(), "options": checker.options, "regions": regions}),
                )?)
                .context("invalid check response")
            })();
            match result {
                Ok(result) => findings.push(LinkFindings {
                    key: link.key.clone(),
                    extension: checker.extension.clone(),
                    findings: result,
                }),
                Err(error) => selection.diagnostics.push(Diagnostic {
                    stage: "check".into(),
                    subject: format!("{}:{}", checker.extension, link.key),
                    message: format!("{error:#}"),
                }),
            }
        }
    }
    selection.complete &= selection.diagnostics.is_empty();
    let valid = selection.complete
        && selection.conflicts.is_empty()
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
    })
}

pub fn execute(workspace: Workspace, snapshot: Option<&str>, operation: Operation) -> Response {
    if let Operation::Init = operation {
        return match (|| -> Result<Value> {
            ensure!(snapshot.is_none(), "init does not accept --snapshot");
            Ok(json!({"config": workspace.initialize()?}))
        })() {
            Ok(data) => Response {
                schema: 1,
                ok: true,
                complete: true,
                snapshot: None,
                data: Some(data),
                error: None,
                exit_code: 0,
            },
            Err(error) => Response::error(format!("{error:#}")),
        };
    }
    let prepared = (|| -> Result<Engine> {
        Engine::new(match snapshot {
            Some(id) => Session::replay(workspace, id)?,
            None => Session::new(workspace)?,
        })
    })();
    let mut engine = match prepared {
        Ok(engine) => engine,
        Err(error) => return Response::error(format!("{error:#}")),
    };
    let result = (|| -> Result<(Value, bool, u8)> {
        Ok(match operation {
            Operation::Init => unreachable!(),
            Operation::Show { target } => {
                let selector = target.expand(&engine.session.workspace.config.names)?;
                let region = engine.resolve(&selector)?;
                (json!({"selector": selector, "region": region}), true, 0)
            }
            Operation::Links { filter } => {
                let selected = select(&mut engine, filter.as_ref())?;
                let complete = selected.complete;
                (
                    serde_json::to_value(selected)?,
                    complete,
                    if complete { 0 } else { 2 },
                )
            }
            Operation::Check { filter } => {
                let checked = check(&mut engine, filter.as_ref())?;
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
            Operation::ExtensionList => (serde_json::to_value(engine.manifests()?)?, true, 0),
            Operation::ExtensionShow { name } => {
                (serde_json::to_value(engine.manifest(&name)?)?, true, 0)
            }
            Operation::ExtensionTest { selectors } => {
                let report = extension_test::test_regions(&mut engine, &selectors)?;
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
            Operation::Edit {
                destination,
                change,
                patch_out,
            } => {
                let patch = editing::prepare(&mut engine, &destination, &change)?;
                let data = if let Some(path) = patch_out {
                    use std::io::Write;
                    let path = if path.is_absolute() {
                        path
                    } else {
                        engine.session.workspace.root.join(path)
                    };
                    let parent = path.parent().context("patch path has no parent")?;
                    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
                    serde_json::to_writer_pretty(staged.as_file_mut(), &patch)?;
                    staged.write_all(b"\n")?;
                    staged.as_file().sync_all()?;
                    staged
                        .persist_noclobber(&path)
                        .map_err(|error| error.error)
                        .context("save patch (destination must not exist)")?;
                    json!({"patch": patch, "path": path, "applied": false})
                } else {
                    json!({"patch": patch.id, "applied": true, "commit": editing::apply(&mut engine, &patch)?})
                };
                (data, true, 0)
            }
            Operation::Apply { patch } => (
                json!({"patch": patch.id, "applied": true, "commit": editing::apply(&mut engine, &patch)?}),
                true,
                0,
            ),
        })
    })();
    let snapshot = match engine.session.save() {
        Ok(snapshot) => snapshot,
        Err(error) => return Response::error(format!("save observations: {error:#}")),
    };
    match result {
        Ok((mut data, complete, exit_code)) => {
            let result_snapshot =
                if let Some(committed) = data.pointer("/commit/snapshot").and_then(Value::as_str) {
                    let committed = committed.to_owned();
                    data["base_snapshot"] = json!(snapshot);
                    committed
                } else {
                    snapshot
                };
            Response {
                schema: 1,
                ok: true,
                complete,
                snapshot: Some(result_snapshot),
                data: Some(data),
                error: None,
                exit_code,
            }
        }
        Err(error) => {
            let mut response = Response::error(format!("{error:#}"));
            response.snapshot = Some(snapshot);
            response
        }
    }
}
