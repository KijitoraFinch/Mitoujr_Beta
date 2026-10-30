use anyhow::Result;
use monika::engine::Engine;
use monika::extensions::process;
use monika::model::{RegionSelector, Relation};
use monika::workspace::{Config, ProcessExtension, Session, Workspace};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

fn extension() -> ProcessExtension {
    ProcessExtension {
        command: vec![
            "python3".into(),
            format!(
                "{}/tests/fixtures/sets_extension.py",
                env!("CARGO_MANIFEST_DIR")
            ),
        ],
        revision: "test-v1".into(),
        schemes: vec!["file".into(), "fixture".into()],
        timeout_ms: 5_000,
    }
}

async fn configure(path: &Path) -> Result<Workspace> {
    let mut config = Config::default();
    config.extensions.insert("sets".into(), extension());
    fs::write(path.join("monika.json"), serde_json::to_vec(&config)?)?;
    Workspace::open(path).await
}

#[tokio::test]
async fn real_process_resolves_multiple_observations_and_fresh_sessions_observe_changes()
-> Result<()> {
    let mut host = process::Host::default();
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("a.json"), "[1,2,3]")?;
    fs::write(dir.path().join("b.json"), "[2,4]")?;
    let workspace = configure(dir.path()).await?;
    let a = workspace.normalize_resource("a.json").await?;
    let b = workspace.normalize_resource("b.json").await?;
    let selector = |minimum| RegionSelector {
        extension: "sets".into(),
        resource: None,
        query: json!({"resources": [a, b], "minimum": minimum}),
    };
    let mut engine = Engine::new(Session::new(workspace), &mut host)?;
    let all = engine.resolve(&selector(0)).await?;
    let part = engine.resolve(&selector(3)).await?;
    assert_eq!(all.content["count"], 5);
    assert_eq!(part.content["count"], 2);
    assert_eq!(all.observations.len(), 2);
    assert_eq!(engine.compare(&all, &part).await?, Relation::Contains);
    assert_eq!(engine.compare(&part, &all).await?, Relation::Within);
    fs::write(dir.path().join("a.json"), "[100]")?;
    fs::write(dir.path().join("b.json"), "[200]")?;
    assert_eq!(engine.resolve(&selector(0)).await?, all);
    let mut current = Engine::new(Session::new(Workspace::open(dir.path()).await?), &mut host)?;
    let changed = current.resolve(&selector(0)).await?;
    assert_eq!(changed.content["count"], 2);
    assert_ne!(changed.observations, all.observations);
    assert!(!dir.path().join(".monika").exists());
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn process_observation_freezes_external_resource_and_reports_failure() -> Result<()> {
    let mut host = process::Host::default();
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("external.json"), "[42]")?;
    let workspace = configure(dir.path()).await?;
    let resource = workspace
        .observation_uri("external.json")?
        .as_str()
        .replacen("file:", "fixture:", 1);
    let mut engine = Engine::new(Session::new(workspace), &mut host)?;
    let observed = engine.observe(&resource).await?;
    fs::write(dir.path().join("external.json"), "[24]")?;
    assert_eq!(engine.session.bytes(&observed)?, b"[42]");
    assert_eq!(engine.observe(&resource).await?, observed);
    let missing = engine
        .session
        .workspace
        .observation_uri("missing.json")?
        .as_str()
        .replacen("file:", "fixture:", 1);
    assert!(!engine.observe(&missing).await?.state.exists);
    assert!(
        engine
            .resolve(&RegionSelector {
                extension: "sets".into(),
                resource: None,
                query: json!({"resources":[missing]})
            })
            .await
            .is_err()
    );
    assert!(engine.observe("unsupported:resource").await.is_err());
    assert!(engine.session.failures.contains_key("unsupported:resource"));
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn extension_protocol_rejects_wrong_revision_and_reports_errors_and_timeout() -> Result<()> {
    let mut host = process::Host::default();
    let dir = tempfile::tempdir()?;
    let mut config = extension();
    config.revision = "wrong".into();
    assert!(
        host.worker(dir.path(), "sets", &config)?
            .describe("sets", &config.revision)
            .await
            .unwrap_err()
            .to_string()
            .contains("revision mismatch")
    );
    let error = host
        .worker(dir.path(), "sets", &extension())?
        .invoke("unknown", Value::Null)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("unsupported operation"),
        "{error:#}"
    );
    let config = ProcessExtension {
        command: vec![
            "python3".into(),
            "-c".into(),
            "import time; time.sleep(5)".into(),
        ],
        revision: "1".into(),
        schemes: vec![],
        timeout_ms: 30,
    };
    assert!(
        host.worker(dir.path(), "slow", &config)?
            .invoke("describe", Value::Null)
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn empty_configuration_has_no_builtin_extensions_or_observation_provider() -> Result<()> {
    let mut host = process::Host::default();
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("sample.md"), "[link](somewhere)\n")?;
    let mut engine = Engine::new(Session::new(Workspace::open(dir.path()).await?), &mut host)?;
    assert!(engine.manifests().await?.is_empty());
    assert!(engine.session.sidecar().await.is_err());
    for name in ["file", "text", "md", "sidecar", "rust", "ts", "python"] {
        assert!(
            engine
                .resolve(&RegionSelector {
                    extension: name.into(),
                    resource: Some("sample.md".into()),
                    query: json!(null)
                })
                .await
                .is_err()
        );
    }
    assert!(
        engine
            .observe("sample.md")
            .await
            .unwrap_err()
            .to_string()
            .contains("no resource provider")
    );
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn external_regions_obey_comparison_laws_including_empty_and_multi_resource_sets()
-> Result<()> {
    let mut host = process::Host::default();
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("rows.json"), "[1,2,3,4]")?;
    fs::write(dir.path().join("more.json"), "[2,5]")?;
    let workspace = configure(dir.path()).await?;
    let resource = workspace.normalize_resource("rows.json").await?;
    let other = workspace.normalize_resource("more.json").await?;
    let mut selectors: Vec<_> = [0, 2, 3, 6]
        .into_iter()
        .map(|minimum| RegionSelector {
            extension: "sets".into(),
            resource: None,
            query: json!({"resources": [resource, other], "minimum": minimum}),
        })
        .collect();
    selectors.push(RegionSelector {
        extension: "sets".into(),
        resource: None,
        query: json!({"resources":[resource],"minimum":0}),
    });
    selectors.push(RegionSelector {
        extension: "sets".into(),
        resource: None,
        query: json!({"resources":[other],"minimum":0}),
    });
    selectors.push(RegionSelector {
        extension: "sets".into(),
        resource: None,
        query: json!({"resources":[resource],"minimum":6}),
    });
    let mut engine = Engine::new(Session::new(workspace), &mut host)?;
    let report = monika::extension_test::test_regions(&mut engine, &selectors).await?;
    assert!(report.passed, "{:?}", report.failures);
    assert_eq!(report.comparison_calls, 105);
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn separate_extensions_share_a_comparison_domain_via_explicit_provider() -> Result<()> {
    let mut host = process::Host::default();
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("rows.json"), "[1,2,3]")?;
    let mut workspace = configure(dir.path()).await?;
    let resource = workspace.normalize_resource("rows.json").await?;
    let mut second = extension();
    second.command.push("second".into());
    second.schemes.clear();
    workspace.config.extensions.insert("second".into(), second);
    workspace
        .config
        .comparators
        .insert("test.sets.v1".into(), "sets".into());
    let mut engine = Engine::new(Session::new(workspace), &mut host)?;
    let make_selector = |extension: &str, minimum| RegionSelector {
        extension: extension.into(),
        resource: None,
        query: json!({"resources": [resource], "minimum": minimum}),
    };
    let left = engine.resolve(&make_selector("sets", 0)).await?;
    let alias = engine.resolve(&make_selector("second", 0)).await?;
    let part = engine.resolve(&make_selector("second", 3)).await?;
    assert_eq!(engine.compare(&left, &alias).await?, Relation::Equal);
    assert_eq!(engine.compare(&left, &part).await?, Relation::Contains);
    engine.session.workspace.config.comparators.clear();
    assert!(
        engine
            .compare(&left, &alias)
            .await
            .unwrap_err()
            .to_string()
            .contains("multiple comparison providers")
    );
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn worker_reuses_one_process_and_poisoned_protocol_does_not_restart() -> Result<()> {
    let mut host = process::Host::default();
    let dir = tempfile::tempdir()?;
    let script = "import os,json,sys\nfor line in sys.stdin:\n r=json.loads(line); print(json.dumps({'protocol':1,'id':r['id'],'result':os.getpid()}),flush=True)";
    let config = ProcessExtension {
        command: vec!["python3".into(), "-c".into(), script.into()],
        revision: "1".into(),
        schemes: vec![],
        timeout_ms: 1000,
    };
    let worker = host.worker(dir.path(), "worker", &config)?;
    let first = worker.invoke("ping", Value::Null).await?;
    for _ in 0..20 {
        assert_eq!(worker.invoke("ping", Value::Null).await?, first);
    }
    let bad = ProcessExtension { command: vec!["python3".into(), "-c".into(), "import sys; sys.stdin.readline(); print('{\"protocol\":1,\"id\":999,\"result\":null}',flush=True); sys.stdin.read()".into()], ..config };
    let worker = host.worker(dir.path(), "bad", &bad)?;
    assert!(
        worker
            .invoke("ping", Value::Null)
            .await
            .unwrap_err()
            .to_string()
            .contains("ID mismatch")
    );
    assert!(
        worker
            .invoke("ping", Value::Null)
            .await
            .unwrap_err()
            .to_string()
            .contains("unavailable")
    );
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn worker_timeout_covers_a_blocked_stdin_write() -> Result<()> {
    let mut host = process::Host::default();
    let dir = tempfile::tempdir()?;
    let config = ProcessExtension {
        command: vec![
            "python3".into(),
            "-c".into(),
            "import time; time.sleep(10)".into(),
        ],
        revision: "1".into(),
        schemes: vec![],
        timeout_ms: 50,
    };
    let worker = host.worker(dir.path(), "worker", &config)?;
    let started = std::time::Instant::now();
    let error = worker
        .invoke("blocked", json!("x".repeat(1_000_000)))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error:#}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    host.shutdown(false).await?;
    Ok(())
}
