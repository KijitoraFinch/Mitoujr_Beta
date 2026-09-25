use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use monika::editing::{Change, Destination, Pick};
use monika::operations::{self, Operation, Response};
use monika::query::{self, Expression, Operand, Predicate};
use monika::workspace::Workspace;
use serde_json::Value;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

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
    /// Create monika.json with no built-in extensions or sources
    Init,
    /// Resolve a selector or @named target and display its content
    Show { target: String },
    /// List declarations, evidence, unresolved endpoints, and conflicts
    Links(FilterArgs),
    /// Check endpoints, conflicts, and configured annotation checkers
    Check(FilterArgs),
    /// Add, replace, remove, or copy a declaration in its source resource
    Link {
        #[command(subcommand)]
        command: LinkCommand,
    },
    /// Apply a saved patch after checking the resource's current state
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
    #[arg(long = "in", help = "Resource containing the declaration")]
    resource: String,
    #[arg(long, help = "External extraction/editing extension")]
    extension: String,
    #[arg(long, help = "Save a reviewable patch instead of applying it")]
    patch_out: Option<PathBuf>,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct PickArgs {
    #[arg(long)]
    id: Option<String>,
    #[arg(long, help = "Evidence location as JSON")]
    at: Option<String>,
}

#[derive(Subcommand)]
enum LinkCommand {
    Add {
        #[command(flatten)]
        edit: EditArgs,
        #[arg(long, help = "Declaration JSON file; '-' reads stdin")]
        declaration: PathBuf,
    },
    Set {
        #[command(flatten)]
        edit: EditArgs,
        #[command(flatten)]
        pick: PickArgs,
        #[arg(long)]
        declaration: PathBuf,
    },
    Remove {
        #[command(flatten)]
        edit: EditArgs,
        #[command(flatten)]
        pick: PickArgs,
    },
    Copy {
        #[command(flatten)]
        edit: EditArgs,
        #[command(flatten)]
        pick: PickArgs,
        #[arg(long)]
        from: String,
        #[arg(long)]
        from_extension: String,
    },
}

impl PickArgs {
    fn value(self) -> Result<Pick> {
        Ok(Pick {
            id: self.id,
            location: self
                .at
                .map(|value| serde_json::from_str(&value))
                .transpose()
                .context("invalid evidence location JSON")?,
        })
    }
}

fn edit_operation(command: LinkCommand) -> Result<Operation> {
    let (edit, change) = match command {
        LinkCommand::Add { edit, declaration } => (
            edit,
            Change::Add {
                declaration: serde_json::from_value(read_json(Some(&declaration))?)?,
            },
        ),
        LinkCommand::Set {
            edit,
            pick,
            declaration,
        } => (
            edit,
            Change::Set {
                pick: pick.value()?,
                declaration: serde_json::from_value(read_json(Some(&declaration))?)?,
            },
        ),
        LinkCommand::Remove { edit, pick } => (
            edit,
            Change::Remove {
                pick: pick.value()?,
            },
        ),
        LinkCommand::Copy {
            edit,
            pick,
            from,
            from_extension,
        } => (
            edit,
            Change::Copy {
                from: Destination {
                    resource: from,
                    extension: from_extension,
                },
                pick: pick.value()?,
            },
        ),
    };
    Ok(Operation::Edit {
        destination: Destination {
            resource: edit.resource,
            extension: edit.extension,
        },
        change,
        patch_out: edit.patch_out,
    })
}

fn read_json(path: Option<&PathBuf>) -> Result<Value> {
    let bytes = match path {
        Some(path) if path.as_os_str() != "-" => {
            std::fs::read(path).with_context(|| format!("read {}", path.display()))?
        }
        _ => {
            let mut bytes = Vec::new();
            io::stdin().read_to_end(&mut bytes)?;
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

fn operation(command: Command) -> Result<Operation> {
    Ok(match command {
        Command::Init => Operation::Init,
        Command::Show { target } => Operation::Show {
            target: query::target(&target)?,
        },
        Command::Links(args) => Operation::Links {
            filter: filter(args)?,
        },
        Command::Check(args) => Operation::Check {
            filter: filter(args)?,
        },
        Command::Link { command } => edit_operation(command)?,
        Command::Apply { patch } => Operation::Apply {
            patch: serde_json::from_value(read_json(Some(&patch))?)?,
        },
        Command::Extension { command } => match command {
            ExtensionCommand::List => Operation::ExtensionList,
            ExtensionCommand::Show { name } => Operation::ExtensionShow { name },
            ExtensionCommand::Test { cases } => Operation::ExtensionTest {
                selectors: serde_json::from_value(read_json(Some(&cases))?)?,
            },
        },
        Command::Request { file } => {
            serde_json::from_value(read_json(file.as_ref())?).context("invalid operation")?
        }
    })
}

fn print_human(response: &Response) -> Result<()> {
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
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
            for link in selection["links"]
                .as_array()
                .context("invalid selection result")?
            {
                let key = link["key"].as_str().context("missing link key")?;
                let occurrences = link["occurrences"].as_array().context("missing evidence")?;
                let ids: std::collections::BTreeSet<_> = occurrences
                    .iter()
                    .filter_map(|occurrence| occurrence["declaration"]["id"].as_str())
                    .collect();
                writeln!(
                    stdout,
                    "{} [{}]  {} -> {}  ({} evidence)",
                    &key[..12],
                    ids.into_iter().collect::<Vec<_>>().join(", "),
                    link["source"]["selector"],
                    link["target"]["selector"],
                    occurrences.len()
                )?;
                for occurrence in occurrences {
                    let evidence = &occurrence["evidence"];
                    writeln!(
                        stdout,
                        "  declared in {} at {}{}",
                        evidence["resource"],
                        evidence["location"],
                        if evidence["writable"] == true {
                            ""
                        } else {
                            " (read-only)"
                        }
                    )?;
                }
                writeln!(
                    stdout,
                    "  annotation: {}",
                    occurrences[0]["declaration"]["annotation"]
                )?;
                for endpoint in ["source", "target"] {
                    if let Some(error) = link[endpoint]["error"].as_str() {
                        writeln!(stdout, "  {endpoint} unresolved: {error}")?;
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
                    unknown["link"]["key"], unknown["reason"]
                )?;
            }
            for conflict in selection["conflicts"]
                .as_array()
                .context("missing conflicts")?
            {
                writeln!(
                    stdout,
                    "conflict {}: {}",
                    conflict["id"], conflict["variants"]
                )?;
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
                                group["key"], group["extension"], finding["message"]
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
    Ok(())
}

fn main() -> ExitCode {
    #[cfg(unix)]
    if let Err(error) = monika::cancellation::install() {
        eprintln!("{error:#}");
        return ExitCode::from(2);
    }
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
                println!(
                    "{}",
                    serde_json::to_string(&Response::error(error)).expect("serializable CLI error")
                );
                return ExitCode::from(2);
            }
            error.exit();
        }
    };
    let json = cli.json || matches!(cli.command, Command::Request { .. });
    let prepared = (|| -> Result<Response> {
        let operation = operation(cli.command)?;
        let root = match cli.root {
            Some(root) => root,
            None => Workspace::discover_root(&std::env::current_dir()?)?,
        };
        let workspace = if cli.snapshot.is_some() {
            Workspace::for_replay(&root)?
        } else {
            Workspace::open(&root)?
        };
        Ok(operations::execute(
            workspace,
            cli.snapshot.as_deref(),
            operation,
        ))
    })();
    let mut response = prepared.unwrap_or_else(|error| Response::error(format!("{error:#}")));
    if let Some(code) = monika::cancellation::exit_code() {
        let snapshot = response.snapshot;
        response = Response::error(
            "operation interrupted; inspect any resource being written before retrying",
        );
        response.snapshot = snapshot;
        response.exit_code = code;
    }
    let printed = if json {
        (|| -> Result<()> {
            let mut out = io::stdout().lock();
            serde_json::to_writer(&mut out, &response)?;
            writeln!(out)?;
            Ok(())
        })()
    } else {
        print_human(&response)
    };
    if let Err(error) = printed {
        eprintln!("error writing output: {error:#}");
        return ExitCode::from(2);
    }
    ExitCode::from(response.exit_code)
}
