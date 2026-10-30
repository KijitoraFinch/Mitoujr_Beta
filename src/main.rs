use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use monika::cancellation::Signals;
use monika::engine::Engine;
use monika::extensions::process::Host;
use monika::operations;
use monika::stream::{Input, emit};
use monika::workspace::{Session, Workspace};
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "monika",
    version,
    about = "Record and inspect connections between parts of files and other data",
    after_help = "Data: JSON values on stdin or in FILE; JSON Lines on stdout. Diagnostics: stderr.\nUse jq for selection and transformation. Pipelines require shell pipefail (set -o pipefail).\nKeep related data unchanged during a pipeline. Do not run capture, refresh, or gc concurrently.\nCommit monika.json, monika.links.json, and .monika/; saved data includes Resource content.\nExamples:\n  set -o pipefail\n  monika links | jq -c 'select(.annotation.relation == \"implements\")' | monika check\n  monika links | monika read --at /target\n  monika capture --link spec-impl --endpoint target\n  monika links | monika read --at /target --captured\nExit codes: 0 success, 1 failed checks, 2 error, 130/143 interrupted."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Project root (otherwise discover monika.json or .git)"
    )]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct InputArgs {
    #[arg(default_value = "-", help = "JSON input file; '-' reads stdin")]
    file: PathBuf,
}
#[derive(Args)]
struct AtArgs {
    #[command(flatten)]
    input: InputArgs,
    #[arg(
        long,
        default_value = "",
        help = "JSON Pointer selecting the value in each input record; empty selects the whole record"
    )]
    at: String,
}
#[derive(Subcommand)]
enum Command {
    /// Create monika.json and monika.links.json (edit them with ordinary tools)
    Init,
    /// Emit each saved Link after expansion and normalization; no observation
    Links,
    /// Read a Target from each input; emit {input, selector, region, observations}
    Read {
        #[command(flatten)]
        input: AtArgs,
        #[arg(
            long,
            help = "Read saved Capture content without observation or resolution; accepts endpoints or Capture IDs"
        )]
        captured: bool,
    },
    /// Compare two Targets in each input; emit {input, relation, observations}
    Compare {
        #[command(flatten)]
        input: InputArgs,
        #[arg(
            long,
            default_value = "/left",
            help = "JSON Pointer to the left Target"
        )]
        left: String,
        #[arg(
            long,
            default_value = "/right",
            help = "JSON Pointer to the right Target"
        )]
        right: String,
    },
    /// Check endpoint resolution, Capture content changes, and configured annotation conditions
    Check {
        #[command(flatten)]
        input: AtArgs,
        #[arg(
            long,
            help = "Also find semantic duplicates within these inputs (buffers input)"
        )]
        duplicates: bool,
    },
    /// Resolve and save a Capture; optionally record its ID on a selected Link
    Capture {
        #[arg(
            help = "JSON Target input file; stdin if omitted (without --link)",
            conflicts_with = "link"
        )]
        file: Option<PathBuf>,
        #[arg(
            long,
            default_value = "",
            conflicts_with = "link",
            help = "JSON Pointer selecting a Target in each record"
        )]
        at: String,
        #[arg(
            long,
            help = "Capture a saved Link and update only its selected endpoint(s)"
        )]
        link: Option<String>,
        #[arg(
            long,
            value_enum,
            requires = "link",
            help = "Endpoint(s) reviewed and ready to capture; required with --link"
        )]
        endpoint: Option<CaptureEndpoint>,
    },
    /// Observe refreshed Resources and adopt saved Observations as their current state
    Refresh {
        #[arg(
            required_unless_present = "all",
            conflicts_with = "all",
            help = "Resources configured as refreshed"
        )]
        resources: Vec<String>,
        #[arg(
            long,
            help = "Refresh refreshed Resources named in Sidecar endpoint selectors (expands names)"
        )]
        all: bool,
    },
    /// Remove unreferenced Captures and saved Observations; run without concurrent edits
    Gc {
        #[arg(long, help = "List objects to remove without deleting them")]
        dry_run: bool,
        #[arg(
            long,
            value_name = "CAPTURE_ID",
            help = "Also retain this standalone Capture; repeat for multiple IDs on each GC"
        )]
        keep: Vec<String>,
    },
    /// Inspect and test configured external extensions
    Extension {
        #[command(subcommand)]
        command: ExtensionCommand,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum CaptureEndpoint {
    Source,
    Target,
    Both,
}

#[derive(Subcommand)]
enum ExtensionCommand {
    /// Emit each configured extension's manifest
    List,
    /// Emit an extension's manifest
    Show { name: String },
    /// Test comparison laws for a JSON array of Targets (RegionSelectors or named targets)
    Test {
        #[arg(long)]
        cases: PathBuf,
    },
}

fn pointer(value: &str) -> Result<()> {
    ensure!(
        value.is_empty() || value.starts_with('/'),
        "JSON Pointer must be empty or start with '/': {value}"
    );
    // RFC 6901 escape sequences. serde_json::Value::pointer performs traversal.
    for token in value.split('/') {
        let mut chars = token.chars();
        while let Some(ch) = chars.next() {
            if ch == '~' {
                ensure!(
                    matches!(chars.next(), Some('0' | '1')),
                    "invalid JSON Pointer escape: {value}"
                );
            }
        }
    }
    Ok(())
}

async fn execute(cli: Cli, host: &mut Host) -> Result<u8> {
    let root = match cli.root {
        Some(root) => root,
        None => Workspace::discover_root(&std::env::current_dir()?).await?,
    };
    let workspace = Workspace::open(&root).await?;
    if matches!(cli.command, Command::Init) {
        emit(&json!({"config": workspace.initialize().await?})).await?;
        return Ok(0);
    }
    let mut engine = Engine::new(Session::new(workspace), host)?;
    let mut exit = 0;
    match cli.command {
        Command::Init => unreachable!(),
        Command::Links => {
            for link in operations::links(&mut engine).await? {
                emit(&link).await?;
            }
        }
        Command::Read {
            input: args,
            captured,
        } => {
            pointer(&args.at)?;
            let mut input = Input::open(&args.input.file);
            let mut record = 0;
            while let Some(value) = input.next().await? {
                record += 1;
                let result = if captured {
                    operations::read_captured(&engine, value, &args.at).await
                } else {
                    operations::read(&mut engine, value, &args.at).await
                };
                emit(&result.with_context(|| format!("read input record {record}"))?).await?;
            }
        }
        Command::Compare { input, left, right } => {
            pointer(&left)?;
            pointer(&right)?;
            let mut input = Input::open(&input.file);
            let mut record = 0;
            while let Some(value) = input.next().await? {
                record += 1;
                emit(
                    &operations::compare(&mut engine, value, &left, &right)
                        .await
                        .with_context(|| format!("compare input record {record}"))?,
                )
                .await?;
            }
        }
        Command::Check {
            input: args,
            duplicates,
        } => {
            pointer(&args.at)?;
            let mut input = Input::open(&args.input.file);
            let mut checked = Vec::new();
            let mut record = 0;
            while let Some(value) = input.next().await? {
                record += 1;
                let result = operations::check(&mut engine, value, &args.at)
                    .await
                    .with_context(|| format!("check input record {record}"))?;
                if duplicates {
                    checked.push(result);
                } else {
                    if !result.valid {
                        exit = 1;
                    }
                    emit(&result).await?;
                }
            }
            if duplicates {
                operations::duplicates(&mut engine, &mut checked).await?;
                for result in checked {
                    if !result.valid {
                        exit = 1;
                    }
                    emit(&result).await?;
                }
            }
        }
        Command::Capture {
            file,
            at,
            link,
            endpoint,
        } => {
            if let Some(id) = link {
                let endpoints: &[&str] =
                    match endpoint.context("--link requires --endpoint source, target, or both")? {
                        CaptureEndpoint::Source => &["source"],
                        CaptureEndpoint::Target => &["target"],
                        CaptureEndpoint::Both => &["source", "target"],
                    };
                emit(&operations::capture_link(&mut engine, &id, endpoints).await?).await?;
            } else {
                pointer(&at)?;
                let mut input = Input::open(&file.unwrap_or_else(|| PathBuf::from("-")));
                let mut record = 0;
                while let Some(value) = input.next().await? {
                    record += 1;
                    emit(
                        &operations::capture(&mut engine, value, &at)
                            .await
                            .with_context(|| format!("capture input record {record}"))?,
                    )
                    .await?;
                }
            }
        }
        Command::Refresh { resources, all } => {
            for observation in operations::refresh(&mut engine, &resources, all).await? {
                emit(&observation).await?;
            }
        }
        Command::Gc { dry_run, keep } => {
            emit(&monika::storage::gc(&engine.session.workspace, &keep, dry_run).await?).await?;
        }
        Command::Extension { command } => match command {
            ExtensionCommand::List => {
                for manifest in engine.manifests().await? {
                    emit(&manifest).await?;
                }
            }
            ExtensionCommand::Show { name } => emit(&engine.manifest(&name).await?).await?,
            ExtensionCommand::Test { cases } => {
                let targets: Vec<monika::model::Target> =
                    serde_json::from_value(Input::open(&cases).one().await?)?;
                let mut selectors = Vec::new();
                for target in targets {
                    selectors.push(
                        monika::sidecar::canonical_selector(&engine.session.workspace, &target)
                            .await?,
                    );
                }
                let report = monika::extension_test::test_regions(&mut engine, &selectors).await?;
                exit = if !report.complete {
                    2
                } else if report.passed {
                    0
                } else {
                    1
                };
                emit(&report).await?;
            }
        },
    }
    Ok(exit)
}

async fn run() -> Result<u8> {
    let mut signals = Signals::new()?;
    let cli = Cli::parse();
    let mut host = Host::default();
    let mut interruption = None;
    let result = tokio::select! {
        biased;
        signal = signals.recv() => {
            let signal = signal?;
            interruption = Some(signal);
            Ok(signal.exit_code())
        }
        result = execute(cli, &mut host) => result,
    };
    // The Host outlives cancellation and owns every child and temporary input.
    let cleanup = loop {
        tokio::select! {
            biased;
            signal = signals.recv() => {
                let signal = signal?;
                if interruption.is_some() { host.terminate()?; return Ok(signal.exit_code()); }
                interruption = Some(signal);
            }
            result = host.shutdown(interruption.is_some() || result.is_err()) => break result,
        }
    };
    if let Some(signal) = interruption {
        eprintln!(
            "operation interrupted; completed Capture objects may remain; inspect the Sidecar and refresh state before retrying"
        );
        if let Err(error) = cleanup {
            eprintln!("{error:#}");
        }
        return Ok(signal.exit_code());
    }
    if let Err(error) = cleanup {
        if let Err(previous) = result {
            return Err(previous.context(format!("extension shutdown failed: {error:#}")));
        }
        return Err(error);
    }
    result
}

fn main() -> ExitCode {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("create Tokio runtime");
    let result = runtime.block_on(run());
    // A blocking stdin read cannot be cancelled. Child processes were shut down
    // above; process exit must not wait for the input producer to close stdin.
    runtime.shutdown_background();
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::from(2)
        }
    }
}
