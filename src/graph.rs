use crate::engine::Engine;
use crate::extensions::Occurrence;
use crate::model::{Declaration, Diagnostic, Region, Relation, Selector, digest};
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Endpoint {
    Resolved {
        selector: Selector,
        region: Region,
    },
    Unresolved {
        selector: Option<Selector>,
        error: String,
    },
}

impl Endpoint {
    pub fn selector(&self) -> Option<&Selector> {
        match self {
            Self::Resolved { selector, .. } => Some(selector),
            Self::Unresolved { selector, .. } => selector.as_ref(),
        }
    }

    pub fn region(&self) -> Result<&Region> {
        match self {
            Self::Resolved { region, .. } => Ok(region),
            Self::Unresolved { error, .. } => Err(anyhow::anyhow!("unresolved region: {error}")),
        }
    }

    pub async fn resolve(engine: &mut Engine<'_>, selector: Selector) -> Self {
        match engine.resolve(&selector).await {
            Ok(region) => Self::Resolved { selector, region },
            Err(error) => Self::Unresolved {
                selector: Some(selector),
                error: format!("{error:#}"),
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct Link {
    /// A value identifier; edits use evidence or an explicit declaration ID.
    pub key: String,
    pub source: Endpoint,
    pub target: Endpoint,
    pub occurrences: Vec<Occurrence>,
}

impl Link {
    pub fn extension_input(&self) -> serde_json::Value {
        serde_json::json!({"key": self.key, "source": self.source, "target": self.target, "occurrences": self.occurrences})
    }
    pub fn declaration(&self) -> &Declaration {
        &self.occurrences[0].declaration
    }

    pub fn ids(&self) -> BTreeSet<&str> {
        self.occurrences
            .iter()
            .filter_map(|o| o.declaration.id.as_deref())
            .collect()
    }
}

impl Serialize for Link {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct RegionSummary<'a> {
            domain: &'a str,
            observations: &'a [String],
        }
        #[derive(Serialize)]
        #[serde(tag = "status", rename_all = "snake_case")]
        enum Summary<'a> {
            Resolved {
                selector: &'a Selector,
                region: RegionSummary<'a>,
            },
            Unresolved {
                selector: &'a Option<Selector>,
                error: &'a str,
            },
        }
        impl<'a> From<&'a Endpoint> for Summary<'a> {
            fn from(endpoint: &'a Endpoint) -> Self {
                match endpoint {
                    Endpoint::Resolved { selector, region } => Self::Resolved {
                        selector,
                        region: RegionSummary {
                            domain: &region.domain,
                            observations: &region.observations,
                        },
                    },
                    Endpoint::Unresolved { selector, error } => {
                        Self::Unresolved { selector, error }
                    }
                }
            }
        }
        #[derive(Serialize)]
        struct View<'a> {
            key: &'a str,
            source: Summary<'a>,
            target: Summary<'a>,
            occurrences: &'a [Occurrence],
        }
        View {
            key: &self.key,
            source: (&self.source).into(),
            target: (&self.target).into(),
            occurrences: &self.occurrences,
        }
        .serialize(serializer)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Conflict {
    pub id: String,
    pub variants: Vec<String>,
    pub verified: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Graph {
    pub links: Vec<Link>,
    pub conflicts: Vec<Conflict>,
    /// Covers source enumeration, extraction, and semantic aggregation.
    /// Endpoint failures stay on their endpoints, not in the scan coverage.
    pub complete: bool,
    pub diagnostics: Vec<Diagnostic>,
}

async fn same_endpoint(engine: &mut Engine<'_>, left: &Endpoint, right: &Endpoint) -> Result<bool> {
    if let (Some(left), Some(right)) = (left.selector(), right.selector())
        && left == right
    {
        return Ok(true);
    }
    match (left, right) {
        (Endpoint::Resolved { region: left, .. }, Endpoint::Resolved { region: right, .. }) => {
            if left.domain != right.domain {
                return Ok(false);
            }
            if let (Some(left), Some(right)) = (&left.equivalence_key, &right.equivalence_key)
                && left != right
            {
                return Ok(false);
            }
            Ok(engine.compare(left, right).await? == Relation::Equal)
        }
        _ => Ok(false),
    }
}

pub async fn build(engine: &mut Engine<'_>) -> Result<Graph> {
    let extracted = engine.extract().await?;
    let mut graph = Graph {
        links: Vec::new(),
        conflicts: Vec::new(),
        complete: true,
        diagnostics: extracted.diagnostics,
    };
    for occurrence in extracted.occurrences {
        tokio::task::yield_now().await;
        let source = Endpoint::resolve(engine, occurrence.declaration.source.clone()).await;
        let target = match occurrence
            .declaration
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
        let mut matched = None;
        for (index, candidate) in graph.links.iter().enumerate() {
            if candidate.declaration().annotation != occurrence.declaration.annotation {
                continue;
            }
            let same = (async {
                if !same_endpoint(engine, &source, &candidate.source).await? {
                    return Ok(false);
                }
                if target.selector().is_none() && candidate.target.selector().is_none() {
                    return Ok(
                        occurrence.declaration.reference == candidate.declaration().reference
                    );
                }
                same_endpoint(engine, &target, &candidate.target).await
            })
            .await;
            match same {
                Ok(true) => {
                    matched = Some(index);
                    break;
                }
                Ok(false) => {}
                Err(error) => graph.diagnostics.push(Diagnostic {
                    stage: "aggregate".into(),
                    subject: occurrence.evidence.resource.clone(),
                    message: format!("{error:#}"),
                }),
            }
        }
        if let Some(index) = matched {
            graph.links[index].occurrences.push(occurrence);
        } else {
            let mut value = occurrence.declaration.clone();
            value.id = None;
            graph.links.push(Link {
                key: digest(&serde_json::to_vec(&value)?),
                source,
                target,
                occurrences: vec![occurrence],
            });
        }
    }
    let mut ids: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for link in &graph.links {
        for id in link.ids() {
            ids.entry(id.to_owned())
                .or_default()
                .insert(link.key.clone());
        }
    }
    graph.conflicts = ids
        .into_iter()
        .filter(|(_, variants)| variants.len() > 1)
        .map(|(id, variants)| {
            let verified = graph.diagnostics.is_empty()
                && graph
                    .links
                    .iter()
                    .filter(|link| variants.contains(&link.key))
                    .all(|link| {
                        matches!(link.source, Endpoint::Resolved { .. })
                            && matches!(link.target, Endpoint::Resolved { .. })
                    });
            Conflict {
                id,
                variants: variants.into_iter().collect(),
                verified,
            }
        })
        .collect();
    graph.complete = graph.diagnostics.is_empty();
    Ok(graph)
}
