use anyhow::Result;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn run(root: &Path, args: &[&str], request: Option<&Value>) -> Result<(i32, Value)> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_monika"))
        .current_dir(root)
        .arg("--root")
        .arg(root)
        .arg("--json")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(request) = request {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(request)?)?;
    }
    let output = child.wait_with_output()?;
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON output: {error}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    });
    Ok((output.status.code().unwrap(), value))
}

fn setup() -> Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    assert_eq!(run(dir.path(), &["init"], None)?.0, 0);
    let config = json!({
        "version": 1,
        "extensions": {"sets": {
            "command": ["python3", format!("{}/tests/fixtures/sets_extension.py", env!("CARGO_MANIFEST_DIR"))],
            "revision": "test-v1", "schemes": ["file"]
        }},
        "names": {"all": selector("rows.json", 0)},
        "checks": [{"extension": "sets"}]
    });
    fs::write(dir.path().join("monika.json"), serde_json::to_vec(&config)?)?;
    fs::write(dir.path().join("rows.json"), "[1,2,3]")?;
    Ok(dir)
}

fn selector(file: &str, minimum: u32) -> Value {
    json!({"extension": "sets", "resource": file, "query": minimum.to_string()})
}

fn link(id: &str, minimum: u32, annotation: Value) -> Value {
    json!({"id": id, "source": selector("rows.json", 0), "reference": {"target": selector("rows.json", minimum)}, "annotation": annotation})
}

fn add(root: &Path, link: &Value) -> Result<Value> {
    let (code, result) = run(root, &["link", "add", "--link", "-"], Some(link))?;
    assert_eq!(code, 0, "{result}");
    Ok(result)
}

fn stored(root: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(
        root.join("monika.links.json"),
    )?)?)
}

fn configure_variant(root: &Path, name: &str) -> Result<()> {
    let path = root.join("monika.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path)?)?;
    let extension = json!({
        "command": ["python3", format!("{}/tests/fixtures/sets_extension.py", env!("CARGO_MANIFEST_DIR")), name],
        "revision": "test-v1", "schemes": ["file"]
    });
    config["extensions"] = json!({name: extension});
    config["checks"] = json!([]);
    fs::write(path, serde_json::to_vec(&config)?)?;
    Ok(())
}

#[test]
fn conditions_preserve_unknown_matches_and_boolean_dominance() -> Result<()> {
    let dir = setup()?;
    let good = link("good", 2, json!({"relation":"supports", "odd.key":[null]}));
    let mut missing = link("missing", 0, json!({"relation":"broken"}));
    missing["reference"]["target"] = selector("missing.json", 0);
    add(dir.path(), &good)?;
    add(dir.path(), &missing)?;
    let (code, result) = run(
        dir.path(),
        &["links", "--where", r#"target within region("@all")"#],
        None,
    )?;
    assert_eq!(code, 2, "{result}");
    assert_eq!(result["complete"], false);
    assert_eq!(result["data"]["links"].as_array().unwrap().len(), 1);
    let (code, stored) = run(
        dir.path(),
        &[
            "links",
            "--where",
            r#"target == selector("sets:rows.json#2")"#,
        ],
        None,
    )?;
    assert_eq!(code, 0, "{stored}");
    assert_eq!(stored["data"]["links"].as_array().unwrap().len(), 1);
    let (code, contains) = run(
        dir.path(),
        &[
            "links",
            "--where",
            r#"source contains region("sets:rows.json#2") and target != region("sets:rows.json#3")"#,
        ],
        None,
    )?;
    assert_eq!(code, 2, "{contains}");
    assert_eq!(contains["data"]["links"].as_array().unwrap().len(), 1);
    assert_eq!(
        contains["data"]["undetermined"].as_array().unwrap().len(),
        1
    );
    assert_eq!(result["data"]["undetermined"].as_array().unwrap().len(), 1);
    let (code, result) = run(
        dir.path(),
        &[
            "links",
            "--where",
            r#"target within region("@all") and annotation.relation == "supports" and annotation["odd.key"][0] == null"#,
        ],
        None,
    )?;
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["data"]["links"].as_array().unwrap().len(), 1);
    let (code, result) = run(
        dir.path(),
        &[
            "links",
            "--where",
            r#"target within region("@all") or annotation.relation == "broken""#,
        ],
        None,
    )?;
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["data"]["links"].as_array().unwrap().len(), 2);
    let (code, result) = run(
        dir.path(),
        &[
            "links",
            "--where",
            r#"not annotation.absent == null and exists(annotation["odd.key"])"#,
        ],
        None,
    )?;
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["data"]["links"].as_array().unwrap().len(), 1);
    Ok(())
}

#[test]
fn cli_and_json_requests_share_operations_and_snapshots_survive_live_changes() -> Result<()> {
    let dir = setup()?;
    let (code, shown) = run(dir.path(), &["show", "sets:rows.json#2"], None)?;
    assert_eq!(code, 0, "{shown}");
    assert_eq!(shown["data"]["region"]["content"]["count"], 2);
    let snapshot = shown["snapshot"].as_str().unwrap();
    fs::write(dir.path().join("rows.json"), "[0]")?;
    let (code, replay) = run(
        dir.path(),
        &["--snapshot", snapshot, "request"],
        Some(&json!({"op":"show","target":selector("rows.json",2)})),
    )?;
    assert_eq!(code, 0, "{replay}");
    assert_eq!(replay["data"], shown["data"]);
    let (code, latest) = run(dir.path(), &["show", "sets:rows.json#2"], None)?;
    assert_eq!(code, 0, "{latest}");
    assert_eq!(latest["data"]["region"]["content"]["count"], 0);
    fs::write(
        dir.path().join("monika.json"),
        "configuration under editing",
    )?;
    let (code, saved_config) = run(
        dir.path(),
        &["--snapshot", snapshot, "show", "sets:rows.json#2"],
        None,
    )?;
    assert_eq!(code, 0, "{saved_config}");
    assert_eq!(saved_config["data"], shown["data"]);
    Ok(())
}

#[test]
fn annotation_checker_uses_saved_baseline_and_reports_changed_content() -> Result<()> {
    let dir = setup()?;
    let (_, shown) = run(dir.path(), &["show", "sets:rows.json#2"], None)?;
    let declaration = link(
        "baseline",
        2,
        json!({"baseline":{
            "snapshot":shown["snapshot"],"target":selector("rows.json",2)
        }}),
    );
    add(dir.path(), &declaration)?;
    let (code, checked) = run(dir.path(), &["check"], None)?;
    assert_eq!(code, 0, "{checked}");
    fs::write(dir.path().join("rows.json"), "[1,2,3,4]")?;
    let (code, checked) = run(dir.path(), &["check"], None)?;
    assert_eq!(code, 1, "{checked}");
    assert_eq!(
        checked["data"]["findings"][1]["findings"][0]["status"],
        "fail"
    );
    Ok(())
}

#[test]
fn extension_testing_distinguishes_law_violations_from_failed_resolution() -> Result<()> {
    let dir = setup()?;
    let config_path = dir.path().join("monika.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path)?)?;
    let mut broken = config["extensions"]["sets"].clone();
    broken["command"]
        .as_array_mut()
        .unwrap()
        .push(json!("broken"));
    broken["schemes"] = json!([]);
    config["extensions"]["broken"] = broken;
    config["comparators"] = json!({"test.sets.v1":"broken"});
    fs::write(&config_path, serde_json::to_vec(&config)?)?;
    let (code, broken) = run(
        dir.path(),
        &["extension", "test", "--cases", "-"],
        Some(&json!([selector("rows.json", 2)])),
    )?;
    assert_eq!(code, 1, "{broken}");
    assert_eq!(broken["complete"], true);
    assert_eq!(broken["data"]["passed"], false);
    let (code, unresolved) = run(
        dir.path(),
        &["extension", "test", "--cases", "-"],
        Some(&json!([selector("missing.json", 2)])),
    )?;
    assert_eq!(code, 2, "{unresolved}");
    assert_eq!(unresolved["complete"], false);
    Ok(())
}

#[test]
fn core_owns_links_without_extensions_and_cache_deletion_preserves_authority() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (code, init) = run(dir.path(), &["init"], None)?;
    assert_eq!(code, 0, "{init}");
    assert!(dir.path().join("monika.links.json").exists());
    let original = link(
        "first",
        2,
        json!({"日本語":[null, true, {"value":123456789012345678901234567890_u128}]}),
    );
    add(dir.path(), &original)?;
    let data = stored(dir.path())?;
    assert_eq!(data["links"][0]["annotation"], original["annotation"]);
    assert!(
        data["links"][0]["source"]["resource"]
            .as_str()
            .unwrap()
            .starts_with("file:")
    );
    let (code, listed) = run(dir.path(), &["links"], None)?;
    assert_eq!(code, 0, "{listed}");
    assert_eq!(listed["data"]["links"], data["links"]);
    fs::remove_dir_all(dir.path().join(".monika"))?;
    let (code, rebuilt) = run(dir.path(), &["links"], None)?;
    assert_eq!(code, 0, "{rebuilt}");
    assert_eq!(rebuilt["data"]["links"], data["links"]);
    let mut replacement = original.clone();
    replacement["annotation"] = json!([false, {"nested": "preserved"}]);
    let (code, changed) = run(
        dir.path(),
        &["link", "set", "--link", "-"],
        Some(&replacement),
    )?;
    assert_eq!(code, 0, "{changed}");
    assert_eq!(
        stored(dir.path())?["links"][0]["annotation"],
        replacement["annotation"]
    );
    let (code, removed) = run(dir.path(), &["link", "remove", "first"], None)?;
    assert_eq!(code, 0, "{removed}");
    assert_eq!(stored(dir.path())?["links"], json!([]));
    Ok(())
}

#[test]
fn normalization_rejects_duplicate_ids_definitions_and_unknown_named_targets() -> Result<()> {
    let dir = setup()?;
    let mut named = link("one", 0, json!({"n":1}));
    named["reference"]["target"] = json!({"name":"all"});
    add(dir.path(), &named)?;
    let original = stored(dir.path())?;
    for candidate in [
        link("one", 3, json!(null)),
        link("other", 0, json!({"n":1})),
    ] {
        let (code, error) = run(
            dir.path(),
            &["link", "add", "--link", "-"],
            Some(&candidate),
        )?;
        assert_eq!(code, 2, "{error}");
        assert_eq!(stored(dir.path())?, original);
    }
    named["id"] = json!("unknown");
    named["reference"]["target"] = json!({"name":"unknown"});
    assert_eq!(
        run(dir.path(), &["link", "add", "--link", "-"], Some(&named))?.0,
        2
    );
    assert!(
        original["links"][0]["reference"]["target"]
            .get("name")
            .is_none()
    );
    Ok(())
}

#[test]
fn sidecar_snapshots_ignore_later_edits_and_unrelated_external_files() -> Result<()> {
    let dir = setup()?;
    add(dir.path(), &link("one", 0, json!("original")))?;
    fs::write(dir.path().join("unrelated.links"), "this is not JSON")?;
    let (code, before) = run(dir.path(), &["links"], None)?;
    assert_eq!(code, 0, "{before}");
    let snapshot = before["snapshot"].as_str().unwrap();
    run(dir.path(), &["link", "remove", "one"], None)?;
    let (code, replay) = run(dir.path(), &["--snapshot", snapshot, "links"], None)?;
    assert_eq!(code, 0, "{replay}");
    assert_eq!(before["data"], replay["data"]);
    fs::write(dir.path().join("monika.links.json"), "corrupted")?;
    assert_eq!(run(dir.path(), &["links"], None)?.0, 2);
    assert_eq!(
        run(dir.path(), &["--snapshot", snapshot, "links"], None)?.0,
        0
    );
    Ok(())
}

#[test]
fn saved_sidecar_patches_reject_stale_tampered_and_concurrent_state() -> Result<()> {
    let dir = setup()?;
    add(dir.path(), &link("one", 0, json!(null)))?;
    let patch_path = dir.path().join("change.json");
    let (code, prepared) = run(
        dir.path(),
        &[
            "link",
            "remove",
            "one",
            "--patch-out",
            patch_path.to_str().unwrap(),
        ],
        None,
    )?;
    assert_eq!(code, 0, "{prepared}");
    let original_patch = fs::read(&patch_path)?;
    let mut tampered: Value = serde_json::from_slice(&original_patch)?;
    tampered["change"]["after"]["revision"] = json!(999);
    fs::write(&patch_path, serde_json::to_vec(&tampered)?)?;
    assert_eq!(
        run(dir.path(), &["apply", patch_path.to_str().unwrap()], None)?.0,
        2
    );
    fs::write(&patch_path, original_patch)?;
    add(dir.path(), &link("two", 2, json!(null)))?;
    let before = stored(dir.path())?;
    let (code, stale) = run(dir.path(), &["apply", patch_path.to_str().unwrap()], None)?;
    assert_eq!(code, 2, "{stale}");
    assert!(stale["error"].as_str().unwrap().contains("Sidecar changed"));
    assert_eq!(before, stored(dir.path())?);
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.path().join(".monika-sidecar.lock"))?;
    lock.lock()?;
    let (code, locked) = run(dir.path(), &["link", "remove", "one"], None)?;
    assert_eq!(code, 2, "{locked}");
    assert_eq!(before, stored(dir.path())?);
    drop(lock);
    assert_eq!(run(dir.path(), &["link", "remove", "one"], None)?.0, 0);
    Ok(())
}

#[test]
fn import_is_explicit_atomic_idempotent_and_retains_observation_provenance() -> Result<()> {
    let dir = setup()?;
    let original = link("imported", 2, json!({"kind":"from-external"}));
    fs::write(
        dir.path().join("input.links"),
        serde_json::to_vec(&json!({"links":[original]}))?,
    )?;
    assert_eq!(
        run(dir.path(), &["links"], None)?.1["data"]["links"],
        json!([])
    );
    let (code, imported) = run(
        dir.path(),
        &["import", "input.links", "--extension", "sets"],
        None,
    )?;
    assert_eq!(code, 0, "{imported}");
    let before = stored(dir.path())?;
    assert_eq!(before["origins"].as_array().unwrap().len(), 1);
    assert_eq!(before["origins"][0]["location"], json!({"index":0}));
    let observation =
        &before["observations"][before["origins"][0]["observation"].as_str().unwrap()];
    assert_eq!(
        fs::read(
            dir.path()
                .join(".monika/blobs")
                .join(observation["content_hash"].as_str().unwrap())
        )?,
        fs::read(dir.path().join("input.links"))?
    );
    assert_eq!(
        run(
            dir.path(),
            &["import", "input.links", "--extension", "sets"],
            None
        )?
        .0,
        0
    );
    assert_eq!(stored(dir.path())?, before);
    let mut conflicting = original;
    conflicting["annotation"] = json!("changed");
    fs::write(
        dir.path().join("input.links"),
        serde_json::to_vec(&json!({"links":[link("new",3,json!(null)), conflicting]}))?,
    )?;
    let (code, conflict) = run(
        dir.path(),
        &["import", "input.links", "--extension", "sets"],
        None,
    )?;
    assert_eq!(code, 2, "{conflict}");
    assert_eq!(
        stored(dir.path())?,
        before,
        "an import must not partly register candidates"
    );
    fs::remove_file(dir.path().join("input.links"))?;
    assert_eq!(
        run(dir.path(), &["links"], None)?.1["data"]["links"],
        before["links"]
    );
    Ok(())
}

#[test]
fn semantic_duplicates_are_checked_without_merging_link_identity() -> Result<()> {
    let dir = setup()?;
    let first = link("first", 2, json!({"kind":"same"}));
    let mut second = first.clone();
    second["id"] = json!("second");
    second["reference"]["target"] = json!({"extension":"sets","query":{"resources":[url::Url::from_file_path(dir.path().canonicalize()?.join("rows.json")).unwrap().to_string()],"minimum":2}});
    add(dir.path(), &first)?;
    add(dir.path(), &second)?;
    let (code, listed) = run(dir.path(), &["links", "--where", "id == \"first\""], None)?;
    assert_eq!(code, 0, "{listed}");
    assert_eq!(listed["data"]["links"].as_array().unwrap().len(), 1);
    let (code, checked) = run(
        dir.path(),
        &["check", "--duplicates", "--where", "id == \"first\""],
        None,
    )?;
    assert_eq!(code, 1, "{checked}");
    assert_eq!(checked["data"]["duplicates"], json!([["first", "second"]]));
    assert_eq!(stored(dir.path())?["links"].as_array().unwrap().len(), 2);
    Ok(())
}

#[test]
fn generate_preserves_selected_links_and_does_not_update_the_sidecar() -> Result<()> {
    let dir = setup()?;
    add(
        dir.path(),
        &link("one", 1, json!({"nested":[null, {"日本語":false}]})),
    )?;
    add(dir.path(), &link("two", 2, json!([1, 2, 3])))?;
    let authority = stored(dir.path())?;
    let (code, generated) = run(
        dir.path(),
        &[
            "generate",
            "output.links",
            "--extension",
            "sets",
            "--where",
            "id == \"one\"",
        ],
        None,
    )?;
    assert_eq!(code, 0, "{generated}");
    let output: Value = serde_json::from_slice(&fs::read(dir.path().join("output.links"))?)?;
    assert_eq!(output["links"], json!([authority["links"][0]]));
    assert_eq!(stored(dir.path())?, authority);
    configure_variant(dir.path(), "lines")?;
    let (code, generated) = run(
        dir.path(),
        &["generate", "output.jsonl", "--extension", "lines"],
        None,
    )?;
    assert_eq!(code, 0, "{generated}");
    let output: Vec<Value> = fs::read_to_string(dir.path().join("output.jsonl"))?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(json!(output), authority["links"]);
    Ok(())
}

#[test]
fn generation_rejects_annotation_loss_false_commits_and_intervening_creation() -> Result<()> {
    let dir = setup()?;
    add(dir.path(), &link("one", 2, json!({"keep":true})))?;
    configure_variant(dir.path(), "tamper")?;
    let (code, lost) = run(
        dir.path(),
        &["generate", "bad.links", "--extension", "tamper"],
        None,
    )?;
    assert_eq!(code, 2, "{lost}");
    assert!(
        lost["error"]
            .as_str()
            .unwrap()
            .contains("generator changed")
    );
    assert!(!dir.path().join("bad.links").exists());
    configure_variant(dir.path(), "no_write")?;
    fs::write(dir.path().join("bad.links"), r#"{"links":[]}"#)?;
    let (code, unconfirmed) = run(
        dir.path(),
        &["generate", "bad.links", "--extension", "no_write"],
        None,
    )?;
    assert_eq!(code, 2, "{unconfirmed}");
    assert!(
        unconfirmed["error"]
            .as_str()
            .unwrap()
            .contains("does not match committed patch"),
        "{unconfirmed}"
    );
    configure_variant(dir.path(), "sets")?;
    assert_eq!(
        run(
            dir.path(),
            &[
                "generate",
                "new.links",
                "--extension",
                "sets",
                "--patch-out",
                "proposal.json"
            ],
            None
        )?
        .0,
        0
    );
    fs::write(dir.path().join("new.links"), "intervening content")?;
    let (code, stale) = run(dir.path(), &["apply", "proposal.json"], None)?;
    assert_eq!(code, 2, "{stale}");
    assert!(
        stale["error"]
            .as_str()
            .unwrap()
            .contains("Resource changed")
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("new.links"))?,
        "intervening content"
    );
    assert_eq!(
        run(
            dir.path(),
            &["generate", "monika.links.json", "--extension", "sets"],
            None
        )?
        .0,
        2
    );
    Ok(())
}

#[test]
fn missing_resources_are_unknown_checks_and_do_not_hide_stored_links() -> Result<()> {
    let dir = setup()?;
    let mut missing = link("missing", 0, json!(null));
    missing["reference"]["target"] = selector("missing.json", 0);
    add(dir.path(), &missing)?;
    assert_eq!(run(dir.path(), &["links"], None)?.0, 0);
    let (code, checked) = run(dir.path(), &["check"], None)?;
    assert_eq!(code, 2, "{checked}");
    assert_eq!(checked["complete"], false);
    assert_eq!(
        checked["data"]["selection"]["diagnostics"][0]["stage"],
        "resolve"
    );
    Ok(())
}

#[test]
fn removed_configuration_and_cli_forms_are_rejected() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (code, bad) = run(dir.path(), &["link", "copy"], None)?;
    assert_eq!(code, 2, "{bad}");
    fs::write(
        dir.path().join("monika.json"),
        r#"{"version":1,"sources":[]}"#,
    )?;
    let (code, bad) = run(dir.path(), &["links"], None)?;
    assert_eq!(code, 2, "{bad}");
    assert!(bad["error"].as_str().unwrap().contains("unknown field"));
    Ok(())
}

#[test]
fn missing_authority_and_broken_origin_references_do_not_become_empty_results() -> Result<()> {
    let dir = setup()?;
    fs::remove_file(dir.path().join("monika.links.json"))?;
    let (code, missing) = run(dir.path(), &["links"], None)?;
    assert_eq!(code, 2, "{missing}");
    let (code, added) = run(
        dir.path(),
        &["link", "add", "--link", "-"],
        Some(&link("one", 0, json!(null))),
    )?;
    assert_eq!(code, 2, "{added}");
    assert!(!dir.path().join("monika.links.json").exists());
    fs::write(
        dir.path().join("monika.links.json"),
        serde_json::to_vec(&json!({
            "version":1,"revision":1,"links":[],"origins":[{"link":"absent","observation":"absent","extension":"sets","revision":"test-v1","location":0}]
        }))?,
    )?;
    let (code, broken) = run(dir.path(), &["links"], None)?;
    assert_eq!(code, 2, "{broken}");
    assert!(
        broken["error"]
            .as_str()
            .unwrap()
            .contains("missing Observation")
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn generation_paths_use_os_parent_resolution_and_detect_destination_replacement() -> Result<()> {
    use std::os::unix::fs::symlink;
    let dir = setup()?;
    add(dir.path(), &link("one", 2, json!(null)))?;
    fs::create_dir_all(dir.path().join("real/child"))?;
    symlink("real/child", dir.path().join("shortcut"))?;
    let (code, generated) = run(
        dir.path(),
        &[
            "generate",
            "shortcut/../日本語 #%.links",
            "--extension",
            "sets",
        ],
        None,
    )?;
    assert_eq!(code, 0, "{generated}");
    assert!(dir.path().join("real/日本語 #%.links").exists());
    assert!(!dir.path().join("日本語 #%.links").exists());
    let (code, prepared) = run(
        dir.path(),
        &[
            "generate",
            "real/日本語 #%.links",
            "--extension",
            "sets",
            "--patch-out",
            "replacement.json",
        ],
        None,
    )?;
    assert_eq!(code, 0, "{prepared}");
    let destination = dir.path().join("real/日本語 #%.links");
    let saved = fs::read(&destination)?;
    fs::write(dir.path().join("substitute"), &saved)?;
    fs::rename(dir.path().join("substitute"), &destination)?;
    let (code, changed) = run(dir.path(), &["apply", "replacement.json"], None)?;
    assert_eq!(code, 2, "{changed}");
    assert!(
        changed["error"]
            .as_str()
            .unwrap()
            .contains("Resource changed")
    );
    assert_eq!(fs::read(&destination)?, saved);
    Ok(())
}

#[test]
fn executable_documentation_covers_all_commands() -> Result<()> {
    let output = Command::new("python3")
        .arg(format!("{}/examples/demo.py", env!("CARGO_MANIFEST_DIR")))
        .args(["--monika", env!("CARGO_BIN_EXE_monika")])
        .output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn imported_links_share_observation_metadata_and_deletion_removes_only_unused_origins() -> Result<()>
{
    let dir = setup()?;
    fs::write(
        dir.path().join("input.links"),
        serde_json::to_vec(&json!({"links":[
            link("one",1,json!(null)), link("two",2,json!(null))
        ]}))?,
    )?;
    assert_eq!(
        run(
            dir.path(),
            &["import", "input.links", "--extension", "sets"],
            None
        )?
        .0,
        0
    );
    let before = stored(dir.path())?;
    assert_eq!(before["origins"].as_array().unwrap().len(), 2);
    assert_eq!(before["observations"].as_object().unwrap().len(), 1);
    assert_eq!(
        run(
            dir.path(),
            &["import", "input.links", "--extension", "sets"],
            None
        )?
        .0,
        0
    );
    assert_eq!(stored(dir.path())?, before);
    assert_eq!(run(dir.path(), &["link", "remove", "one"], None)?.0, 0);
    let one_left = stored(dir.path())?;
    assert_eq!(one_left["origins"].as_array().unwrap().len(), 1);
    assert_eq!(one_left["observations"], before["observations"]);
    assert_eq!(run(dir.path(), &["link", "remove", "two"], None)?.0, 0);
    let empty = stored(dir.path())?;
    assert_eq!(empty["origins"], json!([]));
    assert_eq!(empty["observations"], json!({}));
    Ok(())
}

#[test]
fn saved_generation_tracks_its_sidecar_basis_and_rejects_a_changed_authority() -> Result<()> {
    let dir = setup()?;
    add(dir.path(), &link("one", 2, json!({"version":1})))?;
    assert_eq!(
        run(
            dir.path(),
            &[
                "generate",
                "out.links",
                "--extension",
                "sets",
                "--patch-out",
                "generate.json"
            ],
            None
        )?
        .0,
        0
    );
    let (code, applied) = run(dir.path(), &["apply", "generate.json"], None)?;
    assert_eq!(code, 0, "{applied}");
    let (code, replay) = run(
        dir.path(),
        &["--snapshot", applied["snapshot"].as_str().unwrap(), "links"],
        None,
    )?;
    assert_eq!(code, 0, "{replay}");
    assert_eq!(
        replay["data"]["links"][0]["annotation"],
        json!({"version":1})
    );
    assert_eq!(
        run(
            dir.path(),
            &[
                "generate",
                "next.links",
                "--extension",
                "sets",
                "--patch-out",
                "stale.json"
            ],
            None
        )?
        .0,
        0
    );
    let changed = link("one", 2, json!({"version":2}));
    assert_eq!(
        run(dir.path(), &["link", "set", "--link", "-"], Some(&changed))?.0,
        0
    );
    let (code, stale) = run(dir.path(), &["apply", "stale.json"], None)?;
    assert_eq!(code, 2, "{stale}");
    assert!(stale["error"].as_str().unwrap().contains("Sidecar changed"));
    assert!(!dir.path().join("next.links").exists());
    Ok(())
}

#[test]
fn human_output_shows_link_identity_annotation_and_import_origin() -> Result<()> {
    let dir = setup()?;
    fs::write(
        dir.path().join("source.links"),
        serde_json::to_vec(&json!({"links":[link("readable",2,json!({"note":"visible"}))]}))?,
    )?;
    assert_eq!(
        run(
            dir.path(),
            &["import", "source.links", "--extension", "sets"],
            None
        )?
        .0,
        0
    );
    let output = Command::new(env!("CARGO_BIN_EXE_monika"))
        .current_dir(dir.path())
        .args(["links"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout)?;
    assert!(
        text.contains("readable")
            && text.contains("visible")
            && text.contains("imported from")
            && text.contains("source.links")
            && text.contains("Observation"),
        "{text}"
    );
    Ok(())
}

#[test]
fn incomplete_import_does_not_register_even_successfully_read_candidates() -> Result<()> {
    let dir = setup()?;
    configure_variant(dir.path(), "partial")?;
    let before = stored(dir.path())?;
    fs::write(
        dir.path().join("partial.links"),
        serde_json::to_vec(&json!({"links":[link("partial",0,json!(null))]}))?,
    )?;
    let (code, failed) = run(
        dir.path(),
        &["import", "partial.links", "--extension", "partial"],
        None,
    )?;
    assert_eq!(code, 2, "{failed}");
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("import was incomplete")
    );
    assert_eq!(stored(dir.path())?, before);
    Ok(())
}
