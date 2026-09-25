use anyhow::Result;
use monika::engine::Engine;
use monika::extensions::process;
use monika::model::{Relation, Selector};
use monika::workspace::{Config, ProcessExtension, Session, Source, Workspace};
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

fn configure(path: &Path) -> Result<Workspace> {
    let mut config = Config::default();
    config.extensions.insert("sets".into(), extension());
    fs::write(path.join("monika.json"), serde_json::to_vec(&config)?)?;
    Workspace::open(path)
}

#[test]
fn real_process_resolves_multiple_observations_and_replays_saved_content() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("a.json"), "[1,2,3]")?;
    fs::write(dir.path().join("b.json"), "[2,4]")?;
    let workspace = configure(dir.path())?;
    let a = workspace.resource("a.json")?;
    let b = workspace.resource("b.json")?;
    let selector = |minimum| Selector {
        extension: "sets".into(),
        resource: None,
        query: json!({"resources": [a, b], "minimum": minimum}),
    };
    let mut engine = Engine::new(Session::new(workspace)?)?;
    let all = engine.resolve(&selector(0))?;
    let part = engine.resolve(&selector(3))?;
    assert_eq!(all.content["count"], 5);
    assert_eq!(part.content["count"], 2);
    assert_eq!(all.observations.len(), 2);
    assert_eq!(engine.compare(&all, &part)?, Relation::Contains);
    assert_eq!(engine.compare(&part, &all)?, Relation::Within);
    let snapshot = engine.session.save()?;
    fs::write(dir.path().join("a.json"), "[100]")?;
    fs::remove_file(dir.path().join("b.json"))?;
    let mut replay = Engine::new(Session::replay(Workspace::open(dir.path())?, &snapshot)?)?;
    assert_eq!(replay.resolve(&selector(0))?, all);
    assert_eq!(replay.resolve(&selector(3))?, part);
    assert_eq!(replay.compare(&all, &part)?, Relation::Contains);
    Ok(())
}

#[test]
fn process_observation_freezes_external_resource_and_reports_failure() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("external.json"), "[42]")?;
    let workspace = configure(dir.path())?;
    let resource = workspace
        .resource("external.json")?
        .replacen("file:", "fixture:", 1);
    let mut engine = Engine::new(Session::new(workspace)?)?;
    let observed = engine.observe(&resource)?;
    fs::write(dir.path().join("external.json"), "[24]")?;
    assert_eq!(engine.session.bytes(&observed)?, b"[42]");
    assert_eq!(engine.observe(&resource)?, observed);
    let missing = engine
        .session
        .workspace
        .resource("missing.json")?
        .replacen("file:", "fixture:", 1);
    assert!(!engine.observe(&missing)?.state.exists);
    assert!(
        engine
            .resolve(&Selector {
                extension: "sets".into(),
                resource: None,
                query: json!({"resources":[missing]})
            })
            .is_err()
    );
    assert!(engine.observe("unsupported:resource").is_err());
    assert!(
        engine
            .session
            .snapshot
            .failures
            .contains_key("unsupported:resource")
    );
    Ok(())
}

#[test]
fn extension_protocol_rejects_wrong_revision_and_reports_errors_and_timeout() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = extension();
    config.revision = "wrong".into();
    assert!(
        process::describe(dir.path(), "sets", &config)
            .unwrap_err()
            .to_string()
            .contains("revision mismatch")
    );
    let error = process::invoke(dir.path(), &extension(), "unknown", Value::Null).unwrap_err();
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
        process::invoke(dir.path(), &config, "describe", Value::Null)
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
    Ok(())
}

#[test]
fn snapshot_reuses_source_inventory_instead_of_scanning_live_files() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut workspace = configure(dir.path())?;
    workspace.config.sources.push(Source {
        pattern: "*.links".into(),
        extension: "sets".into(),
    });
    fs::write(
        dir.path().join("monika.json"),
        serde_json::to_vec(&workspace.config)?,
    )?;
    let document = json!({"links": [{
        "source": {"extension": "sets", "query": {"resources": [], "minimum": 0}},
        "reference": {"target": {"extension": "sets", "query": {"resources": [], "minimum": 1}}},
        "annotation": {"arbitrary": [true, {"日本語": null}]}
    }]});
    fs::write(
        dir.path().join("original.links"),
        serde_json::to_vec(&document)?,
    )?;
    let mut engine = Engine::new(Session::new(workspace)?)?;
    let original = engine.extract()?;
    assert!(
        original.diagnostics.is_empty(),
        "{:?}",
        original.diagnostics
    );
    assert_eq!(original.occurrences.len(), 1);
    let snapshot = engine.session.save()?;
    fs::remove_file(dir.path().join("original.links"))?;
    fs::write(
        dir.path().join("added.links"),
        serde_json::to_vec(&document)?,
    )?;
    let mut replay = Engine::new(Session::replay(Workspace::open(dir.path())?, &snapshot)?)?;
    assert_eq!(replay.extract()?.occurrences, original.occurrences);
    Ok(())
}

#[test]
fn empty_configuration_has_no_builtin_extensions_or_observation_provider() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("sample.md"), "[link](somewhere)\n")?;
    let mut engine = Engine::new(Session::new(Workspace::open(dir.path())?)?)?;
    assert!(engine.manifests()?.is_empty());
    assert!(engine.sources()?.is_empty());
    assert!(engine.extract()?.occurrences.is_empty());
    for name in ["file", "text", "md", "sidecar", "rust", "ts", "python"] {
        assert!(
            engine
                .resolve(&Selector::parse(&format!("{name}:sample.md"))?)
                .is_err()
        );
    }
    assert!(
        engine
            .observe("sample.md")
            .unwrap_err()
            .to_string()
            .contains("no resource provider")
    );
    Ok(())
}

#[test]
fn external_regions_obey_comparison_laws_including_empty_and_multi_resource_sets() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("rows.json"), "[1,2,3,4]")?;
    fs::write(dir.path().join("more.json"), "[2,5]")?;
    let workspace = configure(dir.path())?;
    let resource = workspace.resource("rows.json")?;
    let other = workspace.resource("more.json")?;
    let mut selectors: Vec<_> = [0, 2, 3, 6]
        .into_iter()
        .map(|minimum| Selector {
            extension: "sets".into(),
            resource: None,
            query: json!({"resources": [resource, other], "minimum": minimum}),
        })
        .collect();
    selectors.push(Selector {
        extension: "sets".into(),
        resource: None,
        query: json!({"resources":[resource],"minimum":0}),
    });
    selectors.push(Selector {
        extension: "sets".into(),
        resource: None,
        query: json!({"resources":[other],"minimum":0}),
    });
    selectors.push(Selector {
        extension: "sets".into(),
        resource: None,
        query: json!({"resources":[resource],"minimum":6}),
    });
    let mut engine = Engine::new(Session::new(workspace)?)?;
    let report = monika::extension_test::test_regions(&mut engine, &selectors)?;
    assert!(report.passed, "{:?}", report.failures);
    assert_eq!(report.comparison_calls, 98);
    Ok(())
}

#[test]
fn separate_extensions_share_a_comparison_domain_via_explicit_provider() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("rows.json"), "[1,2,3]")?;
    let mut workspace = configure(dir.path())?;
    let resource = workspace.resource("rows.json")?;
    let mut second = extension();
    second.command.push("second".into());
    second.schemes.clear();
    workspace.config.extensions.insert("second".into(), second);
    workspace
        .config
        .comparators
        .insert("test.sets.v1".into(), "sets".into());
    let mut engine = Engine::new(Session::new(workspace)?)?;
    let make_selector = |extension: &str, minimum| Selector {
        extension: extension.into(),
        resource: None,
        query: json!({"resources": [resource], "minimum": minimum}),
    };
    let left = engine.resolve(&make_selector("sets", 0))?;
    let alias = engine.resolve(&make_selector("second", 0))?;
    let part = engine.resolve(&make_selector("second", 3))?;
    assert_eq!(engine.compare(&left, &alias)?, Relation::Equal);
    assert_eq!(engine.compare(&left, &part)?, Relation::Contains);
    engine.session.workspace.config.comparators.clear();
    assert!(
        engine
            .compare(&left, &alias)
            .unwrap_err()
            .to_string()
            .contains("multiple comparison providers")
    );
    Ok(())
}

#[test]
fn worker_reuses_one_process_and_poisoned_protocol_does_not_restart() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let script = "import os,json,sys\nfor line in sys.stdin:\n r=json.loads(line); print(json.dumps({'protocol':1,'id':r['id'],'result':os.getpid()}),flush=True)";
    let config = ProcessExtension {
        command: vec!["python3".into(), "-c".into(), script.into()],
        revision: "1".into(),
        schemes: vec![],
        timeout_ms: 1000,
    };
    let mut worker = process::Worker::spawn(dir.path(), &config)?;
    let first = worker.invoke("ping", Value::Null)?;
    for _ in 0..20 {
        assert_eq!(worker.invoke("ping", Value::Null)?, first);
    }
    let bad = ProcessExtension { command: vec!["python3".into(), "-c".into(), "import sys; sys.stdin.readline(); print('{\"protocol\":1,\"id\":999,\"result\":null}',flush=True); sys.stdin.read()".into()], ..config };
    let mut worker = process::Worker::spawn(dir.path(), &bad)?;
    assert!(
        worker
            .invoke("ping", Value::Null)
            .unwrap_err()
            .to_string()
            .contains("ID mismatch")
    );
    assert!(
        worker
            .invoke("ping", Value::Null)
            .unwrap_err()
            .to_string()
            .contains("unavailable")
    );
    Ok(())
}

#[test]
fn worker_timeout_covers_a_blocked_stdin_write() -> Result<()> {
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
    let mut worker = process::Worker::spawn(dir.path(), &config)?;
    let started = std::time::Instant::now();
    let error = worker
        .invoke("blocked", json!("x".repeat(1_000_000)))
        .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error:#}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    Ok(())
}
