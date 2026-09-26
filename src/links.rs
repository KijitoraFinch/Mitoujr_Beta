use crate::engine::Engine;
use crate::model::{Region, RegionSelector};
use anyhow::Result;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Endpoint {
    Resolved {
        selector: RegionSelector,
        region: Region,
    },
    Unresolved {
        selector: Option<RegionSelector>,
        error: String,
    },
}

impl Endpoint {
    pub fn region(&self) -> Result<&Region> {
        match self {
            Self::Resolved { region, .. } => Ok(region),
            Self::Unresolved { error, .. } => Err(anyhow::anyhow!("unresolved region: {error}")),
        }
    }

    pub async fn resolve(engine: &mut Engine<'_>, selector: RegionSelector) -> Self {
        match engine.resolve(&selector).await {
            Ok(region) => Self::Resolved { selector, region },
            Err(error) => Self::Unresolved {
                selector: Some(selector),
                error: format!("{error:#}"),
            },
        }
    }
}
