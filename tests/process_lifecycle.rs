use anyhow::{Context, Result};
use monika::extensions::process::Host;
use monika::workspace::ProcessExtension;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

fn extension(script: &str, timeout_ms: u64) -> ProcessExtension {
    ProcessExtension {
        command: vec!["python3".into(), "-c".into(), script.into()],
        revision: "1".into(),
        schemes: vec![],
        timeout_ms,
    }
}

#[tokio::test]
async fn normal_shutdown_sends_eof_and_waits_for_extension_cleanup() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = extension(
        r#"
import json, pathlib, sys
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({'protocol': 1, 'id': request['id'], 'result': 'ok'}), flush=True)
pathlib.Path('cleaned-up').write_text('EOF received')
"#,
        2000,
    );
    let mut host = Host::default();
    let worker = host.worker(dir.path(), "clean", &config)?;
    assert_eq!(worker.invoke("ping", Value::Null).await?, "ok");
    host.shutdown(false).await?;
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("cleaned-up")).await?,
        "EOF received"
    );
    Ok(())
}

#[tokio::test]
async fn partial_response_times_out_and_worker_cannot_be_reused() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = extension(
        "import sys,time; sys.stdin.readline(); sys.stdout.write('{'); sys.stdout.flush(); time.sleep(60)",
        150,
    );
    let mut host = Host::default();
    let worker = host.worker(dir.path(), "partial", &config)?;
    let result = timeout(Duration::from_secs(3), worker.invoke("ping", Value::Null)).await?;
    assert!(format!("{:#}", result.unwrap_err()).contains("timed out"));
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
async fn request_errors_preserve_protocol_but_malformed_responses_poison_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = extension(
        r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if request['operation'] == 'bad-protocol':
        print('invalid JSON', flush=True)
    else:
        result = {'error': 'bad input'} if request['operation'] == 'bad-input' else {'result': 'ok'}
        print(json.dumps({'protocol': 1, 'id': request['id'], **result}), flush=True)
"#,
        2000,
    );
    let mut host = Host::default();
    let worker = host.worker(dir.path(), "errors", &config)?;
    assert!(
        worker
            .invoke("bad-input", Value::Null)
            .await
            .unwrap_err()
            .to_string()
            .contains("bad input")
    );
    assert_eq!(worker.invoke("ping", Value::Null).await?, "ok");
    assert!(
        worker
            .invoke("bad-protocol", Value::Null)
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid extension response")
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
async fn shutdown_reports_nonzero_exit_and_still_closes_every_worker() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let good = extension(
        "import json,sys,pathlib; r=json.loads(sys.stdin.readline()); print(json.dumps({'protocol':1,'id':r['id'],'result':0}),flush=True); sys.stdin.read(); pathlib.Path('other-closed').write_text('closed')",
        2000,
    );
    let bad = extension(
        "import json,sys; r=json.loads(sys.stdin.readline()); print(json.dumps({'protocol':1,'id':r['id'],'result':0}),flush=True); sys.stdin.read(); sys.exit(7)",
        2000,
    );
    let mut host = Host::default();
    host.worker(dir.path(), "a-bad", &bad)?
        .invoke("ping", Value::Null)
        .await?;
    host.worker(dir.path(), "b-good", &good)?
        .invoke("ping", Value::Null)
        .await?;
    assert!(
        host.shutdown(false)
            .await
            .unwrap_err()
            .to_string()
            .contains('7')
    );
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("other-closed")).await?,
        "closed"
    );
    Ok(())
}

#[tokio::test]
async fn eof_ignoring_extension_is_terminated_and_reported() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = extension(
        "import json,sys,time; r=json.loads(sys.stdin.readline()); print(json.dumps({'protocol':1,'id':r['id'],'result':0}),flush=True); time.sleep(60)",
        2000,
    );
    let mut host = Host::default();
    host.worker(dir.path(), "ignores-eof", &config)?
        .invoke("ping", Value::Null)
        .await?;
    let result = timeout(Duration::from_secs(3), host.shutdown(false)).await?;
    assert!(result.unwrap_err().to_string().contains("stdin EOF"));
    Ok(())
}

#[cfg(unix)]
async fn wait_until_gone(pid: libc::pid_t) -> Result<()> {
    timeout(Duration::from_secs(3), async {
        loop {
            // SAFETY: signal zero only tests the PID returned by our test child.
            if unsafe { libc::kill(pid, 0) } == -1 {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH)
                );
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("child process survived cleanup")
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_kills_helpers_holding_stdout_after_the_leader_exits() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = extension(
        r#"
import pathlib, subprocess, sys
sys.stdin.readline()
helper = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
pathlib.Path('helper.pid').write_text(str(helper.pid))
sys.exit(0)
"#,
        500,
    );
    let mut host = Host::default();
    let result = host
        .worker(dir.path(), "helper", &config)?
        .invoke("ping", Value::Null)
        .await;
    assert!(result.unwrap_err().to_string().contains("timed out"));
    let pid = tokio::fs::read_to_string(dir.path().join("helper.pid"))
        .await?
        .parse()?;
    wait_until_gone(pid).await?;
    host.shutdown(false).await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn normal_shutdown_also_terminates_helpers_left_by_an_exited_leader() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = extension(
        r#"
import json, subprocess, sys
request = json.loads(sys.stdin.readline())
helper = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
print(json.dumps({'protocol': 1, 'id': request['id'], 'result': helper.pid}), flush=True)
sys.stdin.read()
"#,
        2000,
    );
    let mut host = Host::default();
    let pid = host
        .worker(dir.path(), "helper", &config)?
        .invoke("ping", Value::Null)
        .await?
        .as_i64()
        .unwrap() as libc::pid_t;
    host.shutdown(false).await?;
    wait_until_gone(pid).await?;
    Ok(())
}

fn cli(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_monika"));
    command
        .arg("--root")
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

#[cfg(unix)]
#[tokio::test]
async fn signals_interrupt_stdin_without_waiting_for_eof() -> Result<()> {
    let dir = tempfile::tempdir()?;
    for (signal, code) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
        let mut child = cli(dir.path()).arg("read").spawn()?;
        // Fill beyond the pipe buffer to synchronize with the CLI's first read.
        // The request remains incomplete and stdin stays open throughout exit.
        let mut stdin = child.stdin.take().unwrap();
        timeout(
            Duration::from_secs(3),
            stdin.write_all(&vec![b' '; 4 * 1024 * 1024]),
        )
        .await??;
        // SAFETY: this PID belongs to the live CLI spawned above.
        assert_eq!(
            unsafe { libc::kill(child.id().unwrap() as libc::pid_t, signal) },
            0
        );
        let output = timeout(Duration::from_secs(3), child.wait_with_output()).await??;
        assert_eq!(
            output.status.code(),
            Some(code),
            "status={}, stdout={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("interrupted"));
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn interruption_kills_the_extension_and_its_helper() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let script = r#"
import json, os, pathlib, subprocess, sys, time
sys.stdin.readline()
helper = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
ready = pathlib.Path('ready.tmp')
ready.write_text(json.dumps([os.getpid(), helper.pid]))
ready.replace('ready.json')
time.sleep(60)
"#;
    let config = json!({"version":1,"extensions":{"slow": extension(script, 30000)}});
    tokio::fs::write(dir.path().join("monika.json"), serde_json::to_vec(&config)?).await?;
    let child = cli(dir.path())
        .args(["extension", "show", "slow"])
        .spawn()?;
    timeout(Duration::from_secs(3), async {
        while !tokio::fs::try_exists(dir.path().join("ready.json")).await? {
            sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let pids: Vec<libc::pid_t> =
        serde_json::from_slice(&tokio::fs::read(dir.path().join("ready.json")).await?)?;
    // SAFETY: this PID belongs to the live CLI spawned above.
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGINT) },
        0
    );
    let output = timeout(Duration::from_secs(3), child.wait_with_output()).await??;
    assert_eq!(output.status.code(), Some(130));
    for pid in pids {
        wait_until_gone(pid).await?;
    }
    Ok(())
}

#[tokio::test]
async fn cancelling_a_request_invalidates_the_worker_without_restarting_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = extension(
        "import sys,time; sys.stdin.readline(); time.sleep(60)",
        30000,
    );
    let mut host = Host::default();
    let worker = host.worker(dir.path(), "cancelled", &config)?;
    assert!(
        timeout(
            Duration::from_millis(100),
            worker.invoke("pending", Value::Null)
        )
        .await
        .is_err()
    );
    let error = worker.invoke("next", Value::Null).await.unwrap_err();
    assert!(
        error.to_string().contains("cancelled during communication"),
        "{error:#}"
    );
    timeout(Duration::from_secs(3), host.shutdown(false)).await??;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn interruption_removes_observation_inputs_after_stopping_the_extension() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = json!({"version":1,"extensions":{"sets":{
        "command":["python3",format!("{}/tests/fixtures/reading_extension.py",env!("CARGO_MANIFEST_DIR"))],
        "revision":"test-v1","schemes":["file"]
    }}});
    tokio::fs::write(dir.path().join("monika.json"), serde_json::to_vec(&config)?).await?;
    tokio::fs::write(dir.path().join("rows.json"), b"[1,2,3]").await?;
    let workspace = monika::workspace::Workspace::open(dir.path()).await?;
    let request = json!({"extension":"sets","query":{
        "mode":"hold_input","resource":workspace.normalize_resource("rows.json").await?
    }});
    let mut child = cli(dir.path()).arg("read").spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&request)?)
        .await?;
    let ready = dir.path().join("input-ready.json");
    timeout(Duration::from_secs(3), async {
        while !tokio::fs::try_exists(&ready).await? {
            sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let input: Value = serde_json::from_slice(&tokio::fs::read(&ready).await?)?;
    let path = Path::new(input["path"].as_str().unwrap());
    assert_eq!(tokio::fs::read(path).await?, b"[1,2,3]");
    // SAFETY: this PID belongs to the live CLI spawned above.
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGINT) },
        0
    );
    let output = timeout(Duration::from_secs(3), child.wait_with_output()).await??;
    assert_eq!(output.status.code(), Some(130));
    wait_until_gone(input["pid"].as_i64().unwrap() as libc::pid_t).await?;
    assert!(!tokio::fs::try_exists(path).await?);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn read_emits_a_record_before_stdin_eof() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let dir = tempfile::tempdir()?;
    let config = json!({"version":1,"extensions":{"sets":{
        "command":["python3",format!("{}/tests/fixtures/sets_extension.py",env!("CARGO_MANIFEST_DIR"))],
        "revision":"test-v1","schemes":["file"]
    }}});
    tokio::fs::write(dir.path().join("monika.json"), serde_json::to_vec(&config)?).await?;
    tokio::fs::write(dir.path().join("rows.json"), b"[1,2,3]").await?;
    let mut child = cli(dir.path()).arg("read").spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    stdin
        .write_all(b"{\"extension\":\"sets\",\"resource\":\"rows.json\",\"query\":\"2\"}\n")
        .await?;
    let line = timeout(Duration::from_secs(3), output.next_line())
        .await??
        .context("missing first record")?;
    let value: Value = serde_json::from_str(&line)?;
    assert_eq!(value["region"]["content"]["count"], 2);
    // stdin is still open: the first result cannot depend on reading the batch.
    stdin
        .write_all(b"{\"extension\":\"sets\",\"resource\":\"rows.json\",\"query\":\"3\"}\n")
        .await?;
    let line = timeout(Duration::from_secs(3), output.next_line())
        .await??
        .context("missing second record")?;
    assert_eq!(
        serde_json::from_str::<Value>(&line)?["region"]["content"]["count"],
        1
    );
    drop(stdin);
    let result = timeout(Duration::from_secs(3), child.wait_with_output()).await??;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}

#[tokio::test]
async fn broken_output_pipe_exits_with_an_error() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let link = json!({"id":"one","source":{"extension":"example"},"target":{"extension":"example"},"annotation":"x".repeat(1_000_000)});
    tokio::fs::write(
        dir.path().join("monika.links.json"),
        serde_json::to_vec(&json!({"version":1,"links":[link]}))?,
    )
    .await?;
    let mut child = cli(dir.path()).arg("links").spawn()?;
    drop(child.stdout.take());
    let output = timeout(Duration::from_secs(3), child.wait_with_output()).await??;
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("JSON output"));
    Ok(())
}
