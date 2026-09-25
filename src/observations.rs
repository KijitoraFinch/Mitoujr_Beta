use crate::engine::Engine;
use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::BTreeSet;
use url::Url;

/// Reads made by one interpretation. The session owns the immutable contents;
/// this scope owns only its dependencies, independently of the transport.
pub(crate) struct ReadScope {
    allowed: Option<BTreeSet<String>>,
    pub resources: BTreeSet<String>,
    observations: BTreeSet<String>,
    failure: Option<String>,
}

impl ReadScope {
    pub fn new(allowed: Option<BTreeSet<String>>) -> Self {
        Self {
            allowed,
            resources: BTreeSet::new(),
            observations: BTreeSet::new(),
            failure: None,
        }
    }

    pub async fn read(
        &mut self,
        engine: &mut Engine<'_>,
        resources: Vec<String>,
    ) -> std::result::Result<Vec<Value>, String> {
        let result: Result<Vec<Value>> = async {
            let mut values = Vec::with_capacity(resources.len());
            for resource in resources {
                // Interpretation requests addresses, not paths resolved against
                // the live filesystem when replaying an old observation.
                Url::parse(&resource)?;
                let resource = engine.session.workspace.resource(&resource).await?;
                if let Some(allowed) = &self.allowed {
                    ensure!(
                        allowed.contains(&resource),
                        "resource is outside the recorded resolution reads: {resource}"
                    );
                }
                let observed = engine.observe(&resource).await;
                if observed.is_ok() || engine.session.snapshot.failures.contains_key(&resource) {
                    self.resources.insert(resource);
                }
                let observation = observed?;
                self.observations.insert(observation.id.clone());
                values.push(engine.observation_input(&observation).await?);
            }
            Ok(values)
        }
        .await;
        result.map_err(|error| {
            let message = format!("{error:#}");
            self.failure.get_or_insert(message.clone());
            message
        })
    }

    pub fn finish(&self) -> Result<Vec<String>> {
        ensure!(
            self.failure.is_none(),
            "observation read failed: {}",
            self.failure.as_deref().unwrap_or_default()
        );
        if let Some(allowed) = &self.allowed {
            ensure!(
                &self.resources == allowed,
                "resolution changed its recorded read set"
            );
        }
        Ok(self.observations.iter().cloned().collect())
    }
}
