use std::collections::HashSet;
use std::io::IsTerminal;
use std::sync::atomic::Ordering;

use anyhow::{Result, bail};
use clap::{CommandFactory, Parser, error::ErrorKind};
use pep508_rs::Requirement;

use check_updates::{
    Interrupted, changes, dependencies,
    indexes::ReleaseClient,
    interactive, locking, output,
    selection::{self, Selection},
    workspace,
};

#[derive(Debug, Parser)]
#[command(
    name = "check-updates",
    version,
    about = "Check stable Python releases; lock existing uv projects after updates, without installing packages."
)]
struct Args {
    /// Write dependency updates to pyproject.toml.
    #[arg(short = 'u', long, conflicts_with_all = ["interactive", "check", "diff"])]
    upgrade: bool,
    /// Check the uv workspace root and included members.
    #[arg(short = 'w', long)]
    workspace: bool,
    /// Select updates, preview changes, and confirm before writing.
    #[arg(short = 'i', long, conflicts_with_all = ["check", "diff"])]
    interactive: bool,
    /// Skip automatic uv lock after writes.
    #[arg(long)]
    no_lock: bool,
    /// Update limit: patch, minor, or latest.
    #[arg(long, default_value = "latest", value_parser = ["patch", "minor", "latest"])]
    target: String,
    /// Include package (repeatable).
    #[arg(long)]
    include: Vec<String>,
    /// Exclude package (repeatable).
    #[arg(long)]
    exclude: Vec<String>,
    /// Select dependency group (repeatable).
    #[arg(long)]
    group: Vec<String>,
    /// Select optional extra (repeatable).
    #[arg(long)]
    extra: Vec<String>,
    /// Select project.dependencies.
    #[arg(long)]
    runtime: bool,
    /// Output format; JSON is read-only.
    #[arg(long, default_value = "table", value_parser = ["table", "json"])]
    format: String,
    /// Read-only; exit 3 when updates exist.
    #[arg(long)]
    check: bool,
    /// Show a read-only unified diff.
    #[arg(long)]
    diff: bool,
    /// Simple index URL (repeatable, first-match order).
    #[arg(long)]
    index: Vec<String>,
    /// Bypass the one-hour persistent PyPI cache.
    #[arg(long)]
    no_cache: bool,
    /// Write detailed diagnostics to stderr.
    #[arg(long)]
    debug: bool,
}

fn invalid(message: &str) -> ! {
    Args::command()
        .error(ErrorKind::ArgumentConflict, message)
        .exit()
}

fn debug(enabled: bool, message: impl std::fmt::Display) {
    if enabled {
        eprintln!("DEBUG: {}", changes::safe_text(&message.to_string(), false));
    }
}

fn collect(args: &Args) -> Result<(std::path::PathBuf, output::Pending)> {
    debug(
        args.debug,
        format!("Discovering projects; workspace={}", args.workspace),
    );
    let projects = workspace::load_projects(args.workspace)?;
    let root = &projects[0];
    let root_directory = root
        .path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
        .canonicalize()?;
    let normalized = |names: &[String]| -> HashSet<String> {
        names
            .iter()
            .map(|name| dependencies::normalize(name))
            .collect()
    };
    let selection = Selection {
        target: args.target.clone(),
        include: normalized(&args.include),
        exclude: normalized(&args.exclude),
        groups: normalized(&args.group),
        extras: normalized(&args.extra),
        runtime: args.runtime,
    };
    let mut client = ReleaseClient::new(args.no_cache)?;
    let mut pending = Vec::new();
    for project in &projects {
        check_updates::check_interrupt()?;
        debug(
            args.debug,
            format!("Checking project {}", project.path.display()),
        );
        let local_names = workspace::workspace_dependencies(root, project)?;
        let python = selection::python_baseline(
            project
                .document
                .get("project")
                .and_then(|p| p.get("requires-python"))
                .map(|value| {
                    value
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("project.requires-python must be a string"))
                })
                .transpose()?,
        )?;
        let mut updates = Vec::new();
        for dependency in dependencies::dependencies(&project.document) {
            check_updates::check_interrupt()?;
            let requirement: Requirement =
                dependencies::parse_requirement(&dependency.requirement)?;
            if !selection.accepts(&dependency, &requirement) {
                continue;
            }
            let name = dependencies::normalize(requirement.name.as_ref());
            debug(
                args.debug,
                format!(
                    "Checking {name} in {}[{}]",
                    dependency.section.join("."),
                    dependency.index
                ),
            );
            let reason = if local_names.contains(&name) {
                Some("workspace dependency".to_owned())
            } else {
                dependencies::unsupported_reason(&requirement).map(str::to_owned)
            }
            .or_else(|| {
                (selection.target != "latest" && dependencies::baseline(&requirement).is_none())
                    .then(|| format!("no version baseline for {} updates", selection.target))
            });
            if let Some(reason) = reason {
                let prefix = if args.workspace {
                    format!("{}: ", project.path.display())
                } else {
                    String::new()
                };
                let hint = if local_names.contains(&name) && !args.workspace {
                    " Use -w to also check workspace members."
                } else {
                    ""
                };
                eprintln!(
                    "{}",
                    changes::safe_text(
                        &format!(
                            "{prefix}Skipped {}: {reason}.{hint}",
                            dependency.requirement
                        ),
                        false
                    )
                );
                continue;
            }
            let indexes =
                workspace::package_indexes(root, project, &name, &args.index, &dependency.section)?;
            let releases = client.get(&name, &indexes)?;
            check_updates::check_interrupt()?;
            if releases.is_empty() {
                bail!("No stable, non-yanked release found for {name}.");
            }
            if let Some(version) = selection::choose_version(
                &requirement,
                &releases,
                &selection.target,
                python.as_ref(),
            ) && let Some(replacement) =
                dependencies::replacement(&requirement, &dependency.requirement, &version)
            {
                debug(
                    args.debug,
                    format!(
                        "Update available for {name}: {} -> {version}",
                        dependency.requirement
                    ),
                );
                let mut release_link = client.release_link(&name, &indexes);
                if release_link.as_deref() == Some(&format!("https://pypi.org/project/{name}/")) {
                    release_link = Some(format!("https://pypi.org/project/{name}/{version}/"));
                }
                let project_link = release_link
                    .as_ref()
                    .filter(|link| link.starts_with(&format!("https://pypi.org/project/{name}/")))
                    .and_then(|_| client.project_link(&name));
                updates.push(dependencies::Update {
                    dependency,
                    replacement,
                    version,
                    release_link,
                    project_link,
                });
            } else {
                debug(args.debug, format!("No update for {name}"));
            }
        }
        if !updates.is_empty() {
            pending.push((project.clone(), updates));
        }
    }
    Ok((root_directory, pending))
}

fn run(args: &Args) -> Result<i32> {
    let (root_directory, pending) = collect(args)?;
    check_updates::check_interrupt()?;
    if args.format == "json" {
        println!("{}", output::json_updates(&pending));
    } else if args.diff {
        for (project, updates) in &pending {
            print!(
                "{}",
                changes::preview_diff(&changes::prepare_change(project, updates)?)
            );
        }
    } else if pending.is_empty() {
        println!("No dependency updates available.");
    } else if args.interactive {
        let selected = interactive::select_updates(&pending)?;
        if selected.is_empty() {
            println!("No updates selected. No files were written.");
            return Ok(0);
        }
        let changes: Vec<_> = selected
            .iter()
            .map(|(project, updates)| changes::prepare_change(project, updates))
            .collect::<Result<_>>()?;
        for change in &changes {
            print!("{}", changes::preview_diff(change));
        }
        for (_, updates) in &selected {
            for update in updates {
                if let Some(link) = &update.release_link {
                    println!("Release: {}", changes::safe_text(link, false));
                }
                if let Some(link) = &update.project_link {
                    println!("Release notes: {}", changes::safe_text(link, false));
                }
            }
        }
        println!(
            "Selected {} dependencies in {} files.",
            changes.iter().map(|change| change.count).sum::<usize>(),
            changes.len()
        );
        if !interactive::confirm_changes()? {
            println!("Cancelled. No files were written.");
            return Ok(0);
        }
        changes::write_changes(&changes)?;
        if !args.no_lock {
            locking::lock_project(&root_directory)?;
        }
    } else {
        output::show_updates(&pending, args.upgrade, args.workspace);
        if args.upgrade {
            let changes: Vec<_> = pending
                .iter()
                .map(|(project, updates)| changes::prepare_change(project, updates))
                .collect::<Result<_>>()?;
            changes::write_changes(&changes)?;
            if !args.no_lock {
                locking::lock_project(&root_directory)?;
            }
        }
    }
    Ok(if args.check && !pending.is_empty() {
        3
    } else {
        0
    })
}

fn main() {
    let args = Args::parse();
    if args.no_lock && !(args.upgrade || args.interactive) {
        invalid("--no-lock requires --upgrade (-u) or --interactive (-i).");
    }
    if args.format == "json" && (args.upgrade || args.interactive || args.diff) {
        invalid("--format json cannot be combined with -u, -i, or --diff.");
    }
    if args.interactive && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        invalid(
            "--interactive (-i) requires terminal input and output. Use -u for automatic updates.",
        );
    }
    if let Err(error) =
        ctrlc::set_handler(|| check_updates::INTERRUPTED.store(true, Ordering::Relaxed))
    {
        eprintln!("Error: cannot install interrupt handler: {error}");
        std::process::exit(1);
    }
    match run(&args) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            let interrupted = error.downcast_ref::<Interrupted>().is_some()
                || check_updates::INTERRUPTED.load(Ordering::Relaxed);
            eprintln!(
                "Error: {}",
                changes::safe_text(&format!("{error:#}"), false)
            );
            debug(
                args.debug,
                "Command failed; Rust error context is shown above.",
            );
            std::process::exit(if interrupted { 130 } else { 1 });
        }
    }
}
