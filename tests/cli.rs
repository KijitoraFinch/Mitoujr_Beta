use anyhow::Result;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

#[derive(Debug)]
struct ResultData {
    code: i32,
    values: Vec<Value>,
    error: String,
}
fn decode(output: Output) -> Result<ResultData> {
    Ok(ResultData {
        code: output.status.code().unwrap(),
        values: serde_json::Deserializer::from_slice(&output.stdout)
            .into_iter()
            .collect::<std::result::Result<_, _>>()?,
        error: String::from_utf8(output.stderr)?,
    })
}
fn raw(root: &Path, args: &[&str], input: &[u8]) -> Result<ResultData> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_monika"))
        .current_dir(root)
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let bytes = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&bytes));
    let output = child.wait_with_output()?;
    let _ = writer.join().unwrap(); // Rejected inputs may close the pipe early.
    decode(output)
}
fn run(root: &Path, args: &[&str], input: &[Value]) -> Result<ResultData> {
    let mut bytes = Vec::new();
    for value in input {
        serde_json::to_writer(&mut bytes, value)?;
        bytes.push(b'\n');
    }
    raw(root, args, &bytes)
}
fn success(result: ResultData) -> Vec<Value> {
    assert_eq!(result.code, 0, "{result:?}");
    assert!(result.error.is_empty(), "{result:?}");
    result.values
}
fn shell(root: &Path, script: &str) -> Result<ResultData> {
    decode(
        Command::new("bash")
            .args(["-o", "pipefail", "-c", script])
            .env("M", env!("CARGO_BIN_EXE_monika"))
            .current_dir(root)
            .output()?,
    )
}
fn selector(file: &str, minimum: u32) -> Value {
    json!({"extension":"sets", "resource":file, "query":minimum.to_string()})
}
fn link(id: &str, minimum: u32, annotation: Value) -> Value {
    json!({"id":id, "source":selector("rows.json",0), "target":selector("rows.json",minimum), "annotation":annotation})
}
fn save(root: &Path, links: &[Value]) -> Result<()> {
    fs::write(
        root.join("monika.links.json"),
        serde_json::to_vec_pretty(&json!({"version":1,"links":links}))?,
    )?;
    Ok(())
}
fn stored(root: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(
        root.join("monika.links.json"),
    )?)?)
}
fn setup() -> Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    success(run(dir.path(), &["init"], &[])?);
    let config = json!({"version":1,"extensions":{"sets":{
        "command":["python3",format!("{}/tests/fixtures/sets_extension.py",env!("CARGO_MANIFEST_DIR"))],
        "revision":"test-v1","schemes":["file"]}},"names":{"all":selector("rows.json",0)},"checks":[{"extension":"sets"}]});
    fs::write(dir.path().join("monika.json"), serde_json::to_vec(&config)?)?;
    fs::write(dir.path().join("rows.json"), "[1,2,3]")?;
    Ok(dir)
}
fn variant(root: &Path, name: &str) -> Result<()> {
    let path = root.join("monika.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path)?)?;
    config["extensions"] = json!({name:{"command":["python3",format!("{}/tests/fixtures/sets_extension.py",env!("CARGO_MANIFEST_DIR")),name],"revision":"test-v1","schemes":["file"]}});
    config["checks"] = json!([]);
    fs::write(path, serde_json::to_vec(&config)?)?;
    Ok(())
}
#[test]
fn ordinary_sidecar_edits_support_names_and_paths_without_starting_extensions() -> Result<()> {
    let dir = setup()?;
    let mut item = link("one", 2, json!({"relation":"implements"}));
    item["source"] = json!({"name":"all"});
    item["target"]["resource"] = json!("./rows.json");
    save(dir.path(), &[item.clone()])?;
    let before = fs::read(dir.path().join("monika.links.json"))?;
    fs::remove_file(dir.path().join("rows.json"))?;
    let values = success(run(dir.path(), &["links"], &[])?);
    assert_eq!(values.len(), 1);
    assert!(values[0]["source"]["resource"].as_str().unwrap() == "rows.json");
    assert_eq!(fs::read(dir.path().join("monika.links.json"))?, before);
    assert_eq!(run(dir.path(), &["check"], &values)?.code, 2);
    save(dir.path(), &[])?;
    assert!(success(run(dir.path(), &["links"], &[])?).is_empty());
    fs::remove_file(dir.path().join("monika.links.json"))?;
    assert_eq!(run(dir.path(), &["links"], &[])?.code, 2);
    Ok(())
}

#[test]
fn jq_pipeline_selects_compares_reads_and_checks_with_original_input() -> Result<()> {
    let dir = setup()?;
    let mut ignored = link("ignored", 0, json!({"relation":"other"}));
    ignored["target"] = selector("missing.json", 0);
    save(
        dir.path(),
        &[
            link(
                "chosen",
                2,
                json!({"relation":"implements","expected_count":2}),
            ),
            ignored,
        ],
    )?;
    let output = success(shell(
        dir.path(),
        r#""$M" links | jq -c 'select(.annotation.relation == "implements") | {link:., boundary:{name:"all"}}' | "$M" compare --left /link/target --right /boundary | jq -c 'select(.relation == "equal" or .relation == "within") | .input.link' | "$M" read --at /target | "$M" check --at /input"#,
    )?);
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["valid"], true);
    assert_eq!(output[0]["input"]["input"]["id"], "chosen");
    assert_eq!(output[0]["input"]["region"]["content"]["count"], 2);
    Ok(())
}

#[test]
fn large_sidecar_is_filtered_before_any_resource_is_observed() -> Result<()> {
    let dir = setup()?;
    let mut config: Value = serde_json::from_slice(&fs::read(dir.path().join("monika.json"))?)?;
    config["extensions"]["sets"]["command"][1] = json!(format!(
        "{}/tests/fixtures/reading_extension.py",
        env!("CARGO_MANIFEST_DIR")
    ));
    fs::write(dir.path().join("monika.json"), serde_json::to_vec(&config)?)?;
    let mut links = vec![link("chosen", 2, json!(null))];
    for i in 1..600 {
        let mut item = link(&i.to_string(), 2, json!(null));
        item["target"] = selector(&format!("absent-{i}.json"), 0);
        links.push(item);
    }
    save(dir.path(), &links)?;
    success(shell(
        dir.path(),
        r#""$M" links | jq -c 'select(.id == "chosen")' | "$M" read --at /target"#,
    )?);
    let log = fs::read_to_string(dir.path().join("wire.jsonl"))?;
    let records: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        records
            .iter()
            .filter(|record| record["message"]["operation"] == "observe")
            .count(),
        1,
        "{log}"
    );
    Ok(())
}

#[test]
fn observations_in_results_are_scoped_to_each_record() -> Result<()> {
    let dir = setup()?;
    fs::write(dir.path().join("other.json"), "[4,5]")?;
    let values = success(run(
        dir.path(),
        &["read"],
        &[
            selector("rows.json", 0),
            selector("other.json", 0),
            selector("rows.json", 1),
        ],
    )?);
    assert_eq!(values.len(), 3);
    for result in &values {
        assert_eq!(result["observations"].as_object().unwrap().len(), 1);
    }
    assert_eq!(
        values[0]["region"]["observations"],
        values[2]["region"]["observations"]
    );
    assert_ne!(
        values[0]["region"]["observations"],
        values[1]["region"]["observations"]
    );
    assert!(!dir.path().join(".monika").exists());
    Ok(())
}

#[test]
fn compare_reports_all_relations_and_fails_on_uninterpretable_targets() -> Result<()> {
    let dir = setup()?;
    fs::write(dir.path().join("other.json"), "[1]")?;
    let cases = [(0, 0, "equal"), (0, 2, "contains"), (2, 0, "within")];
    for (a, b, expected) in cases {
        let result = success(run(
            dir.path(),
            &["compare"],
            &[json!({"left":selector("rows.json",a),"right":selector("rows.json",b)})],
        )?);
        assert_eq!(result[0]["relation"], expected);
    }
    let result = success(run(
        dir.path(),
        &["compare"],
        &[json!({"left":selector("rows.json",0),"right":selector("other.json",0)})],
    )?);
    assert_eq!(result[0]["relation"], "incomparable");
    let result = run(
        dir.path(),
        &["compare"],
        &[json!({"left":selector("rows.json",0),"right":selector("missing.json",0)})],
    )?;
    assert_eq!(result.code, 2);
    assert!(result.values.is_empty());
    assert!(result.error.contains("does not exist"));
    Ok(())
}

#[test]
fn checks_distinguish_failed_conditions_from_execution_errors_and_allow_new_annotations()
-> Result<()> {
    let dir = setup()?;
    let pass = link(
        "new",
        2,
        json!({"custom_new_field":"freely chosen","expected_count":2}),
    );
    let fail = link("fail", 2, json!({"expected_count":99}));
    let result = run(dir.path(), &["check"], &[pass, fail])?;
    assert_eq!(result.code, 1);
    assert!(result.error.is_empty());
    assert_eq!(result.values.len(), 2);
    assert_eq!(result.values[0]["valid"], true);
    assert_eq!(result.values[1]["valid"], false);
    let mut bad = link("bad", 0, json!(null));
    bad["target"] = json!({"name":"unknown"});
    assert_eq!(run(dir.path(), &["check"], &[bad])?.code, 2);
    Ok(())
}

#[test]
fn duplicates_only_compare_the_supplied_links_and_preserve_identity() -> Result<()> {
    let dir = setup()?;
    let first = link("one", 1, json!(null));
    let second = link("two", 0, json!(null)); // Both select all three rows, different spelling.
    let third = link("other_annotation", 0, json!({"meaning":"different"}));
    save(dir.path(), &[first.clone(), second.clone(), third.clone()])?;
    let result = success(run(
        dir.path(),
        &["check", "--duplicates"],
        std::slice::from_ref(&first),
    )?);
    assert_eq!(result[0]["duplicates"], json!([]));
    let result = run(
        dir.path(),
        &["check", "--duplicates"],
        &[first.clone(), second, third],
    )?;
    assert_eq!(result.code, 1);
    assert_eq!(result.values[0]["duplicates"], json!(["two"]));
    assert_eq!(result.values[1]["duplicates"], json!(["one"]));
    assert_eq!(result.values[2]["valid"], true);
    assert_eq!(
        run(
            dir.path(),
            &["check", "--duplicates"],
            &[first.clone(), first]
        )?
        .code,
        2
    );
    Ok(())
}

#[test]
fn streams_accept_pretty_json_and_empty_input_and_reject_invalid_pointers_or_records() -> Result<()>
{
    let dir = setup()?;
    let pretty = format!(
        "{}\n{}\n",
        serde_json::to_string_pretty(&selector("rows.json", 0))?,
        serde_json::to_string_pretty(&selector("rows.json", 2))?
    );
    assert_eq!(
        success(raw(dir.path(), &["read"], pretty.as_bytes())?).len(),
        2
    );
    for command in ["read", "compare", "check"] {
        assert!(success(run(dir.path(), &[command], &[])?).is_empty());
    }
    for pointer in ["missing", "/missing", "/~2"] {
        assert_eq!(
            run(dir.path(), &["read", "--at", pointer], &[json!({})])?.code,
            2
        );
    }
    let value = json!({"a/b":{"~target":selector("rows.json",0)},"keep":9007199254740993_u64});
    let output = success(run(
        dir.path(),
        &["read", "--at", "/a~1b/~0target"],
        std::slice::from_ref(&value),
    )?);
    assert_eq!(output[0]["input"], value);
    let truncated = format!(
        "{}\n{{broken",
        serde_json::to_string(&selector("rows.json", 0))?
    );
    let result = raw(dir.path(), &["read"], truncated.as_bytes())?;
    assert_eq!(result.code, 2);
    assert_eq!(result.values.len(), 1);
    assert!(result.error.contains("record 2"));
    Ok(())
}

#[test]
fn extension_tests_distinguish_law_violations_from_failed_resolution() -> Result<()> {
    for (name, extension, code) in [
        ("sets", "sets", 0),
        ("broken", "broken", 1),
        ("sets", "absent", 2),
    ] {
        let dir = setup()?;
        variant(dir.path(), name)?;
        let mut s = selector("rows.json", 0);
        s["extension"] = json!(extension);
        let result = run(
            dir.path(),
            &["extension", "test", "--cases", "-"],
            &[json!([s])],
        )?;
        assert_eq!(result.code, code, "{result:?}");
        assert_eq!(result.values[0]["complete"], code != 2);
    }
    Ok(())
}

#[test]
fn removed_interfaces_are_rejected_and_init_is_not_destructive() -> Result<()> {
    let dir = setup()?;
    let before = fs::read(dir.path().join("monika.json"))?;
    assert_eq!(run(dir.path(), &["init"], &[])?.code, 2);
    assert_eq!(fs::read(dir.path().join("monika.json"))?, before);
    for args in [
        &["request"][..],
        &["import"],
        &["generate"],
        &["apply"],
        &["filter"],
        &["links", "--where", "true"],
        &["--json", "links"],
        &["show", "@all"],
        &["link", "add"],
    ] {
        assert_eq!(run(dir.path(), args, &[])?.code, 2);
    }
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

fn configure_resources(root: &Path, resources: Value, trace: bool) -> Result<()> {
    let path = root.join("monika.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path)?)?;
    config["resources"] = resources;
    if trace {
        config["extensions"]["sets"]["command"][1] = json!(format!(
            "{}/tests/fixtures/reading_extension.py",
            env!("CARGO_MANIFEST_DIR")
        ));
    }
    fs::write(path, serde_json::to_vec(&config)?)?;
    Ok(())
}

fn wire(root: &Path) -> Result<Vec<Value>> {
    let path = root.join("wire.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    fs::read_to_string(path)?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn observations_count(root: &Path) -> Result<usize> {
    Ok(wire(root)?
        .iter()
        .filter(|value| value["message"]["operation"] == "observe")
        .count())
}

#[test]
fn capture_is_immutable_and_checks_content_not_identity_or_observation_revision() -> Result<()> {
    let dir = setup()?;
    configure_resources(dir.path(), json!({}), true)?;
    save(dir.path(), &[link("reviewed", 2, json!(null))])?;
    let capture = success(run(
        dir.path(),
        &["capture", "--link", "reviewed", "--endpoint", "target"],
        &[],
    )?);
    let id = capture[0]["captures"]["target"].as_str().unwrap();
    let baseline = success(run(dir.path(), &["read", "--captured"], &[json!(id)])?);
    assert_eq!(baseline[0]["selector"]["resource"], "rows.json");
    assert_eq!(
        baseline[0]["observations"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()["resource"],
        "rows.json"
    );
    let captured_bytes = fs::read(dir.path().join(".monika/captures").join(id))?;
    fs::write(dir.path().join("rows.json"), "[9,8,0]")?; // Same selected count, different identity and revision.
    let item = stored(dir.path())?["links"][0].clone();
    let checked = success(run(dir.path(), &["check"], std::slice::from_ref(&item))?);
    assert_eq!(checked[0]["captures"]["target"]["changed"], false);
    assert_ne!(
        checked[0]["target"]["region"]["identity"],
        baseline[0]["region"]["identity"]
    );
    fs::write(dir.path().join("rows.json"), "[9,8,7]")?;
    let checked = run(dir.path(), &["check"], std::slice::from_ref(&item))?;
    assert_eq!(checked.code, 1, "{checked:?}");
    assert_eq!(checked.values[0]["captures"]["target"]["changed"], true);
    let before = wire(dir.path())?.len();
    fs::remove_file(dir.path().join("rows.json"))?;
    success(run(dir.path(), &["links"], &[])?);
    let saved = success(run(
        dir.path(),
        &["read", "--captured", "--at", "/target"],
        &[item],
    )?);
    assert_eq!(saved[0]["region"], baseline[0]["region"]);
    assert_eq!(wire(dir.path())?.len(), before); // No describe, resolve, compare or observe.
    assert_eq!(
        fs::read(dir.path().join(".monika/captures").join(id))?,
        captured_bytes
    );
    let input = json!({"name":"no-longer-defined","capture":id});
    assert_eq!(
        success(run(dir.path(), &["read", "--captured"], &[input])?)[0]["region"],
        baseline[0]["region"]
    );
    Ok(())
}

#[test]
fn capture_updates_only_selected_json_spans_and_records_each_confirmation() -> Result<()> {
    let dir = setup()?;
    configure_resources(dir.path(), json!({}), true)?;
    let original = "{\n \"links\" : [ {\"id\":\"one\", \"target\": { \"name\" : \"all\" }, \"source\":{\"extension\":\"sets\",\"resource\":\"./rows.json\",\"query\":\"2\"}, \"annotation\":{\"number\":1e2,\"text\":\"\\u65e5\"}} ], \"version\":1\n}\n";
    fs::write(dir.path().join("monika.links.json"), original)?;
    let first = success(run(
        dir.path(),
        &["capture", "--link", "one", "--endpoint", "target"],
        &[],
    )?);
    let id = first[0]["captures"]["target"].as_str().unwrap();
    let expected = original.replace("\"all\" }", &format!("\"all\" ,\"capture\":\"{id}\"}}"));
    assert_eq!(
        fs::read_to_string(dir.path().join("monika.links.json"))?,
        expected
    );
    let second = success(run(
        dir.path(),
        &["capture", "--link", "one", "--endpoint", "target"],
        &[],
    )?);
    let second_id = second[0]["captures"]["target"].as_str().unwrap();
    assert_ne!(id, second_id);
    assert_eq!(
        fs::read_to_string(dir.path().join("monika.links.json"))?,
        expected.replace(id, second_id)
    );
    let before = observations_count(dir.path())?;
    let both = success(run(
        dir.path(),
        &["capture", "--link", "one", "--endpoint", "both"],
        &[],
    )?);
    assert_eq!(both[0]["captures"].as_object().unwrap().len(), 2);
    assert_eq!(observations_count(dir.path())?, before + 1);
    assert!(!dir.path().join(".monika-sidecar.lock").exists());
    let captured = stored(dir.path())?;
    assert_eq!(captured["links"][0]["target"]["name"], "all");
    assert_eq!(captured["links"][0]["source"]["resource"], "./rows.json");
    assert_eq!(run(dir.path(), &["capture", "--link", "one"], &[])?.code, 2);
    assert_eq!(
        run(
            dir.path(),
            &["capture", "--link", "missing", "--endpoint", "target"],
            &[]
        )?
        .code,
        2
    );
    Ok(())
}

#[test]
fn capture_mismatches_missing_objects_and_corruption_are_errors_before_resolution() -> Result<()> {
    let dir = setup()?;
    configure_resources(dir.path(), json!({}), true)?;
    let mut item = link("one", 0, json!(null));
    item["target"] = json!({"name":"all"});
    save(dir.path(), &[item])?;
    let captured = success(run(
        dir.path(),
        &["capture", "--link", "one", "--endpoint", "target"],
        &[],
    )?);
    let id = captured[0]["captures"]["target"].as_str().unwrap();
    let mut item = stored(dir.path())?["links"][0].clone();
    let path = dir.path().join("monika.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path)?)?;
    config["names"]["all"] = selector("rows.json", 2);
    fs::write(&path, serde_json::to_vec(&config)?)?;
    let before = wire(dir.path())?.len();
    let mismatch = run(dir.path(), &["check"], std::slice::from_ref(&item))?;
    assert_eq!(mismatch.code, 2);
    assert!(
        mismatch.error.contains("selector does not match"),
        "{mismatch:?}"
    );
    assert_eq!(wire(dir.path())?.len(), before);
    let read = success(run(
        dir.path(),
        &["read", "--captured", "--at", "/target"],
        std::slice::from_ref(&item),
    )?);
    assert_eq!(read[0]["selector"]["query"], "0");
    item["target"]["capture"] = json!("0".repeat(64));
    assert_eq!(run(dir.path(), &["check"], &[item])?.code, 2);
    let path = dir.path().join(".monika/captures").join(id);
    let mut bytes = fs::read(&path)?;
    bytes.push(b' ');
    fs::write(path, bytes)?;
    assert_eq!(
        run(dir.path(), &["read", "--captured"], &[json!(id)])?.code,
        2
    );
    assert_eq!(
        run(
            dir.path(),
            &["read", "--captured"],
            &[json!("../monika.json")]
        )?
        .code,
        2
    );
    Ok(())
}

#[test]
fn failed_capture_of_both_endpoints_leaves_sidecar_unchanged() -> Result<()> {
    let dir = setup()?;
    let mut item = link("one", 0, json!(null));
    item["target"] = selector("missing/directory/rows.json", 0);
    save(dir.path(), &[item])?;
    let before = fs::read(dir.path().join("monika.links.json"))?;
    success(run(dir.path(), &["links"], &[])?);
    assert_eq!(
        run(
            dir.path(),
            &["capture", "--link", "one", "--endpoint", "both"],
            &[]
        )?
        .code,
        2
    );
    assert_eq!(fs::read(dir.path().join("monika.links.json"))?, before);
    Ok(())
}

#[test]
fn refreshed_resources_are_fixed_until_refresh_and_all_expands_sidecar_names() -> Result<()> {
    let dir = setup()?;
    configure_resources(
        dir.path(),
        json!({"./rows.json":"refreshed","unused.json":"refreshed"}),
        true,
    )?;
    let mut item = link("one", 2, json!(null));
    item["source"] = json!({"name":"all"});
    save(dir.path(), &[item])?;
    let target = json!({"name":"all"});
    let missing = run(dir.path(), &["read"], std::slice::from_ref(&target))?;
    assert_eq!(missing.code, 2);
    assert!(missing.error.contains("no saved Observation"));
    assert_eq!(observations_count(dir.path())?, 0);
    let first = success(run(
        dir.path(),
        &["refresh", "rows.json", "./rows.json"],
        &[],
    )?);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["resource"], "rows.json");
    assert_eq!(observations_count(dir.path())?, 1);
    fs::write(dir.path().join("rows.json"), "[1,2,3,4]")?;
    let read = success(run(
        dir.path(),
        &["read"],
        &[target.clone(), target.clone()],
    )?);
    for value in &read {
        assert_eq!(value["region"]["content"]["count"], 3);
        assert_eq!(value["region"]["observations"], json!([first[0]["id"]]));
    }
    success(run(
        dir.path(),
        &["capture", "--link", "one", "--endpoint", "both"],
        &[],
    )?);
    assert_eq!(observations_count(dir.path())?, 1);
    let second = success(run(dir.path(), &["refresh", "--all"], &[])?);
    assert_eq!(second.len(), 1);
    assert_ne!(first[0]["id"], second[0]["id"]);
    assert_eq!(observations_count(dir.path())?, 2);
    let item = stored(dir.path())?["links"][0].clone();
    assert_eq!(run(dir.path(), &["check"], &[item])?.code, 1);
    fs::remove_file(dir.path().join("rows.json"))?;
    assert_eq!(
        success(run(dir.path(), &["read"], &[target])?)[0]["region"]["content"]["count"],
        4
    );
    assert_eq!(observations_count(dir.path())?, 2);
    assert_eq!(
        run(dir.path(), &["refresh", "unconfigured.json"], &[])?.code,
        2
    );
    Ok(())
}

#[test]
fn nested_reads_apply_each_resource_policy_and_share_observations_for_aliases() -> Result<()> {
    let dir = setup()?;
    configure_resources(dir.path(), json!({"rows.json":"refreshed"}), true)?;
    let adopted = success(run(dir.path(), &["refresh", "rows.json"], &[])?);
    fs::write(dir.path().join("index.json"), r#"{"next":"./rows.json"}"#)?;
    fs::write(dir.path().join("rows.json"), "[99]")?;
    let target = json!({"extension":"sets","query":{"mode":"follow","resource":"index.json"}});
    let read = success(run(dir.path(), &["read"], &[target.clone(), target])?);
    assert_eq!(read[0]["region"]["content"]["count"], 3);
    assert_eq!(read[0]["observations"].as_object().unwrap().len(), 2);
    assert_eq!(read[0]["observations"], read[1]["observations"]);
    assert_eq!(
        read[0]["observations"][adopted[0]["id"].as_str().unwrap()],
        adopted[0]
    );
    assert_eq!(observations_count(dir.path())?, 2); // refresh rows, observe index only
    let wire = wire(dir.path())?;
    for record in &wire {
        let message = &record["message"];
        if message["operation"] == "observe" {
            assert!(
                message["input"]["resource"]
                    .as_str()
                    .unwrap()
                    .starts_with("file:")
            );
            assert!(message["input"]["content_path"].is_string());
            assert!(!Path::new(message["input"]["content_path"].as_str().unwrap()).exists());
        }
        if let Some(result) = message.get("result") {
            assert!(result.get("bytes").is_none());
        }
    }
    // Normalized aliases supplied in one request share the same observed input.
    let batch = json!({"extension":"sets","query":{"resources":["index.json","./index.json"]}});
    // Use numeric rows so the ordinary sets resolver can process the aliases.
    fs::write(dir.path().join("index.json"), "[1]")?;
    let result = success(run(dir.path(), &["read"], &[batch])?);
    assert_eq!(result[0]["observations"].as_object().unwrap().len(), 1);
    assert_eq!(observations_count(dir.path())?, 3);
    Ok(())
}

#[test]
fn refresh_batch_failure_does_not_adopt_partial_results() -> Result<()> {
    let dir = setup()?;
    configure_resources(
        dir.path(),
        json!({"rows.json":"refreshed","z.json":"refreshed"}),
        false,
    )?;
    success(run(dir.path(), &["refresh", "rows.json"], &[])?);
    let before = fs::read(dir.path().join(".monika/current.json"))?;
    fs::write(dir.path().join("rows.json"), "[9]")?;
    fs::create_dir(dir.path().join("z.json"))?; // Real acquisition failure, not an absent Resource.
    let failed = run(dir.path(), &["refresh", "rows.json", "z.json"], &[])?;
    assert_eq!(failed.code, 2);
    assert!(failed.values.is_empty());
    assert_eq!(fs::read(dir.path().join(".monika/current.json"))?, before);
    let result = success(run(dir.path(), &["read"], &[json!({"name":"all"})])?);
    assert_eq!(result[0]["region"]["content"]["count"], 3);
    Ok(())
}

#[test]
fn gc_retains_references_and_explicit_standalone_captures_and_checks_observation_integrity()
-> Result<()> {
    let dir = setup()?;
    configure_resources(dir.path(), json!({"rows.json":"refreshed"}), false)?;
    save(dir.path(), &[link("one", 2, json!(null))])?;
    let old = success(run(dir.path(), &["refresh", "rows.json"], &[])?)[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let unreferenced =
        success(run(dir.path(), &["capture"], &[json!({"name":"all"})])?)[0]["capture"]
            .as_str()
            .unwrap()
            .to_owned();
    let keep = success(run(dir.path(), &["capture"], &[json!({"name":"all"})])?)[0]["capture"]
        .as_str()
        .unwrap()
        .to_owned();
    let linked = success(run(
        dir.path(),
        &["capture", "--link", "one", "--endpoint", "target"],
        &[],
    )?)[0]["captures"]["target"]
        .as_str()
        .unwrap()
        .to_owned();
    let current = success(run(dir.path(), &["refresh", "rows.json"], &[])?)[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let preview = success(run(dir.path(), &["gc", "--dry-run", "--keep", &keep], &[])?);
    assert_eq!(preview[0]["captures"], json!([unreferenced]));
    assert_eq!(preview[0]["observations"], json!([old]));
    assert!(
        dir.path()
            .join(".monika/captures")
            .join(&unreferenced)
            .exists()
    );
    let result = success(run(dir.path(), &["gc", "--keep", &keep], &[])?);
    assert_eq!(result[0]["captures"], preview[0]["captures"]);
    assert!(
        !dir.path()
            .join(".monika/captures")
            .join(&unreferenced)
            .exists()
    );
    success(run(
        dir.path(),
        &["read", "--captured"],
        &[json!(linked), json!(keep)],
    )?);
    let path = dir.path().join(".monika/observations").join(current);
    let mut bytes = fs::read(&path)?;
    bytes.push(0);
    fs::write(path, bytes)?;
    let result = run(dir.path(), &["read"], &[json!({"name":"all"})])?;
    assert_eq!(result.code, 2);
    assert!(result.error.contains("corrupt observation"), "{result:?}");
    assert_eq!(run(dir.path(), &["gc"], &[])?.code, 2);
    Ok(())
}

#[test]
fn named_extension_cases_and_portable_paths_are_normalized_without_filesystem_access() -> Result<()>
{
    let dir = setup()?;
    let report = success(run(
        dir.path(),
        &["extension", "test", "--cases", "-"],
        &[json!([{"name":"all"},selector("./rows.json",2)])],
    )?);
    assert_eq!(report[0]["passed"], true);
    let mut item = link("absent", 0, json!(null));
    item["target"]["resource"] = json!("missing/parent/../rows.json");
    save(dir.path(), &[item.clone()])?;
    let links = success(run(dir.path(), &["links"], &[])?);
    assert_eq!(links[0]["target"]["resource"], "missing/rows.json");
    item["target"]["resource"] = json!("../outside.json");
    save(dir.path(), &[item])?;
    assert_eq!(run(dir.path(), &["links"], &[])?.code, 2);
    Ok(())
}

#[test]
fn saved_objects_work_after_copying_the_project_to_another_root() -> Result<()> {
    fn copy_tree(from: &Path, to: &Path) -> Result<()> {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_tree(&entry.path(), &target)?;
            } else {
                fs::copy(entry.path(), target)?;
            }
        }
        Ok(())
    }
    let dir = setup()?;
    configure_resources(dir.path(), json!({"rows.json":"refreshed"}), false)?;
    save(dir.path(), &[link("one", 2, json!(null))])?;
    success(run(dir.path(), &["refresh", "rows.json"], &[])?);
    success(run(
        dir.path(),
        &["capture", "--link", "one", "--endpoint", "both"],
        &[],
    )?);
    let copied = tempfile::tempdir()?;
    copy_tree(dir.path(), copied.path())?;
    fs::remove_file(copied.path().join("rows.json"))?;
    dir.close()?;
    let links = success(run(copied.path(), &["links"], &[])?);
    success(run(copied.path(), &["check"], &links)?);
    assert_eq!(
        success(run(
            copied.path(),
            &["read", "--at", "/target", "--captured"],
            &links
        )?)[0]["region"]["content"]["count"],
        2
    );
    assert_eq!(
        success(run(copied.path(), &["read", "--at", "/target"], &links)?)[0]["region"]["content"]
            ["count"],
        2
    );
    Ok(())
}

#[test]
fn sidecar_rejects_duplicate_ids_and_definitions_even_with_different_captures() -> Result<()> {
    let dir = setup()?;
    let first = link("one", 0, json!(null));
    let mut second = first.clone();
    second["target"]["query"] = json!("2");
    save(dir.path(), &[first.clone(), second])?;
    assert!(
        run(dir.path(), &["links"], &[])?
            .error
            .contains("duplicate Link ID")
    );
    let mut second = first.clone();
    second["id"] = json!("two");
    second["source"] = json!({"name":"all","capture":"0".repeat(64)});
    second["target"]["resource"] = json!("./rows.json");
    save(dir.path(), &[first, second])?;
    assert!(
        run(dir.path(), &["links"], &[])?
            .error
            .contains("duplicate Link definition")
    );
    Ok(())
}

#[test]
fn refreshed_absence_is_saved_without_fallback_and_gc_removes_abandoned_policies() -> Result<()> {
    let dir = setup()?;
    configure_resources(dir.path(), json!({"rows.json":"refreshed"}), true)?;
    fs::remove_file(dir.path().join("rows.json"))?;
    let absent = success(run(dir.path(), &["refresh", "rows.json"], &[])?);
    assert_eq!(absent[0]["state"]["exists"], false);
    fs::write(dir.path().join("rows.json"), "[1]")?;
    let failed = run(dir.path(), &["read"], &[json!({"name":"all"})])?;
    assert_eq!(failed.code, 2);
    assert!(failed.error.contains("does not exist"));
    assert_eq!(observations_count(dir.path())?, 1);
    configure_resources(dir.path(), json!({}), true)?;
    let removed = success(run(dir.path(), &["gc"], &[])?);
    assert_eq!(removed[0]["observations"], json!([absent[0]["id"]]));
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(dir.path().join(".monika/current.json"))?)?,
        json!({})
    );
    assert_eq!(
        success(run(dir.path(), &["read"], &[json!({"name":"all"})])?)[0]["region"]["content"]["count"],
        1
    );
    Ok(())
}
