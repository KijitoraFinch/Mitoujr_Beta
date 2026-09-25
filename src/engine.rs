use crate::extensions::process::ResolutionEvent;
use crate::extensions::{
    Extraction,
    process::{self, Manifest},
};
use crate::model::{
    Diagnostic, Observation, Region, Relation, ResourceState, Selector, SnapshotSource,
};
use crate::observations::ReadScope;
use crate::workspace::Session;
use anyhow::{Context, Result, bail, ensure};
use globset::Glob;
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

    pub async fn resolve(&mut self, selector: &Selector) -> Result<Region> {
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

    pub(crate) async fn resolve_uncached(&mut self, selector: &Selector) -> Result<Region> {
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
            #[serde(default)]
            equivalence_key: Option<String>,
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
            equivalence_key: result.equivalence_key,
            identity: result.identity,
            content: result.content,
            observations: scope.finish()?,
        })
    }

    async fn resolve_with_reads(
        &mut self,
        selector: &Selector,
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

    pub async fn sources(&mut self) -> Result<Vec<SnapshotSource>> {
        if let Some(sources) = &self.session.snapshot.sources {
            return Ok(sources.clone());
        }
        ensure!(
            !self.session.frozen,
            "this snapshot did not include a source scan"
        );
        let workspace = &self.session.workspace;
        let matchers: Vec<_> = workspace
            .config
            .sources
            .iter()
            .map(|source| {
                Ok((
                    Glob::new(&source.pattern)?.compile_matcher(),
                    source.extension.clone(),
                ))
            })
            .collect::<Result<_>>()?;
        let mut sources = Vec::new();
        for source in &workspace.config.resources {
            sources.push(SnapshotSource {
                resource: workspace.resource(&source.resource).await?,
                extension: source.extension.clone(),
            });
        }
        let mut diagnostics = Vec::new();
        if !matchers.is_empty() {
            let root = workspace.root.clone();
            let entries = tokio::task::spawn_blocking(move || {
                ignore::WalkBuilder::new(&root)
                    .hidden(false)
                    .git_global(false)
                    .require_git(false)
                    .filter_entry(move |entry| {
                        entry.path() != root.join(".monika") && entry.path() != root.join(".git")
                    })
                    .build()
                    .collect::<Vec<_>>()
            })
            .await?;
            for entry in entries {
                match entry {
                    Ok(entry) => {
                        if let Some(error) = entry.error() {
                            diagnostics.push(Diagnostic {
                                stage: "scan".into(),
                                subject: entry.path().display().to_string(),
                                message: error.to_string(),
                            });
                        }
                        if !entry
                            .file_type()
                            .is_some_and(|kind| kind.is_file() || kind.is_symlink())
                        {
                            continue;
                        }
                        let relative = entry.path().strip_prefix(&workspace.root)?;
                        for (matcher, extension) in &matchers {
                            if matcher.is_match(relative) {
                                sources.push(SnapshotSource {
                                    resource: workspace.path_resource(entry.path()).await?,
                                    extension: extension.clone(),
                                });
                            }
                        }
                    }
                    Err(error) => diagnostics.push(Diagnostic {
                        stage: "scan".into(),
                        subject: workspace.root.display().to_string(),
                        message: error.to_string(),
                    }),
                }
            }
        }
        sources.sort_by(|a, b| (&a.resource, &a.extension).cmp(&(&b.resource, &b.extension)));
        sources.dedup_by(|a, b| a.resource == b.resource && a.extension == b.extension);
        self.session.snapshot.diagnostics.extend(diagnostics);
        self.session.snapshot.sources = Some(sources.clone());
        Ok(sources)
    }

    pub async fn extract(&mut self) -> Result<Extraction> {
        let sources = self.sources().await?;
        let mut result = Extraction {
            occurrences: Vec::new(),
            diagnostics: self.session.snapshot.diagnostics.clone(),
        };
        for source in sources {
            let extracted = (async {
                ensure!(
                    self.manifest(&source.extension).await?.supports("extract"),
                    "extension {} does not extract declarations",
                    source.extension
                );
                let observation = self.observe(&source.resource).await?;
                ensure!(
                    observation.state.exists,
                    "resource does not exist: {}",
                    observation.resource
                );
                let input = self.observation_input(&observation).await?;
                let extraction: Extraction =
                    serde_json::from_value(self.call(&source.extension, "extract", input).await?)?;
                for occurrence in &extraction.occurrences {
                    occurrence.declaration.validate()?;
                    ensure!(
                        occurrence.evidence.observation == observation.id
                            && occurrence.evidence.resource == observation.resource
                            && occurrence.evidence.extension == source.extension,
                        "extension {} returned evidence outside the supplied observation",
                        source.extension
                    );
                }
                Ok(extraction)
            })
            .await;
            match extracted {
                Ok(extracted) => {
                    result.occurrences.extend(extracted.occurrences);
                    result.diagnostics.extend(extracted.diagnostics);
                }
                Err(error) => result.diagnostics.push(Diagnostic {
                    stage: "extract".into(),
                    subject: source.resource,
                    message: format!("{error:#}"),
                }),
            }
        }
        Ok(result)
    }
}
