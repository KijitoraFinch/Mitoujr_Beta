use crate::engine::Engine;
use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::BTreeSet;

/// Observation reads made by one interpretation. These do not describe all of
/// an extension's inputs: its backend may also use external state.
#[derive(Default)]
pub(crate) struct ReadScope {
    observations: BTreeSet<String>,
    failure: Option<String>,
}

impl ReadScope {
    pub async fn read(
        &mut self,
        engine: &mut Engine<'_>,
        resources: Vec<String>,
    ) -> std::result::Result<Vec<Value>, String> {
        let result: Result<Vec<Value>> = async {
            let mut values = Vec::with_capacity(resources.len());
            for resource in resources {
                let resource = engine
                    .session
                    .workspace
                    .normalize_resource(&resource)
                    .await?;
                let observation = engine.current(&resource).await?;
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
        Ok(self.observations.iter().cloned().collect())
    }
}
