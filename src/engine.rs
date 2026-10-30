use crate::extensions::process::ResolutionEvent;
use crate::extensions::process::{self, Manifest};
use crate::model::{Diagnostic, Observation, Region, RegionSelector, Relation, ResourceState};
use crate::observations::ReadScope;
use crate::workspace::{ObservationPolicy, Session};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

pub struct Engine<'host> {
    pub session: Session,
    manifests: BTreeMap<String, Manifest>,
    pub(crate) host: &'host mut process::Host,
    failed_manifests: BTreeMap<String, String>,
}

impl<'host> Engine<'host> {
    pub fn new(session: Session, host: &'host mut process::Host) -> Result<Self> {
        let manifests = BTreeMap::new();
        let mut schemes = BTreeSet::new();
        for (name, config) in &session.workspace.config.extensions {
            for scheme in &config.schemes {
                ensure!(
                    schemes.insert(scheme),
                    "multiple providers for resource scheme {scheme}, including {name}"
                );
            }
        }
        Ok(Self {
            session,
            manifests,
            host,
            failed_manifests: BTreeMap::new(),
        })
    }

    pub async fn manifest(&mut self, name: &str) -> Result<Manifest> {
        if let Some(manifest) = self.manifests.get(name) {
            return Ok(manifest.clone());
        }
        if let Some(error) = self.failed_manifests.get(name) {
            bail!("{error}");
        }
        let config = self
            .session
            .workspace
            .config
            .extensions
            .get(name)
            .with_context(|| format!("unknown extension: {name}"))?;
        let result = self
            .host
            .worker(&self.session.workspace.root, name, config)?
            .describe(name, &config.revision)
            .await;
        let manifest = match result {
            Ok(manifest) => manifest,
            Err(error) => {
                self.failed_manifests
                    .insert(name.to_owned(), format!("{error:#}"));
                return Err(error);
            }
        };
        self.manifests.insert(name.to_owned(), manifest.clone());
        Ok(manifest)
    }

    pub async fn manifests(&mut self) -> Result<Vec<Manifest>> {
        let names: Vec<_> = self
            .session
            .workspace
            .config
            .extensions
            .keys()
            .cloned()
            .collect();
        for name in names {
            self.manifest(&name).await?;
        }
        Ok(self.manifests.values().cloned().collect())
    }

    pub async fn call(&mut self, name: &str, operation: &str, input: Value) -> Result<Value> {
        ensure!(
            self.manifest(name).await?.supports(operation),
            "extension {name} does not support {operation}"
        );
        let config = &self.session.workspace.config.extensions[name];
        self.host
            .worker(&self.session.workspace.root, name, config)?
            .invoke(operation, input)
            .await
    }

    /// Select the Resource's current Observation according to its own policy.
    pub async fn current(&mut self, resource: &str) -> Result<Observation> {
        let resource = self.session.workspace.normalize_resource(resource).await?;
        if let Some(observation) = self.session.observations.get(&resource) {
            return Ok(observation.clone());
        }
        if let Some(failure) = self.session.failures.get(&resource) {
            bail!("{}", failure.message);
        }
        if self.session.workspace.observation_policy(&resource) == ObservationPolicy::Observe {
            return self.observe(&resource).await;
        }
        let result: Result<Observation> = async {
            if self.session.saved_current.is_none() {
                self.session.saved_current =
                    Some(crate::storage::current(&self.session.workspace).await?);
            }
            let index = self
                .session
                .saved_current
                .as_ref()
                .expect("current index loaded");
            let id = index.get(&resource).with_context(|| {
                format!(
                    "refreshed Resource has no saved Observation: {resource}; run monika refresh"
                )
            })?;
            let (observation, bytes) =
                crate::storage::observation(&self.session.workspace, id).await?;
            ensure!(
                observation.resource == resource,
                "saved Observation Resource mismatch: {resource}"
            );
            self.session.adopt(observation, bytes)
        }
        .await;
        if let Err(error) = &result {
            self.session.failures.insert(
                resource.clone(),
                Diagnostic {
                    stage: "current".into(),
                    subject: resource,
                    message: format!("{error:#}"),
                },
            );
        }
        result
    }

    pub async fn observe(&mut self, resource: &str) -> Result<Observation> {
        let resource = self.session.workspace.normalize_resource(resource).await?;
        if let Some(observation) = self.session.observations.get(&resource) {
            return Ok(observation.clone());
        }
        if let Some(failure) = self.session.failures.get(&resource) {
            bail!("{}", failure.message);
        }
        let url = self.session.workspace.observation_uri(&resource)?;
        let result = (async {
            let name = self
                .session
                .workspace
                .config
                .extensions
                .iter()
                .find(|(_, config)| config.schemes.iter().any(|scheme| scheme == url.scheme()))
                .map(|(name, _)| name.clone())
                .with_context(|| format!("no resource provider for scheme {}", url.scheme()))?;
            ensure!(
                self.manifest(&name).await?.supports("observe"),
                "extension {name} does not support observe"
            );
            let content_path = self.host.observation_output()?;
            let config = &self.session.workspace.config.extensions[&name];
            let result = self
                .host
                .worker(&self.session.workspace.root, &name, config)?
                .observe(url.as_str(), &content_path)
                .await?;
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Observed {
                state: ResourceState,
            }
            let result: Observed =
                serde_json::from_value(result).context("invalid observe result")?;
            let bytes = tokio::fs::read(&content_path)
                .await
                .context("read observed content")?;
            self.session
                .record(resource.clone(), bytes, name, result.state)
        })
        .await;
        if let Err(error) = &result {
            self.session.failures.insert(
                resource.clone(),
                Diagnostic {
                    stage: "observe".into(),
                    subject: resource,
                    message: format!("{error:#}"),
                },
            );
        }
        result
    }

    pub async fn observation_input(&mut self, observation: &Observation) -> Result<Value> {
        let bytes = self.session.bytes(observation)?;
        self.host.observation_input(observation, bytes).await
    }

    pub async fn resolve(&mut self, selector: &RegionSelector) -> Result<Region> {
        let manifest = self.manifest(&selector.extension).await?;
        ensure!(
            manifest.supports("resolve"),
            "extension {} does not resolve regions",
            selector.extension
        );
        let mut scope = ReadScope::default();
        let config = self.session.workspace.config.extensions[&selector.extension].clone();
        // This deadline covers the whole interpretation, including nested reads
        // served by other processes. A sequence of reads cannot reset it.
        let result = tokio::time::timeout(
            Duration::from_millis(config.timeout_ms),
            self.resolve_with_reads(selector, &mut scope),
        )
        .await;
        let result = match result {
            Ok(result) => result?,
            Err(_) => {
                let error = anyhow::anyhow!(
                    "extension operation resolve timed out after {} ms",
                    config.timeout_ms
                );
                return Err(self
                    .host
                    .worker(&self.session.workspace.root, &selector.extension, &config)?
                    .invalidate(error)
                    .await);
            }
        };
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Resolved {
            domain: String,
            identity: Value,
            content: Value,
        }
        let result: Resolved = serde_json::from_value(result).context("invalid region result")?;
        ensure!(
            manifest.domains.contains(&result.domain),
            "extension returned an undeclared comparison domain: {}",
            result.domain
        );
        Ok(Region {
            domain: result.domain,
            identity: result.identity,
            content: result.content,
            observations: scope.finish()?,
        })
    }

    async fn resolve_with_reads(
        &mut self,
        selector: &RegionSelector,
        scope: &mut ReadScope,
    ) -> Result<Value> {
        let root = self.session.workspace.root.clone();
        let config = self.session.workspace.config.extensions[&selector.extension].clone();
        let mut event = self
            .host
            .worker(&root, &selector.extension, &config)?
            .resolve(serde_json::to_value(selector)?)
            .await?;
        loop {
            match event {
                ResolutionEvent::Complete(region) => return Ok(region),
                ResolutionEvent::Read(resources) => {
                    let observations = scope.read(self, resources).await;
                    event = self
                        .host
                        .worker(&root, &selector.extension, &config)?
                        .resume(observations)
                        .await?;
                }
            }
        }
    }

    pub async fn compare(&mut self, left: &Region, right: &Region) -> Result<Relation> {
        ensure!(
            left.domain == right.domain,
            "no shared comparison domain: {} and {}",
            left.domain,
            right.domain
        );
        let owner = if let Some(name) = self
            .session
            .workspace
            .config
            .comparators
            .get(&left.domain)
            .cloned()
        {
            let manifest = self.manifest(&name).await?;
            ensure!(
                manifest.domains.contains(&left.domain) && manifest.supports("compare"),
                "configured comparator {name} does not compare {}",
                left.domain
            );
            manifest
        } else {
            let mut candidates: Vec<_> = self
                .manifests()
                .await?
                .into_iter()
                .filter(|manifest| {
                    manifest.domains.contains(&left.domain) && manifest.supports("compare")
                })
                .collect();
            ensure!(
                !candidates.is_empty(),
                "no comparison provider for {}",
                left.domain
            );
            ensure!(
                candidates.len() == 1,
                "multiple comparison providers for {}; set comparators in monika.json",
                left.domain
            );
            candidates.remove(0)
        };
        serde_json::from_value(
            self.call(
                &owner.name,
                "compare",
                json!({"left": left, "right": right}),
            )
            .await?,
        )
        .context("invalid comparison result")
    }
}
