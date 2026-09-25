use anyhow::Result;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn run(root: &Path, args: &[&str], request: Option<&Value>) -> Result<(i32, Value)> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_monika"))
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
    let config = json!({
        "version": 1,
        "extensions": {"sets": {
            "command": ["python3", format!("{}/tests/fixtures/sets_extension.py", env!("CARGO_MANIFEST_DIR"))],
            "revision": "test-v1", "schemes": ["file"]
        }},
        "sources": [{"pattern": "*.links", "extension": "sets"}],
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

fn declaration(id: &str, minimum: u32, annotation: Value) -> Value {
    json!({"id": id, "source": selector("rows.json", 0), "reference": {"target": selector("rows.json", minimum)}, "annotation": annotation})
}

fn write_links(root: &Path, file: &str, declarations: Vec<Value>) -> Result<()> {
    fs::write(
        root.join(file),
        serde_json::to_vec(&json!({"links": declarations}))?,
    )?;
    Ok(())
}

#[test]
fn cli_groups_semantic_aliases_preserves_evidence_and_reports_conflicts() -> Result<()> {
    let dir = setup()?;
    let annotation = json!({"relation": "supports", "odd.key": [null, true]});
    let first = declaration("shared", 2, annotation.clone());
    let mut alias = first.clone();
    alias["reference"]["target"] = json!({"extension":"sets","query":{
        "resources":[url::Url::from_file_path(dir.path().canonicalize()?.join("rows.json")).unwrap().to_string()], "minimum":2
    }});
    write_links(
        dir.path(),
        "one.links",
        vec![
            first,
            declaration("shared", 3, json!({"relation":"contradicts"})),
        ],
    )?;
    write_links(dir.path(), "two.links", vec![alias])?;
    let (code, result) = run(dir.path(), &["links"], None)?;
    assert_eq!(code, 0, "{result}");
    let links = result["data"]["links"].as_array().unwrap();
    assert_eq!(links.len(), 2);
    assert_eq!(links[0]["occurrences"].as_array().unwrap().len(), 2);
    assert_eq!(
        links[0]["occurrences"][0]["declaration"]["annotation"],
        annotation
    );
    assert_eq!(result["data"]["conflicts"][0]["id"], "shared");
    assert_ne!(
        links[0]["occurrences"][0]["evidence"]["resource"],
        links[0]["source"]["selector"]["resource"]
    );
    let (code, checked) = run(dir.path(), &["check"], None)?;
    assert_eq!(code, 1, "{checked}");
    assert_eq!(checked["data"]["valid"], false);
    Ok(())
}

#[test]
fn conditions_preserve_unknown_matches_and_boolean_dominance() -> Result<()> {
    let dir = setup()?;
    let good = declaration("good", 2, json!({"relation":"supports", "odd.key":[null]}));
    let mut missing = declaration("missing", 0, json!({"relation":"broken"}));
    missing["reference"]["target"] = selector("missing.json", 0);
    write_links(dir.path(), "links.links", vec![good, missing])?;
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
    let declaration = declaration(
        "baseline",
        2,
        json!({"baseline":{
            "snapshot":shown["snapshot"],"target":selector("rows.json",2)
        }}),
    );
    write_links(dir.path(), "links.links", vec![declaration])?;
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
fn complete_empty_and_failed_extraction_have_distinct_statuses() -> Result<()> {
    let dir = setup()?;
    let (code, empty) = run(dir.path(), &["links"], None)?;
    assert_eq!(code, 0, "{empty}");
    assert_eq!(empty["complete"], true);
    fs::write(dir.path().join("broken.links"), "not json")?;
    let (code, broken) = run(dir.path(), &["links"], None)?;
    assert_eq!(code, 2, "{broken}");
    assert_eq!(broken["complete"], false);
    assert_eq!(broken["data"]["diagnostics"].as_array().unwrap().len(), 1);
    let (code, malformed) = run(dir.path(), &["links", "--where", "source == 3"], None)?;
    assert_eq!(code, 2, "{malformed}");
    assert_eq!(malformed["ok"], false);
    Ok(())
}

#[test]
fn sidecar_annotations_are_added_replaced_and_removed_in_the_original_resource() -> Result<()> {
    let dir = setup()?;
    let initial = declaration(
        "editable",
        2,
        json!({"review":{"owner":"利用者","labels":[1,null,true]}}),
    );
    let (code, added) = run(
        dir.path(),
        &[
            "link",
            "add",
            "--in",
            "new.links",
            "--extension",
            "sets",
            "--declaration",
            "-",
        ],
        Some(&initial),
    )?;
    assert_eq!(code, 0, "{added}");
    let saved: Value = serde_json::from_slice(&fs::read(dir.path().join("new.links"))?)?;
    assert_eq!(saved["links"][0], initial);
    let updated = declaration("editable", 3, json!([{"自由な項目":"値"}, false, null]));
    let (code, changed) = run(
        dir.path(),
        &[
            "link",
            "set",
            "--in",
            "new.links",
            "--extension",
            "sets",
            "--id",
            "editable",
            "--declaration",
            "-",
        ],
        Some(&updated),
    )?;
    assert_eq!(code, 0, "{changed}");
    let saved: Value = serde_json::from_slice(&fs::read(dir.path().join("new.links"))?)?;
    assert_eq!(saved["links"][0], updated);
    let (code, removed) = run(
        dir.path(),
        &[
            "link",
            "remove",
            "--in",
            "new.links",
            "--extension",
            "sets",
            "--id",
            "editable",
        ],
        None,
    )?;
    assert_eq!(code, 0, "{removed}");
    let saved: Value = serde_json::from_slice(&fs::read(dir.path().join("new.links"))?)?;
    assert_eq!(saved["links"], json!([]));
    Ok(())
}

#[test]
fn saved_patches_are_reviewable_and_reject_stale_or_tampered_content() -> Result<()> {
    let dir = setup()?;
    let original = declaration("old", 1, json!({"preserve":true}));
    write_links(dir.path(), "existing.links", vec![original.clone()])?;
    let to_add = declaration("new", 2, Value::Null);
    let (code, prepared) = run(
        dir.path(),
        &[
            "link",
            "add",
            "--in",
            "existing.links",
            "--extension",
            "sets",
            "--declaration",
            "-",
            "--patch-out",
            "change.patch.json",
        ],
        Some(&to_add),
    )?;
    assert_eq!(code, 0, "{prepared}");
    assert_eq!(prepared["data"]["applied"], false);
    let saved: Value = serde_json::from_slice(&fs::read(dir.path().join("existing.links"))?)?;
    assert_eq!(saved["links"], json!([original]));
    let patch = dir.path().join("change.patch.json");
    let (code, applied) = run(dir.path(), &["apply", patch.to_str().unwrap()], None)?;
    assert_eq!(code, 0, "{applied}");
    let after_apply = fs::read(dir.path().join("existing.links"))?;
    let (code, stale) = run(dir.path(), &["apply", patch.to_str().unwrap()], None)?;
    assert_eq!(code, 2, "{stale}");
    assert!(
        stale["error"]
            .as_str()
            .unwrap()
            .contains("resource changed")
    );
    assert_eq!(fs::read(dir.path().join("existing.links"))?, after_apply);
    let mut tampered: Value = serde_json::from_slice(&fs::read(&patch)?)?;
    tampered["replacement"]["text"] = json!("tampered");
    let (code, rejected) = run(
        dir.path(),
        &["request"],
        Some(&json!({"op":"apply","patch":tampered})),
    )?;
    assert_eq!(code, 2, "{rejected}");
    assert!(rejected["error"].as_str().unwrap().contains("integrity"));
    assert_eq!(fs::read(dir.path().join("existing.links"))?, after_apply);
    Ok(())
}

#[test]
fn copying_between_external_formats_preserves_target_and_annotation() -> Result<()> {
    let dir = setup()?;
    let path = dir.path().join("monika.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path)?)?;
    let mut lines = config["extensions"]["sets"].clone();
    lines["command"]
        .as_array_mut()
        .unwrap()
        .push(json!("lines"));
    lines["schemes"] = json!([]);
    config["extensions"]["lines"] = lines;
    config["comparators"] = json!({"test.sets.v1":"sets"});
    fs::write(&path, serde_json::to_vec(&config)?)?;
    let original = declaration("copied", 2, json!({"note":["a",{"nested":null}]}));
    write_links(dir.path(), "source.links", vec![original.clone()])?;
    let (code, result) = run(
        dir.path(),
        &[
            "link",
            "copy",
            "--from",
            "source.links",
            "--from-extension",
            "sets",
            "--id",
            "copied",
            "--in",
            "destination.jsonl",
            "--extension",
            "lines",
        ],
        None,
    )?;
    assert_eq!(code, 0, "{result}");
    let copied: Value = serde_json::from_slice(&fs::read(dir.path().join("destination.jsonl"))?)?;
    assert_eq!(copied, original);
    let source: Value = serde_json::from_slice(&fs::read(dir.path().join("source.links"))?)?;
    assert_eq!(source["links"][0], original);
    Ok(())
}

#[test]
fn new_resource_patch_rejects_intervening_creation_and_existing_path_replacement() -> Result<()> {
    let dir = setup()?;
    let declaration = declaration("one", 2, Value::Null);
    let (code, prepared) = run(
        dir.path(),
        &[
            "link",
            "add",
            "--in",
            "new.links",
            "--extension",
            "sets",
            "--declaration",
            "-",
            "--patch-out",
            "new.patch",
        ],
        Some(&declaration),
    )?;
    assert_eq!(code, 0, "{prepared}");
    fs::write(dir.path().join("new.links"), "user content")?;
    let (code, rejected) = run(
        dir.path(),
        &["apply", dir.path().join("new.patch").to_str().unwrap()],
        None,
    )?;
    assert_eq!(code, 2, "{rejected}");
    assert_eq!(
        fs::read_to_string(dir.path().join("new.links"))?,
        "user content"
    );

    write_links(dir.path(), "original.links", vec![declaration.clone()])?;
    let (code, prepared) = run(
        dir.path(),
        &[
            "link",
            "remove",
            "--in",
            "original.links",
            "--extension",
            "sets",
            "--id",
            "one",
            "--patch-out",
            "remove.patch",
        ],
        None,
    )?;
    assert_eq!(code, 0, "{prepared}");
    let original = fs::read(dir.path().join("original.links"))?;
    fs::write(dir.path().join("replacement"), &original)?;
    fs::rename(
        dir.path().join("replacement"),
        dir.path().join("original.links"),
    )?;
    let (code, rejected) = run(
        dir.path(),
        &["apply", dir.path().join("remove.patch").to_str().unwrap()],
        None,
    )?;
    assert_eq!(code, 2, "{rejected}");
    assert_eq!(fs::read(dir.path().join("original.links"))?, original);
    Ok(())
}

#[test]
fn cli_usage_errors_are_json_and_initialization_has_no_implicit_formats() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (code, initialized) = run(dir.path(), &["init"], None)?;
    assert_eq!(code, 0, "{initialized}");
    let (code, duplicate) = run(dir.path(), &["init"], None)?;
    assert_eq!(code, 2, "{duplicate}");
    let (code, listed) = run(dir.path(), &["extension", "list"], None)?;
    assert_eq!(code, 0, "{listed}");
    assert_eq!(listed["data"], json!([]));
    let (code, invalid) = run(dir.path(), &["unknown-command"], None)?;
    assert_eq!(code, 2, "{invalid}");
    assert_eq!(invalid["ok"], false);
    let (code, invalid) = run(dir.path(), &["links", "--within"], None)?;
    assert_eq!(code, 2, "{invalid}");
    Ok(())
}

#[test]
fn documented_walkthrough_executes_every_command_with_real_files() -> Result<()> {
    let output = Command::new("python3")
        .arg(format!("{}/examples/demo.py", env!("CARGO_MANIFEST_DIR")))
        .arg("--monika")
        .arg(env!("CARGO_BIN_EXE_monika"))
        .output()?;
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
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
fn core_rejects_annotation_loss_and_does_not_accept_an_unperformed_write() -> Result<()> {
    let dir = setup()?;
    let config_path = dir.path().join("monika.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path)?)?;
    let mut tamper = config["extensions"]["sets"].clone();
    tamper["command"]
        .as_array_mut()
        .unwrap()
        .push(json!("tamper"));
    tamper["schemes"] = json!([]);
    config["extensions"]["tamper"] = tamper;
    fs::write(&config_path, serde_json::to_vec(&config)?)?;
    let declaration = declaration("preserve", 2, json!({"keep":[1,null,{"日本語":true}]}));
    let (code, rejected) = run(
        dir.path(),
        &[
            "link",
            "add",
            "--in",
            "new.links",
            "--extension",
            "tamper",
            "--declaration",
            "-",
        ],
        Some(&declaration),
    )?;
    assert_eq!(code, 2, "{rejected}");
    assert!(!dir.path().join("new.links").exists());
    assert!(
        rejected["error"]
            .as_str()
            .unwrap()
            .contains("editor changed")
    );

    let mut no_write = config["extensions"]["sets"].clone();
    no_write["command"]
        .as_array_mut()
        .unwrap()
        .push(json!("no_write"));
    config["extensions"]["sets"]["schemes"] = json!([]);
    config["extensions"]["no_write"] = no_write;
    fs::write(&config_path, serde_json::to_vec(&config)?)?;
    write_links(dir.path(), "existing.links", vec![])?;
    let original = fs::read(dir.path().join("existing.links"))?;
    let (code, rejected) = run(
        dir.path(),
        &[
            "link",
            "add",
            "--in",
            "existing.links",
            "--extension",
            "sets",
            "--declaration",
            "-",
        ],
        Some(&declaration),
    )?;
    assert_eq!(code, 2, "{rejected}");
    assert!(
        rejected["error"]
            .as_str()
            .unwrap()
            .contains("does not match")
    );
    assert_eq!(fs::read(dir.path().join("existing.links"))?, original);
    Ok(())
}

#[cfg(unix)]
#[test]
fn interrupt_stops_a_waiting_cli_and_its_extension() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = json!({"version":1,"sources":[],"extensions":{"slow":{
        "command":["python3","-c","import os,pathlib,time; pathlib.Path('worker.pid').write_text(str(os.getpid())); time.sleep(60)"],
        "revision":"1","timeout_ms":30000
    }}});
    fs::write(dir.path().join("monika.json"), serde_json::to_vec(&config)?)?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_monika"))
        .arg("--root")
        .arg(dir.path())
        .args(["--json", "extension", "show", "slow"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !dir.path().join("worker.pid").exists() {
        if std::time::Instant::now() > deadline {
            child.kill()?;
            panic!("worker did not start");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let worker: libc::pid_t = fs::read_to_string(dir.path().join("worker.pid"))?.parse()?;
    // SAFETY: signal only the live CLI process created by this test.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    while child.try_wait()?.is_none() {
        if std::time::Instant::now() > deadline {
            child.kill()?;
            // SAFETY: this is the dedicated extension process group recorded above.
            unsafe {
                libc::kill(-worker, libc::SIGKILL);
            }
            panic!("CLI did not respond to interruption");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output()?;
    assert_eq!(output.status.code(), Some(130));
    let response: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(response["ok"], false);
    // SAFETY: signal zero only probes the terminated child's process ID.
    assert_eq!(unsafe { libc::kill(worker, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    Ok(())
}
