use crate::extensions::process::ResolutionEvent;
use crate::extensions::process::{self, Manifest};
use crate::model::{Diagnostic, Observation, Region, RegionSelector, Relation, ResourceState};
use crate::observations::ReadScope;
use crate::workspace::Session;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use url::Url;

pub struct Engine<'host> {
    pub session: Session,
    manifests: BTreeMap<String, Manifest>,
    resolutions: BTreeMap<String, std::result::Result<Region, String>>,
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
            resolutions: BTreeMap::new(),
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

    pub async fn observe(&mut self, resource: &str) -> Result<Observation> {
        let resource = self.session.workspace.resource(resource).await?;
        if let Some(observation) = self.session.snapshot.observations.get(&resource) {
            return Ok(observation.clone());
        }
        if let Some(failure) = self.session.snapshot.failures.get(&resource) {
            bail!("{}", failure.message);
        }
        ensure!(
            !self.session.frozen,
            "resource was not observed in this snapshot: {resource}"
        );
        let url = Url::parse(&resource)?;
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
            let config = &self.session.workspace.config.extensions[&name];
            let result = self
                .host
                .worker(&self.session.workspace.root, &name, config)?
                .observe(&resource)
                .await?;
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Observed {
                bytes: Vec<u8>,
                state: ResourceState,
            }
            let result: Observed =
                serde_json::from_value(result).context("invalid observe result")?;
            self.session
                .record(resource.clone(), result.bytes, name, result.state)
                .await
        })
        .await;
        if let Err(error) = &result {
            self.session.snapshot.failures.insert(
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
        self.session.bytes(observation).await?;
        Ok(
            json!({"observation": observation, "content_path": self.session.workspace.blob_path(observation)?}),
        )
    }

    pub async fn resolve(&mut self, selector: &RegionSelector) -> Result<Region> {
        let key = serde_json::to_string(&selector)?;
        if let Some(result) = self.resolutions.get(&key) {
            return result.clone().map_err(anyhow::Error::msg);
        }
        let result = self
            .resolve_uncached(selector)
            .await
            .map_err(|error| format!("{error:#}"));
        self.resolutions.insert(key, result.clone());
        result.map_err(anyhow::Error::msg)
    }

    pub(crate) async fn resolve_uncached(&mut self, selector: &RegionSelector) -> Result<Region> {
        let manifest = self.manifest(&selector.extension).await?;
        ensure!(
            manifest.supports("resolve"),
            "extension {} does not resolve regions",
            selector.extension
        );
        let key = serde_json::to_string(selector)?;
        let mut scope = ReadScope::new(self.session.snapshot.resolution_reads.get(&key).cloned());
        let config = self.session.workspace.config.extensions[&selector.extension].clone();
        // This deadline covers the whole interpretation, including nested reads
        // served by other processes. A sequence of reads cannot reset it.
        let result = tokio::time::timeout(
            Duration::from_millis(config.timeout_ms),
            self.resolve_with_reads(selector, &mut scope),
        )
        .await;
        self.session
            .snapshot
            .resolution_reads
            .entry(key)
            .or_insert_with(|| scope.resources.clone());
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
