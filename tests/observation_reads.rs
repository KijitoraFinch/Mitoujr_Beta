use anyhow::Result;
use monika::engine::Engine;
use monika::extensions::process::Host;
use monika::model::RegionSelector;
use monika::operations;
use monika::workspace::{Config, ProcessExtension, Session, Workspace};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::time::{sleep, timeout};

async fn workspace(root: &Path, split_provider: bool, timeout_ms: u64) -> Result<Workspace> {
    let extension = |name: &str, schemes, timeout_ms| ProcessExtension {
        command: vec![
            "python3".into(),
            format!(
                "{}/tests/fixtures/reading_extension.py",
                env!("CARGO_MANIFEST_DIR")
            ),
            name.into(),
        ],
        revision: "test-v1".into(),
        schemes,
        timeout_ms,
    };
    let mut config = Config::default();
    config.extensions.insert(
        "sets".into(),
        extension(
            "sets",
            if split_provider {
                vec![]
            } else {
                vec!["file".into()]
            },
            timeout_ms,
        ),
    );
    if split_provider {
        config.extensions.insert(
            "provider".into(),
            extension("provider", vec!["file".into()], 5_000),
        );
    }
    config
        .comparators
        .insert("test.sets.v1".into(), "sets".into());
    fs::write(root.join("monika.json"), serde_json::to_vec(&config)?)?;
    Workspace::open(root).await
}

fn selector(query: Value) -> RegionSelector {
    RegionSelector {
        extension: "sets".into(),
        resource: None,
        query,
    }
}

fn wire(root: &Path) -> Result<Vec<Value>> {
    fs::read_to_string(root.join("wire.jsonl"))?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn acquisitions(messages: &[Value]) -> usize {
    messages
        .iter()
        .filter(|m| m["message"]["operation"] == "observe")
        .count()
}

fn deliveries(messages: &[Value]) -> usize {
    messages
        .iter()
        .filter_map(|m| m["message"]["read_result"].as_array())
        .map(Vec::len)
        .sum()
}

#[tokio::test]
async fn observation_files_survive_the_session_until_extension_cleanup_finishes() -> Result<()> {
    for mode in ["chain", "fail_after_read", "hold_input"] {
        let dir = tempfile::tempdir()?;
        let workspace = workspace(dir.path(), false, 500).await?;
        fs::write(dir.path().join("rows.json"), "[1,2,3]")?;
        let target = selector(
            json!({"mode":mode,"resource":workspace.normalize_resource("rows.json").await?}),
        );
        let mut host = Host::default();
        let mut engine = Engine::new(Session::new(workspace), &mut host)?;
        let result = engine.resolve(&target).await;
        match mode {
            "chain" => assert_eq!(result?.content["count"], 3),
            "fail_after_read" => assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("interpretation failed")
            ),
            "hold_input" => assert!(result.unwrap_err().to_string().contains("timed out")),
            _ => unreachable!(),
        }
        drop(engine);
        let paths: Vec<String> = wire(dir.path())?
            .iter()
            .filter_map(|entry| entry["message"]["read_result"].as_array())
            .flatten()
            .map(|input| input["content_path"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(paths.len(), 1);
        assert_eq!(fs::read(&paths[0])?, b"[1,2,3]");
        host.shutdown(false).await?;
        if mode != "hold_input" {
            let at_eof: Vec<String> =
                serde_json::from_slice(&fs::read(dir.path().join("sets-eof-inputs.json"))?)?;
            assert_eq!(at_eof, paths);
        }
        assert!(!Path::new(&paths[0]).exists());
        assert!(!dir.path().join(".monika").exists());
    }
    Ok(())
}

#[tokio::test]
async fn reads_follow_content_and_pin_absence_during_the_operation() -> Result<()> {
    for split in [false, true] {
        let dir = tempfile::tempdir()?;
        let workspace = workspace(dir.path(), split, 5_000).await?;
        let root = workspace.root.clone();
        let entry = workspace.normalize_resource("entry.json").await?;
        let rows = workspace.normalize_resource("rows.json").await?;
        let optional = workspace.normalize_resource("optional.json").await?;
        fs::write(
            root.join("entry.json"),
            serde_json::to_vec(&json!({"next": rows}))?,
        )?;
        fs::write(root.join("rows.json"), "[1,2,3]")?;
        let target = selector(json!({"mode":"chain", "resource":entry, "optional":optional}));
        let mut host = Host::default();
        let mut engine = Engine::new(Session::new(workspace.clone()), &mut host)?;
        let region = engine.resolve(&target).await?;
        assert_eq!(region.content["count"], 3);
        assert_eq!(
            region.observations.len(),
            3,
            "includes entry and absent config, not just row content"
        );
        assert!(!engine.session.observations[&optional].state.exists);
        let resources: BTreeSet<_> = engine.session.observations.keys().cloned().collect();
        assert_eq!(resources, BTreeSet::from([entry, rows, optional]));
        let messages = wire(&root)?;
        assert_eq!(acquisitions(&messages), 3);
        assert_eq!(
            deliveries(&messages),
            3,
            "repeated context.read must reuse its input"
        );
        fs::write(root.join("optional.json"), "{\"minimum\":100}")?;
        fs::remove_file(root.join("entry.json"))?;
        fs::write(root.join("rows.json"), "[100]")?;
        assert_eq!(engine.resolve(&target).await?, region);
        assert_eq!(
            acquisitions(&wire(&root)?),
            3,
            "repeated resolution uses the operation’s fixed Observations"
        );
        host.shutdown(false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn unrelated_observations_do_not_enter_resolution_inputs_or_change_the_result() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let workspace = workspace(dir.path(), false, 5_000).await?;
    for name in ["a.json", "b.json", "unrelated.json"] {
        fs::write(dir.path().join(name), "[1,2,3]")?;
    }
    let a = workspace.normalize_resource("a.json").await?;
    let b = workspace.normalize_resource("b.json").await?;
    let first = selector(json!({"resources":[a]}));
    let second = selector(json!({"resources":[b]}));
    let mut host = Host::default();
    let mut engine = Engine::new(Session::new(workspace.clone()), &mut host)?;
    let before = engine.resolve(&first).await?;
    engine.observe("unrelated.json").await?;
    let other = engine.resolve(&second).await?;
    assert_eq!(deliveries(&wire(dir.path())?), 2);
    assert_eq!(before.observations.len(), 1);
    assert_eq!(other.observations.len(), 1);
    assert_ne!(before.observations, other.observations);
    engine.resolve(&second).await?;
    assert_eq!(engine.resolve(&first).await?, before);
    assert_eq!(deliveries(&wire(dir.path())?), 4);
    assert_eq!(acquisitions(&wire(dir.path())?), 3);
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn failed_reads_cannot_be_hidden_or_retried_within_a_session() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let workspace = workspace(dir.path(), true, 5_000).await?;
    let resource = workspace.normalize_resource("unavailable.json").await?;
    let target = selector(json!({"mode":"ignore_failure","resource":resource}));
    let mut host = Host::default();
    let mut engine = Engine::new(Session::new(workspace.clone()), &mut host)?;
    let error = engine.resolve(&target).await.unwrap_err();
    assert!(
        error.to_string().contains("provider unavailable"),
        "{error:#}"
    );
    assert!(engine.session.failures.contains_key(&resource));
    fs::write(dir.path().join("good.json"), "[7]")?;
    assert_eq!(
        engine
            .resolve(&RegionSelector {
                extension: "sets".into(),
                resource: Some("good.json".into()),
                query: json!("0")
            })
            .await?
            .content["count"],
        1
    );
    let count = acquisitions(&wire(dir.path())?);
    assert!(
        engine
            .resolve(&target)
            .await
            .unwrap_err()
            .to_string()
            .contains("provider unavailable")
    );
    assert_eq!(acquisitions(&wire(dir.path())?), count);
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn repeated_resolution_can_change_reads_without_changing_the_region() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let workspace = workspace(dir.path(), false, 5_000).await?;
    fs::write(dir.path().join("a.json"), "[1]")?;
    fs::write(dir.path().join("b.json"), "[2]")?;
    let target = selector(json!({"mode":"changing_reads",
        "resource":workspace.normalize_resource("a.json").await?, "extra":workspace.normalize_resource("b.json").await?}));
    let mut host = Host::default();
    let mut engine = Engine::new(Session::new(workspace), &mut host)?;
    let report = monika::extension_test::test_regions(&mut engine, &[target]).await?;
    assert!(report.passed, "{:?}", report.failures);
    let messages = wire(dir.path())?;
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["message"]["operation"] == "resolve")
            .count(),
        2
    );
    assert_eq!(acquisitions(&messages), 2);
    assert_eq!(deliveries(&messages), 3);
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn extension_cache_tracks_backend_settings_and_current_observations() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let workspace = workspace(dir.path(), false, 5_000).await?;
    let resource = workspace.normalize_resource("rows.json").await?;
    let target = selector(json!({"mode":"cached_project", "resource":resource}));
    let mut host = Host::default();
    let mut previous_id = None;
    // Only change inputs between operations. Keep the same extension process
    // to exercise its cache across fresh Observation sessions as well.
    for (minimum, rows, count) in [
        (0, "[1,2,3]", 3),
        (0, "[1,2,3]", 3),
        (3, "[1,2,3]", 1),
        (3, "[1,2,3,4]", 2),
    ] {
        fs::write(
            dir.path().join("project.json"),
            serde_json::to_vec(&json!({"minimum":minimum}))?,
        )?;
        fs::write(dir.path().join("rows.json"), rows)?;
        let mut engine = Engine::new(Session::new(workspace.clone()), &mut host)?;
        let region = engine.resolve(&target).await?;
        assert_eq!(region.content["count"], count);
        assert_eq!(engine.resolve(&target).await?, region);
        assert_eq!(engine.session.observations.len(), 1);
        let observed = &engine.session.observations[&resource];
        assert_eq!(region.observations, vec![observed.id.clone()]);
        assert_ne!(previous_id.as_ref(), Some(&observed.id));
        for member in region.identity.as_array().unwrap() {
            assert_eq!(member[0], observed.id);
        }
        previous_id = Some(observed.id.clone());
    }
    let hits: Vec<bool> = fs::read_to_string(dir.path().join("cache-events.jsonl"))?
        .lines()
        .map(|line| {
            Ok(serde_json::from_str::<Value>(line)?["hit"]
                .as_bool()
                .unwrap())
        })
        .collect::<Result<_>>()?;
    assert_eq!(hits, [false, true, true, true, false, true, false, true]);
    let messages = wire(dir.path())?;
    assert_eq!(acquisitions(&messages), 4);
    assert_eq!(deliveries(&messages), 8);
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn a_later_resolution_calls_the_extension_after_an_operation_error() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let workspace = workspace(dir.path(), false, 5_000).await?;
    fs::write(dir.path().join("rows.json"), "[1]")?;
    let target = selector(json!({"mode":"transient_failure",
        "resource":workspace.normalize_resource("rows.json").await?}));
    let mut host = Host::default();
    let mut engine = Engine::new(Session::new(workspace), &mut host)?;
    assert!(
        engine
            .resolve(&target)
            .await
            .unwrap_err()
            .to_string()
            .contains("backend not ready")
    );
    assert_eq!(
        wire(dir.path())?
            .iter()
            .filter(|m| m["message"]["operation"] == "resolve")
            .count(),
        1
    );
    assert_eq!(engine.resolve(&target).await?.content["count"], 1);
    assert_eq!(
        wire(dir.path())?
            .iter()
            .filter(|m| m["message"]["operation"] == "resolve")
            .count(),
        2
    );
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn core_records_observation_reads_and_batch_reads_are_scoped() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let workspace = workspace(dir.path(), false, 5_000).await?;
    fs::write(dir.path().join("a.json"), "[1]")?;
    fs::write(dir.path().join("b.json"), "[2]")?;
    let a = workspace.normalize_resource("a.json").await?;
    let b = workspace.normalize_resource("b.json").await?;
    let mut host = Host::default();
    let mut engine = Engine::new(Session::new(workspace), &mut host)?;
    let region = engine
        .resolve(&selector(json!({"resources":[a,b]})))
        .await?;
    assert_eq!(region.observations.len(), 2);
    assert_eq!(deliveries(&wire(dir.path())?), 2);
    let error = engine
        .resolve(&selector(json!({"mode":"forge_dependencies","resource":a})))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("invalid region result"),
        "{error:#}"
    );
    assert_eq!(acquisitions(&wire(dir.path())?), 2);
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn resolution_deadline_includes_other_providers_and_repeated_read_requests() -> Result<()> {
    for query in [json!({"mode":"endless_reads"}), json!({"mode":"chain"})] {
        let dir = tempfile::tempdir()?;
        let workspace = workspace(dir.path(), true, 400).await?;
        let mut query = query;
        query["resource"] = workspace.normalize_resource("waiting.json").await?.into();
        let mut host = Host::default();
        let mut engine = Engine::new(Session::new(workspace), &mut host)?;
        let start = Instant::now();
        let error = timeout(Duration::from_secs(3), engine.resolve(&selector(query)))
            .await?
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(start.elapsed() < Duration::from_secs(2));
        host.shutdown(false).await?;
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_while_a_provider_is_pending_closes_both_processes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let workspace = workspace(dir.path(), true, 5_000).await?;
    let target = selector(
        json!({"mode":"chain", "resource":workspace.normalize_resource("waiting.json").await?}),
    );
    let mut host = Host::default();
    let mut engine = Engine::new(Session::new(workspace), &mut host)?;
    let mut resolving = Box::pin(engine.resolve(&target));
    let waiting = async {
        while !tokio::fs::try_exists(dir.path().join("provider-waiting")).await? {
            sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        result = &mut resolving => panic!("resolution finished before cancellation: {result:?}"),
        result = timeout(Duration::from_secs(3), waiting) => { result??; }
    }
    drop(resolving);
    timeout(Duration::from_secs(3), host.shutdown(false)).await??;
    let pids: BTreeSet<_> = wire(dir.path())?
        .iter()
        .map(|m| m["pid"].as_i64().unwrap() as libc::pid_t)
        .collect();
    assert_eq!(pids.len(), 2);
    for pid in pids {
        // SAFETY: signal zero checks the PID recorded by the test extension.
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "process {pid} survived cancellation"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    Ok(())
}

#[tokio::test]
async fn enumeration_does_not_acquire_resources_or_start_extensions() -> Result<()> {
    for count in [100, 600] {
        let dir = tempfile::tempdir()?;
        let workspace = workspace(dir.path(), false, 10_000).await?;
        let mut links = Vec::new();
        for i in 0..count {
            // These Resources intentionally do not exist.
            let resource = workspace
                .normalize_resource(&format!("rows-{i:04}.json"))
                .await?;
            let target = json!({"extension":"sets", "resource":resource, "query":"0"});
            links.push(
                json!({"id":i.to_string(), "source":target, "target":target,"annotation":null}),
            );
        }
        fs::write(
            dir.path().join("monika.links.json"),
            serde_json::to_vec(&json!({"version":1,"links":links}))?,
        )?;
        let mut host = Host::default();
        let mut engine = Engine::new(Session::new(workspace), &mut host)?;
        let result = operations::links(&mut engine).await?;
        assert_eq!(result.len(), count);
        assert_eq!(result[0].id, "0");
        assert!(engine.session.observations.is_empty());
        host.shutdown(false).await?;
        assert!(
            !dir.path().join("wire.jsonl").exists(),
            "query started an extension"
        );
        assert!(!dir.path().join(".monika").exists());
    }
    Ok(())
}
