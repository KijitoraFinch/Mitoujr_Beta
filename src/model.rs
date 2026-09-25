use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selector {
    pub extension: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default)]
    pub query: Value,
}

impl Selector {
    /// The short spelling is a CLI convenience. The query remains extension-owned.
    pub fn parse(input: &str) -> Result<Self> {
        if input.trim_start().starts_with('{') {
            return serde_json::from_str(input).context("invalid selector JSON");
        }
        let (extension, rest) = input
            .split_once(':')
            .context("selector must be EXTENSION:RESOURCE[#QUERY] or a JSON object")?;
        ensure!(!extension.is_empty(), "selector extension is empty");
        let (resource, query) = rest.split_once('#').unwrap_or((rest, ""));
        ensure!(!resource.is_empty(), "selector resource is empty");
        Ok(Self {
            extension: extension.to_owned(),
            resource: Some(resource.to_owned()),
            query: Value::String(query.to_owned()),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Target {
    Named(NamedTarget),
    Selector(Selector),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedTarget {
    pub name: String,
}

impl Target {
    pub fn expand(&self, names: &BTreeMap<String, Target>) -> Result<Selector> {
        let mut current = self;
        let mut visited = Vec::new();
        loop {
            match current {
                Self::Selector(selector) => return Ok(selector.clone()),
                Self::Named(NamedTarget { name }) => {
                    ensure!(
                        !visited.contains(&name.as_str()),
                        "named target cycle: {} -> {name}",
                        visited.join(" -> ")
                    );
                    visited.push(name);
                    current = names
                        .get(name)
                        .with_context(|| format!("unknown named target: {name}"))?;
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub source: Selector,
    pub reference: Reference,
    #[serde(default)]
    pub annotation: Value,
}

impl Declaration {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.id.as_ref().is_none_or(|id| !id.is_empty()),
            "empty declaration ID"
        );
        ensure!(!self.source.extension.is_empty(), "empty source extension");
        match &self.reference.target {
            Target::Named(named) => ensure!(!named.name.is_empty(), "empty named target"),
            Target::Selector(selector) => {
                ensure!(!selector.extension.is_empty(), "empty target extension")
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub id: String,
    pub resource: String,
    pub content_hash: String,
    pub size: usize,
    pub observed_at: String,
    pub provider: String,
    pub state: ResourceState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceState {
    pub exists: bool,
    pub revision: Value,
}

impl Observation {
    pub fn new(
        resource: String,
        bytes: &[u8],
        observed_at: String,
        provider: String,
        state: ResourceState,
    ) -> Self {
        let mut observation = Self {
            id: String::new(),
            resource,
            content_hash: digest(bytes),
            size: bytes.len(),
            observed_at,
            provider,
            state,
        };
        observation.id =
            digest(&serde_json::to_vec(&observation).expect("serializable observation"));
        observation
    }

    pub fn verify(&self, bytes: &[u8]) -> Result<()> {
        let expected = Self::new(
            self.resource.clone(),
            bytes,
            self.observed_at.clone(),
            self.provider.clone(),
            self.state.clone(),
        );
        ensure!(
            self.state.exists || bytes.is_empty(),
            "an absent resource cannot have content"
        );
        ensure!(*self == expected, "corrupt observation {}", self.id);
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub stage: String,
    pub subject: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub interpreter: String,
    pub created_at: String,
    pub root: String,
    pub interpretation: Value,
    pub observations: BTreeMap<String, Observation>,
    /// Failures are persisted with successes; replay never silently retries live resources.
    pub failures: BTreeMap<String, Diagnostic>,
    pub sources: Option<Vec<SnapshotSource>>,
    pub diagnostics: Vec<Diagnostic>,
    /// Resource addresses actually requested by each selector, including absence
    /// and acquisition failures. Metadata and contents are stored once per session.
    pub resolution_reads: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotSource {
    pub resource: String,
    pub extension: String,
}

/// The region payload is interpreted exclusively by external extensions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub domain: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equivalence_key: Option<String>,
    pub identity: Value,
    pub observations: Vec<String>,
    pub content: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    Equal,
    Contains,
    Within,
    Incomparable,
}

pub fn require_version(version: u32) -> Result<()> {
    if version != 1 {
        bail!("unsupported format version {version}; expected 1");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn named_targets_report_missing_names_and_cycles() {
        let named = |name: &str| Target::Named(NamedTarget { name: name.into() });
        let mut names = BTreeMap::from([("a".into(), named("b")), ("b".into(), named("a"))]);
        assert!(
            named("a")
                .expand(&names)
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
        names.remove("b");
        assert!(
            named("a")
                .expand(&names)
                .unwrap_err()
                .to_string()
                .contains("unknown")
        );
        let selector = Selector::parse("md:doc.md#hello").unwrap();
        names.insert("b".into(), Target::Selector(selector.clone()));
        assert_eq!(named("a").expand(&names).unwrap(), selector);
        assert!(serde_json::from_value::<Target>(json!({"name":"a", "typo":1})).is_err());
    }
}
