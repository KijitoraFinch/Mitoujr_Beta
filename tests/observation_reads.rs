use anyhow::Result;
use monika::engine::Engine;
use monika::extensions::process::Host;
use monika::model::RegionSelector;
use monika::operations;
use monika::query::Expression;
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
async fn reads_follow_content_and_preserve_absence_and_dependencies_on_replay() -> Result<()> {
    for split in [false, true] {
        let dir = tempfile::tempdir()?;
        let workspace = workspace(dir.path(), split, 5_000).await?;
        let root = workspace.root.clone();
        let entry = workspace.resource("entry.json").await?;
        let rows = workspace.resource("rows.json").await?;
        let optional = workspace.resource("optional.json").await?;
        fs::write(
            root.join("entry.json"),
            serde_json::to_vec(&json!({"next": rows}))?,
        )?;
        fs::write(root.join("rows.json"), "[1,2,3]")?;
        let target = selector(json!({"mode":"chain", "resource":entry, "optional":optional}));
        let mut host = Host::default();
        let mut engine = Engine::new(Session::new(workspace.clone())?, &mut host)?;
        let region = engine.resolve(&target).await?;
        assert_eq!(region.content["count"], 3);
        assert_eq!(
            region.observations.len(),
            3,
            "includes entry and absent config, not just row content"
        );
        assert!(!engine.session.snapshot.observations[&optional].state.exists);
        assert_eq!(
            engine.session.snapshot.resolution_reads[&serde_json::to_string(&target)?],
            BTreeSet::from([entry, rows, optional])
        );
        let snapshot = engine.session.save().await?;
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
        let mut replay = Engine::new(Session::replay(workspace, &snapshot).await?, &mut host)?;
        assert_eq!(replay.resolve(&target).await?, region);
        assert_eq!(
            acquisitions(&wire(&root)?),
            3,
            "replay must not consult the live provider"
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
    let a = workspace.resource("a.json").await?;
    let b = workspace.resource("b.json").await?;
    let first = selector(json!({"resources":[a]}));
    let second = selector(json!({"resources":[b]}));
    let mut host = Host::default();
    let mut engine = Engine::new(Session::new(workspace.clone())?, &mut host)?;
    let before = engine.resolve(&first).await?;
    engine.observe("unrelated.json").await?;
    engine.resolve(&second).await?;
    let snapshot = engine.session.save().await?;
    assert_eq!(deliveries(&wire(dir.path())?), 2);
    assert!(
        engine
            .session
            .snapshot
            .resolution_reads
            .values()
            .all(|reads| reads.len() == 1)
    );
    let mut replay = Engine::new(Session::replay(workspace, &snapshot).await?, &mut host)?;
    replay.resolve(&second).await?;
    assert_eq!(replay.resolve(&first).await?, before);
    assert_eq!(deliveries(&wire(dir.path())?), 4);
    assert_eq!(acquisitions(&wire(dir.path())?), 3);
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn failed_reads_cannot_be_hidden_and_replay_does_not_retry_them() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let workspace = workspace(dir.path(), true, 5_000).await?;
    let resource = workspace.resource("unavailable.json").await?;
    let target = selector(json!({"mode":"ignore_failure","resource":resource}));
    let mut host = Host::default();
    let mut engine = Engine::new(Session::new(workspace.clone())?, &mut host)?;
    let error = engine.resolve(&target).await.unwrap_err();
    assert!(
        error.to_string().contains("provider unavailable"),
        "{error:#}"
    );
    assert!(engine.session.snapshot.failures.contains_key(&resource));
    assert_eq!(
        engine.session.snapshot.resolution_reads[&serde_json::to_string(&target)?],
        BTreeSet::from([resource])
    );
    let snapshot = engine.session.save().await?;
    fs::write(dir.path().join("good.json"), "[7]")?;
    assert_eq!(
        engine
            .resolve(&RegionSelector::parse("sets:good.json#0")?)
            .await?
            .content["count"],
        1
    );
    let count = acquisitions(&wire(dir.path())?);
    let mut replay = Engine::new(Session::replay(workspace, &snapshot).await?, &mut host)?;
    assert!(
        replay
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
async fn replay_rejects_new_dependencies_even_when_another_selector_observed_them() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let workspace = workspace(dir.path(), false, 5_000).await?;
    fs::write(dir.path().join("a.json"), "[1]")?;
    fs::write(dir.path().join("b.json"), "[2]")?;
    let target = selector(json!({"mode":"changing_reads",
        "resource":workspace.resource("a.json").await?, "extra":workspace.resource("b.json").await?}));
    let mut host = Host::default();
    let mut engine = Engine::new(Session::new(workspace.clone())?, &mut host)?;
    engine.resolve(&target).await?;
    engine.observe("b.json").await?;
    let snapshot = engine.session.save().await?;
    let mut replay = Engine::new(Session::replay(workspace, &snapshot).await?, &mut host)?;
    let error = replay.resolve(&target).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("outside the recorded resolution reads"),
        "{error:#}"
    );
    assert_eq!(acquisitions(&wire(dir.path())?), 2);
    host.shutdown(false).await?;
    Ok(())
}

#[tokio::test]
async fn core_owns_dependency_metadata_and_batch_reads_are_scoped() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let workspace = workspace(dir.path(), false, 5_000).await?;
    fs::write(dir.path().join("a.json"), "[1]")?;
    fs::write(dir.path().join("b.json"), "[2]")?;
    let a = workspace.resource("a.json").await?;
    let b = workspace.resource("b.json").await?;
    let mut host = Host::default();
    let mut engine = Engine::new(Session::new(workspace)?, &mut host)?;
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
        query["resource"] = workspace.resource("waiting.json").await?.into();
        let mut host = Host::default();
        let mut engine = Engine::new(Session::new(workspace)?, &mut host)?;
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
    let target =
        selector(json!({"mode":"chain", "resource":workspace.resource("waiting.json").await?}));
    let mut host = Host::default();
    let mut engine = Engine::new(Session::new(workspace)?, &mut host)?;
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
async fn narrow_query_does_not_acquire_resources_or_start_extensions() -> Result<()> {
    let mut sizes = Vec::new();
    for count in [100, 600] {
        let dir = tempfile::tempdir()?;
        let workspace = workspace(dir.path(), false, 10_000).await?;
        let mut links = Vec::new();
        for i in 0..count {
            // These Resources intentionally do not exist.
            let resource = workspace.resource(&format!("rows-{i:04}.json")).await?;
            let target = json!({"extension":"sets", "resource":resource, "query":"0"});
            links.push(json!({"id":i.to_string(), "source":target, "reference":{"target":target},"annotation":null}));
        }
        fs::write(
            dir.path().join("monika.links.json"),
            serde_json::to_vec(&json!({"version":1,"revision":1,"links":links,"origins":[]}))?,
        )?;
        let mut host = Host::default();
        let mut engine = Engine::new(Session::new(workspace)?, &mut host)?;
        let start = Instant::now();
        let result =
            operations::select(&mut engine, Some(&Expression::parse("id == \"0\"")?)).await?;
        assert!(result.complete);
        assert_eq!(result.links.len(), 1);
        assert_eq!(result.links[0].id, "0");
        assert!(engine.session.snapshot.observations.is_empty());
        assert!(engine.session.snapshot.resolution_reads.is_empty());
        let snapshot = engine.session.save().await?;
        host.shutdown(false).await?;
        assert!(
            !dir.path().join("wire.jsonl").exists(),
            "query started an extension"
        );
        let size = fs::metadata(dir.path().join(".monika/snapshots").join(snapshot))?.len();
        sizes.push(size);
        eprintln!(
            "links={count}, observed=0, metadata_sent=0, snapshot_bytes={size}, elapsed={:?}",
            start.elapsed()
        );
    }
    assert!(
        sizes[1] < sizes[0] * 7,
        "Sidecar snapshot grew superlinearly: {sizes:?}"
    );
    Ok(())
}
