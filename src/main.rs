use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use monika::cancellation::{Interrupt, Signals};
use monika::editing::{Change, Destination};
use monika::extensions::process::Host;
use monika::operations::{self, Operation, Response};
use monika::query::{self, Expression, Operand, Predicate};
use monika::workspace::Workspace;
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Parser)]
#[command(
    name = "monika",
    version,
    about = "Inspect declared links using external extensions"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Project root (otherwise discover monika.json or .git)"
    )]
    root: Option<PathBuf>,
    #[arg(long, global = true, help = "Reuse a saved observation snapshot")]
    snapshot: Option<String>,
    #[arg(
        long,
        global = true,
        help = "Emit the structured response, including errors"
    )]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create configuration and the dedicated Sidecar
    Init,
    /// Resolve a selector or @named target and display its content
    Show { target: String },
    /// Query Links in the Sidecar
    Links(FilterArgs),
    /// Resolve selected Links and run annotation checks
    Check {
        #[command(flatten)]
        filter: FilterArgs,
        #[arg(
            long,
            help = "Compare selected Links with the Sidecar to detect semantic duplicates"
        )]
        duplicates: bool,
    },
    /// Add, replace, or remove a Link in the Sidecar
    Link {
        #[command(subcommand)]
        command: LinkCommand,
    },
    /// Import Links from a fixed Observation into the Sidecar
    Import {
        resource: String,
        #[arg(long)]
        extension: String,
        #[command(flatten)]
        edit: EditArgs,
    },
    /// Generate an external representation of selected Links
    Generate {
        resource: String,
        #[arg(long)]
        extension: String,
        #[command(flatten)]
        filter: FilterArgs,
        #[command(flatten)]
        edit: EditArgs,
    },
    /// Apply a saved patch after checking its original state
    Apply { patch: PathBuf },
    /// Inspect and test configured external extensions
    Extension {
        #[command(subcommand)]
        command: ExtensionCommand,
    },
    /// Execute one JSON operation from FILE or stdin; always returns JSON
    Request { file: Option<PathBuf> },
}

#[derive(Args)]
struct FilterArgs {
    #[arg(
        long = "where",
        help = "Boolean condition using annotation values or region comparisons"
    )]
    condition: Option<String>,
    #[arg(long, help = "Match a source region (selector or @name)")]
    from: Option<String>,
    #[arg(long, help = "Match a target region (selector or @name)")]
    to: Option<String>,
    #[arg(long, help = "Include regions contained by --from or --to")]
    within: bool,
}

#[derive(Subcommand)]
enum ExtensionCommand {
    List,
    Show {
        name: String,
    },
    /// Test actual resolutions and the equivalence/containment laws for sample selectors
    Test {
        #[arg(long, help = "JSON array of selectors; '-' reads stdin")]
        cases: PathBuf,
    },
}

#[derive(Args)]
struct EditArgs {
    #[arg(long, help = "Save a reviewable patch instead of applying it")]
    patch_out: Option<PathBuf>,
}

#[derive(Subcommand)]
enum LinkCommand {
    Add {
        #[command(flatten)]
        edit: EditArgs,
        #[arg(long, help = "Link JSON file, including its ID; '-' reads stdin")]
        link: PathBuf,
    },
    Set {
        #[command(flatten)]
        edit: EditArgs,
        #[arg(
            long,
            help = "Replacement Link JSON with the existing ID; '-' reads stdin"
        )]
        link: PathBuf,
    },
    Remove {
        #[command(flatten)]
        edit: EditArgs,
        id: String,
    },
}

async fn edit_operation(command: LinkCommand) -> Result<Operation> {
    let (edit, change) = match command {
        LinkCommand::Add { edit, link } => (
            edit,
            Change::Add {
                link: serde_json::from_value(read_json(Some(&link)).await?)?,
            },
        ),
        LinkCommand::Set { edit, link } => (
            edit,
            Change::Set {
                link: serde_json::from_value(read_json(Some(&link)).await?)?,
            },
        ),
        LinkCommand::Remove { edit, id } => (edit, Change::Remove { id }),
    };
    Ok(Operation::Edit {
        change,
        patch_out: edit.patch_out,
    })
}

async fn read_json(path: Option<&PathBuf>) -> Result<Value> {
    let bytes = match path {
        Some(path) if path.as_os_str() != "-" => tokio::fs::read(path)
            .await
            .with_context(|| format!("read {}", path.display()))?,
        _ => {
            let mut bytes = Vec::new();
            tokio::io::stdin().read_to_end(&mut bytes).await?;
            bytes
        }
    };
    serde_json::from_slice(&bytes).context("invalid JSON input")
}

fn filter(args: FilterArgs) -> Result<Option<Expression>> {
    anyhow::ensure!(
        !args.within || args.from.is_some() || args.to.is_some(),
        "--within requires --from or --to"
    );
    let mut clauses = Vec::new();
    if let Some(condition) = args.condition {
        clauses.push(Expression::parse(&condition)?);
    }
    for (field, target) in [("source", args.from), ("target", args.to)] {
        if let Some(target) = target {
            clauses.push(Expression::Compare {
                field: field.into(),
                relation: if args.within {
                    Predicate::Within
                } else {
                    Predicate::Equal
                },
                value: Operand::Region(query::target(&target)?),
            });
        }
    }
    Ok(match clauses.len() {
        0 => None,
        1 => clauses.pop(),
        _ => Some(Expression::And { args: clauses }),
    })
}

async fn operation(command: Command) -> Result<Operation> {
    Ok(match command {
        Command::Init => Operation::Init,
        Command::Show { target } => Operation::Show {
            target: query::target(&target)?,
        },
        Command::Links(args) => Operation::Links {
            filter: filter(args)?,
        },
        Command::Check {
            filter: args,
            duplicates,
        } => Operation::Check {
            filter: filter(args)?,
            duplicates,
        },
        Command::Import {
            resource,
            extension,
            edit,
        } => Operation::Import {
            source: Destination {
                resource,
                extension,
            },
            patch_out: edit.patch_out,
        },
        Command::Generate {
            resource,
            extension,
            filter: args,
            edit,
        } => Operation::Generate {
            destination: Destination {
                resource,
                extension,
            },
            filter: filter(args)?,
            patch_out: edit.patch_out,
        },
        Command::Link { command } => edit_operation(command).await?,
        Command::Apply { patch } => Operation::Apply {
            patch: serde_json::from_value(read_json(Some(&patch)).await?)?,
        },
        Command::Extension { command } => match command {
            ExtensionCommand::List => Operation::ExtensionList,
            ExtensionCommand::Show { name } => Operation::ExtensionShow { name },
            ExtensionCommand::Test { cases } => Operation::ExtensionTest {
                selectors: serde_json::from_value(read_json(Some(&cases)).await?)?,
            },
        },
        Command::Request { file } => {
            serde_json::from_value(read_json(file.as_ref()).await?).context("invalid operation")?
        }
    })
}

fn render_human(response: &Response) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(error) = &response.error {
        writeln!(stderr, "error: {error}")?;
    }
    if let Some(data) = &response.data {
        if let Some(content) = data.pointer("/region/content") {
            if let Some(text) = content.as_str() {
                writeln!(stdout, "{text}")?;
            } else {
                writeln!(stdout, "{}", serde_json::to_string_pretty(content)?)?;
            }
        } else if data.get("links").is_some() || data.get("selection").is_some() {
            let selection = data.get("selection").unwrap_or(data);
            let mut origins: std::collections::BTreeMap<&str, Vec<&Value>> =
                std::collections::BTreeMap::new();
            for origin in selection["origins"].as_array().context("missing origins")? {
                origins
                    .entry(origin["link"].as_str().context("missing origin Link ID")?)
                    .or_default()
                    .push(origin);
            }
            for link in selection["links"]
                .as_array()
                .context("invalid selection result")?
            {
                writeln!(
                    stdout,
                    "{}  {} -> {}",
                    link["id"].as_str().context("missing Link ID")?,
                    link["source"],
                    link["reference"]["target"]
                )?;
                writeln!(stdout, "  annotation: {}", link["annotation"])?;
                if let Some(imports) = origins.get(link["id"].as_str().unwrap()) {
                    for origin in imports {
                        let id = origin["observation"]
                            .as_str()
                            .context("missing origin Observation ID")?;
                        let observation = &selection["observations"][id];
                        writeln!(
                            stdout,
                            "  imported from {} at {} (Observation {})",
                            observation["resource"], origin["location"], id
                        )?;
                    }
                }
            }
            for unknown in selection["undetermined"]
                .as_array()
                .context("missing undetermined results")?
            {
                writeln!(
                    stdout,
                    "undetermined {}: {}",
                    unknown["link"]["id"], unknown["reason"]
                )?;
            }
            if let Some(duplicates) = data["duplicates"].as_array() {
                for pair in duplicates {
                    writeln!(stdout, "duplicate Links: {} and {}", pair[0], pair[1])?;
                }
            }
            for diagnostic in selection["diagnostics"]
                .as_array()
                .context("missing diagnostics")?
            {
                writeln!(
                    stderr,
                    "{}: {}: {}",
                    diagnostic["stage"], diagnostic["subject"], diagnostic["message"]
                )?;
            }
            if let Some(groups) = data["findings"].as_array() {
                for group in groups {
                    for finding in group["findings"].as_array().context("invalid findings")? {
                        if finding["status"] == "fail" {
                            writeln!(
                                stdout,
                                "FAIL {} {}: {}",
                                group["id"], group["extension"], finding["message"]
                            )?;
                        }
                    }
                }
                writeln!(
                    stdout,
                    "check: {}",
                    if data["valid"] == true {
                        "passed"
                    } else {
                        "failed"
                    }
                )?;
            }
            writeln!(
                stderr,
                "{} links; {} undetermined; complete={}",
                selection["links"].as_array().unwrap().len(),
                selection["undetermined"].as_array().unwrap().len(),
                response.complete
            )?;
        } else {
            writeln!(stdout, "{}", serde_json::to_string_pretty(data)?)?;
        }
    }
    if let Some(snapshot) = &response.snapshot {
        writeln!(stderr, "snapshot: {snapshot}")?;
    }
    Ok((stdout, stderr))
}

async fn emit(response: &Response, json: bool) -> Result<()> {
    let (stdout, stderr) = if json {
        let mut bytes = serde_json::to_vec(response)?;
        bytes.push(b'\n');
        (bytes, Vec::new())
    } else {
        render_human(response)?
    };
    let mut output = tokio::io::stdout();
    output.write_all(&stdout).await?;
    output.flush().await?;
    let mut errors = tokio::io::stderr();
    errors.write_all(&stderr).await?;
    errors.flush().await?;
    Ok(())
}

fn interrupted(signal: Interrupt, snapshot: Option<String>) -> Response {
    let mut response = Response::error(
        "operation interrupted; inspect any resource being written before retrying",
    );
    response.snapshot = snapshot;
    response.exit_code = signal.exit_code();
    response
}

async fn run() -> Result<u8> {
    let mut signals = Signals::new()?;
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let structured = std::env::args_os().any(|arg| arg == "--json" || arg == "request");
            if structured
                && !matches!(
                    error.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                )
            {
                emit(&Response::error(error), true).await?;
                return Ok(2);
            }
            error.exit();
        }
    };
    let json = cli.json || matches!(cli.command, Command::Request { .. });
    let mut host = Host::default();
    let mut interruption = None;
    let mut response = tokio::select! {
        biased;
        signal = signals.recv() => {
            let signal = signal?;
            interruption = Some(signal);
            interrupted(signal, None)
        }
        result = async {
            let operation = operation(cli.command).await?;
            let root = match cli.root {
                Some(root) => root,
                None => Workspace::discover_root(&std::env::current_dir()?).await?,
            };
            let workspace = if cli.snapshot.is_some() {
                Workspace::for_replay(&root).await?
            } else {
                Workspace::open(&root).await?
            };
            Ok::<_, anyhow::Error>(operations::execute(workspace, cli.snapshot.as_deref(), operation, &mut host).await)
        } => result.unwrap_or_else(|error| Response::error(format!("{error:#}"))),
    };

    // The host survives cancellation of the operation and owns all subprocesses.
    let cleanup = loop {
        tokio::select! {
            biased;
            signal = signals.recv() => {
                let signal = signal?;
                if interruption.is_some() {
                    host.terminate()?;
                    return Ok(signal.exit_code());
                }
                interruption = Some(signal);
            }
            result = host.shutdown(interruption.is_some()) => break result,
        }
    };
    if let Some(signal) = interruption {
        response = interrupted(signal, response.snapshot);
    }
    if let Err(error) = cleanup {
        response.ok = false;
        response.complete = false;
        response.error = Some(match response.error {
            Some(previous) => format!("{previous}; {error:#}"),
            None => format!("{error:#}"),
        });
        if interruption.is_none() {
            response.exit_code = 2;
        }
    }
    tokio::select! {
        biased;
        signal = signals.recv() => Ok(signal?.exit_code()),
        result = emit(&response, json) => { result?; Ok(response.exit_code) }
    }
}

fn main() -> ExitCode {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("create Tokio runtime");
    let result = runtime.block_on(run());
    // Tokio's standard-input reader is a blocking task that cannot be cancelled.
    // All extensions have been shut down explicitly before reaching this point.
    // Do not wait for stdin EOF again while exiting this one-shot CLI.
    runtime.shutdown_background();
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::from(2)
        }
    }
}
