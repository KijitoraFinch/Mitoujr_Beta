use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegionSelector {
    pub extension: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default)]
    pub query: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Target {
    Named(NamedTarget),
    RegionSelector(RegionSelector),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedTarget {
    pub name: String,
}

impl Target {
    pub fn expand(&self, names: &BTreeMap<String, Target>) -> Result<RegionSelector> {
        let mut current = self;
        let mut visited = Vec::new();
        loop {
            match current {
                Self::RegionSelector(selector) => return Ok(selector.clone()),
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

/// A Link endpoint keeps its Capture separate from the selector being expanded.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LinkEnd {
    #[serde(flatten)]
    pub target: Target,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture: Option<String>,
}

impl<'de> Deserialize<'de> for LinkEnd {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = Value::deserialize(deserializer)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| serde::de::Error::custom("expected a Target object"))?;
        let capture = object
            .remove("capture")
            .map(serde_json::from_value)
            .transpose()
            .map_err(serde::de::Error::custom)?;
        let target = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self { target, capture })
    }
}

impl From<Target> for LinkEnd {
    fn from(target: Target) -> Self {
        Self {
            target,
            capture: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub selector: RegionSelector,
    pub region: Region,
    pub observations: BTreeMap<String, Observation>,
    pub captured_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub id: String,
    pub source: LinkEnd,
    pub target: LinkEnd,
    #[serde(default)]
    pub annotation: Value,
}

impl Link {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "empty Link ID");
        for target in [&self.source, &self.target] {
            match &target.target {
                Target::Named(named) => ensure!(!named.name.is_empty(), "empty named target"),
                Target::RegionSelector(selector) => {
                    ensure!(!selector.extension.is_empty(), "empty selector extension")
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    pub fn validate_metadata(&self) -> Result<()> {
        let mut value = self.clone();
        value.id.clear();
        ensure!(
            digest(&serde_json::to_vec(&value)?) == self.id,
            "corrupt Observation metadata: {}",
            self.id
        );
        Ok(())
    }

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

/// The region payload is interpreted exclusively by external extensions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub domain: String,
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
        let selector = RegionSelector {
            extension: "md".into(),
            resource: Some("doc.md".into()),
            query: json!("hello"),
        };
        names.insert("b".into(), Target::RegionSelector(selector.clone()));
        assert_eq!(named("a").expand(&names).unwrap(), selector);
        assert!(serde_json::from_value::<Target>(json!({"name":"a", "typo":1})).is_err());
    }
}
