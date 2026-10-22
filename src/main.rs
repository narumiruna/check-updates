use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail, ensure};
use clap::{CommandFactory, Parser, error::ErrorKind};
use pep440_rs::{Operator, Version};
use pep508_rs::{Requirement, VersionOrUrl};
use serde_json::{Value, json};

use check_updates::{
    Interrupted, changes, dependencies, doctor,
    indexes::ReleaseClient,
    interactive, locked, locking, output,
    policy::{self, Settings},
    selection::{self, CandidatePolicy},
    verification::{self, ResolvePolicy},
    workspace,
};

#[derive(Debug, Parser)]
#[command(
    name = "check-updates",
    version,
    about = "Check Python releases; update requirements and existing uv lockfiles without installing packages, except in explicit Doctor mode."
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
    #[arg(long, conflicts_with_all = ["verify", "doctor", "bound"])]
    no_lock: bool,
    /// Update limit: patch, minor, or latest (default: latest).
    #[arg(long, value_parser = ["patch", "minor", "latest"])]
    target: Option<String>,
    /// Include package by exact normalized name (repeatable).
    #[arg(long)]
    include: Vec<String>,
    /// Exclude package by exact normalized name (repeatable).
    #[arg(long)]
    exclude: Vec<String>,
    /// Include a quoted normalized package glob (repeatable; union with include).
    #[arg(long)]
    include_glob: Vec<String>,
    /// Exclude a quoted package glob (repeatable; exclusions win).
    #[arg(long)]
    exclude_glob: Vec<String>,
    /// Select a named workspace project; requires -w (repeatable).
    #[arg(long, requires = "workspace")]
    member: Vec<String>,
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
    /// Read-only; exit 3 when requirement edits exist.
    #[arg(long)]
    check: bool,
    /// Show a read-only unified diff.
    #[arg(long)]
    diff: bool,
    /// Explain selected and blocked versions (read-only; extended JSON schema 2).
    #[arg(long, conflicts_with_all = ["diff", "upgrade", "interactive", "doctor"])]
    explain: bool,
    /// Show unverified lockfile context (read-only; extended JSON schema 2).
    #[arg(long, conflicts_with_all = ["diff", "upgrade", "interactive", "doctor"])]
    show_locked: bool,
    /// Minimum artifact age, for example 7d, 12h, or 30m.
    #[arg(long, value_parser = parse_duration)]
    cooldown: Option<u64>,
    /// Do not raise a requirement that already admits the candidate.
    #[arg(long, conflicts_with = "no_minimal")]
    minimal: bool,
    /// Override a configured minimal policy.
    #[arg(long)]
    no_minimal: bool,
    /// Admit alpha, beta, and RC versions, but not development releases.
    #[arg(long, conflicts_with = "no_pre")]
    pre: bool,
    /// Override a configured prerelease policy.
    #[arg(long)]
    no_pre: bool,
    /// Require an exact candidate, for example 'foo==2.4.1' (repeatable).
    #[arg(long = "to")]
    to: Vec<String>,
    /// Replace one explicit upper bound, for example 'foo<3' (repeatable).
    #[arg(long)]
    bound: Vec<String>,
    /// Resolve the exact selection in a temporary project before writing.
    #[arg(long)]
    verify: bool,
    /// Install and test updates in a temporary project; requires -u and test-command.
    #[arg(long, requires_all = ["upgrade", "test_command"], conflicts_with_all = ["interactive", "verify"])]
    doctor: bool,
    /// Doctor test executable and arguments; put this option last. No implicit shell.
    #[arg(long, requires = "doctor", num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
    test_command: Vec<String>,
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

fn parse_duration(value: &str) -> std::result::Result<u64, String> {
    policy::duration(value).map_err(|e| e.to_string())
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

fn cli_settings(args: &Args) -> Settings {
    let list = |v: &[String]| (!v.is_empty()).then(|| v.to_vec());
    Settings {
        target: args.target.clone(),
        cooldown: args.cooldown,
        pre: if args.pre {
            Some(true)
        } else if args.no_pre {
            Some(false)
        } else {
            None
        },
        minimal: if args.minimal {
            Some(true)
        } else if args.no_minimal {
            Some(false)
        } else {
            None
        },
        runtime: args.runtime.then_some(true),
        include: list(&args.include),
        exclude: list(&args.exclude),
        include_glob: list(&args.include_glob),
        exclude_glob: list(&args.exclude_glob),
        group: list(&args.group),
        extra: list(&args.extra),
        ..Settings::default()
    }
}

fn named_constraints(values: &[String], bound: bool) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for value in values {
        let requirement: Requirement = value
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid explicit package constraint."))?;
        ensure!(
            requirement.extras.is_empty() && requirement.marker.is_true(),
            "Explicit package constraints cannot have extras or markers."
        );
        let Some(VersionOrUrl::VersionSpecifier(specs)) = &requirement.version_or_url else {
            bail!("An explicit version constraint is required.");
        };
        ensure!(
            specs.len() == 1,
            "Use exactly one explicit version constraint per package."
        );
        let spec = &specs[0];
        ensure!(
            if bound {
                matches!(
                    spec.operator(),
                    Operator::LessThan | Operator::LessThanEqual
                )
            } else {
                *spec.operator() == Operator::Equal
            },
            "Use ==VERSION for --to, or <VERSION / <=VERSION for --bound."
        );
        let name = dependencies::normalize(requirement.name.as_ref());
        ensure!(
            result.insert(name, spec.to_string()).is_none(),
            "Duplicate normalized explicit package constraint."
        );
    }
    Ok(result)
}

struct Collected {
    root: PathBuf,
    projects: Vec<workspace::Project>,
    pending: output::Pending,
    reports: Vec<Value>,
    resolver: ResolvePolicy,
    policy_lock: bool,
}

fn cutoff(now: i64, duration: Option<u64>) -> Result<Option<i64>> {
    duration
        .map(|d| {
            now.checked_sub(d as i64)
                .context("Cooldown exceeds the supported timestamp range.")
        })
        .transpose()
}

fn collect(args: &Args) -> Result<Collected> {
    debug(
        args.debug,
        format!("Discovering projects; workspace={}", args.workspace),
    );
    let projects = workspace::load_projects(args.workspace)?;
    let root = &projects[0];
    let root_directory = root
        .path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
        .canonicalize()?;
    let editable = policy::selected_members(&projects, &args.member)?;
    let cli = cli_settings(args);
    cli.validate()?;
    let root_settings = Settings::read(root)?;
    let to = named_constraints(&args.to, false)?;
    let bounds = named_constraints(&args.bound, true)?;
    let mut matched_to = std::collections::HashSet::new();
    let mut matched_bounds = std::collections::HashSet::new();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let mut client = ReleaseClient::new(args.no_cache)?;
    let mut pending = Vec::new();
    let mut reports = Vec::new();
    let effective_root = root_settings.overlay(&cli);
    let mut resolver = ResolvePolicy {
        cutoff: cutoff(now, effective_root.cooldown)?,
        prerelease: effective_root.pre.unwrap_or(false),
        ..Default::default()
    };
    let mut ages = BTreeMap::new();
    let mut age_conflict = false;
    let mut prerelease_conflict = false;
    let root_prerelease = resolver.prerelease;
    let mut policy_lock = resolver.cutoff.is_some() || resolver.prerelease;
    for project in &projects {
        check_updates::check_interrupt()?;
        let local = Settings::read(project)?;
        let settings = if args.workspace {
            root_settings.overlay(&local)
        } else {
            local
        }
        .overlay(&cli);
        settings.validate()?;
        if editable.contains(&project.path) {
            age_conflict |= settings.cooldown != effective_root.cooldown;
            prerelease_conflict |= settings.pre.unwrap_or(false) != root_prerelease;
        }
        for name in settings.packages.keys() {
            let age = cutoff(now, settings.package_policy(name, &cli).cooldown)?;
            policy_lock |= age.is_some();
            if let Some(previous) = ages.insert(name.clone(), age) {
                age_conflict |= previous != age;
            }
        }
        if !editable.contains(&project.path) {
            continue;
        }
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
                .map(|v| {
                    v.as_str()
                        .context("project.requires-python must be a string")
                })
                .transpose()?,
        )?;
        let mut updates = Vec::new();
        for dependency in dependencies::dependencies(&project.document) {
            check_updates::check_interrupt()?;
            let original_req = dependencies::parse_requirement(&dependency.requirement)?;
            if !settings.accepts(&dependency, &original_req) {
                continue;
            }
            let name = dependencies::normalize(original_req.name.as_ref());
            let package = settings.package_policy(&name, &cli);
            let age = cutoff(now, package.cooldown)?;
            if let Some(previous) = ages.insert(name.clone(), age) {
                age_conflict |= previous != age;
            }
            policy_lock |= age.is_some() || settings.pre.unwrap_or(false);
            resolver.prerelease |= settings.pre.unwrap_or(false);
            let reason = if local_names.contains(&name) {
                Some("workspace dependency".to_owned())
            } else {
                dependencies::unsupported_reason(&original_req).map(str::to_owned)
            }
            .or_else(|| {
                (package.target.as_deref().unwrap_or("latest") != "latest"
                    && dependencies::baseline(&original_req).is_none())
                .then(|| {
                    format!(
                        "no version baseline for {} updates",
                        package.target.as_deref().unwrap_or("latest")
                    )
                })
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
                            changes::redact_urls(&dependency.requirement)
                        ),
                        false
                    )
                );
                reports.push(json!({"file":project.path.to_string_lossy(),"requirement":changes::redact_urls(&dependency.requirement),"section":dependency.section,"index":dependency.index,"latest":null,"selected":null,"prerelease":null,"reason":if local_names.contains(&name){"workspace"}else{"unsupported_requirement"},"detail":reason,"rejected":[],"locked":if args.show_locked {Some(locked::inspect(&root_directory,&name,&[]))}else{None}}));
                continue;
            }
            let effective_text = if let Some(bound) = bounds.get(&name) {
                matched_bounds.insert(name.clone());
                dependencies::replacement_bound(&original_req, &dependency.requirement, bound)?
            } else {
                dependency.requirement.clone()
            };
            let bound_changed = effective_text != dependency.requirement;
            let requirement = dependencies::parse_requirement(&effective_text)?;
            let exact = to
                .get(&name)
                .map(|value| value.trim_start_matches("==").parse::<Version>())
                .transpose()?;
            if exact.is_some() {
                matched_to.insert(name.clone());
            }
            let candidate_policy = CandidatePolicy {
                target: package.target.unwrap_or_else(|| "latest".to_owned()),
                cutoff: age,
                allow_prerelease: settings.pre.unwrap_or(false),
                minimal: settings.minimal.unwrap_or(false) && !bound_changed,
                exact,
            };
            let indexes =
                workspace::package_indexes(root, project, &name, &args.index, &dependency.section)?;
            let releases = client.get_candidates(&name, &indexes)?;
            check_updates::check_interrupt()?;
            if !args.explain
                && !releases.iter().any(|r| {
                    !r.yanked
                        && (!r.version.any_prerelease()
                            || candidate_policy.allow_prerelease && !r.version.is_dev())
                })
            {
                bail!("No stable, non-yanked release found for {name}.");
            }
            let decision =
                selection::evaluate(&requirement, &releases, &candidate_policy, python.as_ref());
            if candidate_policy.exact.is_some()
                && decision.selected.is_none()
                && decision.reason != Some("minimal")
            {
                bail!(
                    "Explicit target for {name} is unavailable, not newer, or rejected by an active policy. Use --explain without --to to inspect candidates."
                );
            }
            reports.push(json!({"file":project.path.to_string_lossy(),"requirement":dependency.requirement,"section":dependency.section,"index":dependency.index,"latest":decision.latest.as_ref().map(ToString::to_string),"selected":decision.selected.as_ref().map(ToString::to_string),"prerelease":decision.prerelease.as_ref().map(ToString::to_string),"reason":decision.reason,"rejected":decision.rejected.iter().map(|(v,reasons)|json!({"version":v.to_string(),"reasons":reasons})).collect::<Vec<_>>(),"locked":if args.show_locked {Some(locked::inspect(&root_directory,&name,&report_indexes(&indexes)))}else{None}}));
            let version = decision.selected.or_else(|| {
                bound_changed
                    .then(|| dependencies::baseline(&requirement).map(|s| s.version().clone()))
                    .flatten()
            });
            if let Some(version) = version {
                ensure!(
                    !bound_changed
                        || selection::requirement_contains(&requirement, &version)
                        || dependencies::baseline(&requirement)
                            .is_some_and(|s| *s.operator() == Operator::Equal),
                    "Replacement bound excludes the candidate or current baseline."
                );
                let replacement = if bound_changed
                    && settings.minimal.unwrap_or(false)
                    && selection::requirement_contains(&requirement, &version)
                {
                    effective_text
                } else {
                    dependencies::replacement(&requirement, &effective_text, &version)
                        .unwrap_or(effective_text)
                };
                if replacement == dependency.requirement {
                    continue;
                }
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
            }
        }
        if !updates.is_empty() {
            pending.push((project.clone(), updates));
        }
    }
    ensure!(
        to.keys().all(|n| matched_to.contains(n)),
        "An explicit target did not match a selected supported dependency."
    );
    ensure!(
        bounds.keys().all(|n| matched_bounds.contains(n)),
        "An explicit bound did not match a selected supported dependency."
    );
    let writing = args.upgrade || args.interactive;
    let locks = root_directory.join("uv.lock").is_file()
        || args.verify
        || args.doctor
        || !bounds.is_empty();
    if writing && !args.no_lock && policy_lock && locks {
        ensure!(
            !prerelease_conflict,
            "Different prerelease policies for one workspace cannot be mapped safely to uv. Use one workspace policy or --no-lock for requirement selection only."
        );
        ensure!(
            !age_conflict,
            "Different release-age policies for one workspace cannot be mapped safely to uv. Use one workspace policy or --no-lock for requirement selection only."
        );
    }
    resolver.package_cutoffs = ages
        .into_iter()
        .filter_map(|(n, v)| v.map(|v| (n, v)))
        .collect();
    Ok(Collected {
        root: root_directory,
        projects,
        pending,
        reports,
        resolver,
        policy_lock,
    })
}

fn report_indexes(indexes: &[check_updates::indexes::Index]) -> Vec<check_updates::indexes::Index> {
    let mut effective: Vec<_> = indexes.iter().filter(|i| !i.explicit).cloned().collect();
    if !indexes.iter().any(|i| i.default) {
        effective.push(check_updates::indexes::Index {
            url: "https://pypi.org/simple/".into(),
            ..Default::default()
        });
    }
    effective
}

fn show_reports(reports: &[Value], show_locked: bool) {
    println!("Dependency decisions (metadata eligibility is not runtime compatibility)");
    for row in reports {
        let text = |key: &str| row[key].as_str().unwrap_or("unknown");
        println!(
            "{}: {} [{}.{}]",
            changes::safe_text(text("file"), false),
            changes::safe_text(text("requirement"), false),
            changes::safe_text(
                &row["section"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("."),
                false
            ),
            row["index"]
        );
        println!(
            "  Latest stable: {}; selected: {}; prerelease: {}; reason: {}",
            text("latest"),
            text("selected"),
            text("prerelease"),
            text("reason")
        );
        if let Some(rejected) = row["rejected"].as_array() {
            for candidate in rejected {
                println!(
                    "  {}: {}",
                    candidate["version"].as_str().unwrap_or("unknown"),
                    candidate["reasons"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        if show_locked {
            println!("  Locked (freshness not checked): {}", row["locked"]);
        }
    }
}

fn apply_selection(
    args: &Args,
    result: &Collected,
    pending: &output::Pending,
    interactive: bool,
) -> Result<()> {
    let changes: Vec<_> = pending
        .iter()
        .map(|(p, u)| changes::prepare_change(p, u))
        .collect::<Result<_>>()?;
    let needs_verify = args.verify
        || !args.bound.is_empty()
        || result.policy_lock && !args.no_lock && result.root.join("uv.lock").is_file();
    if needs_verify || args.doctor {
        ensure!(
            args.index.is_empty(),
            "Verified writes require project index configuration; --index only controls metadata lookup. Configure tool.uv.index before retrying."
        );
    }
    if args.doctor {
        return doctor::run(
            &result.root,
            &result.projects,
            pending,
            &result.resolver,
            &args.test_command,
        );
    }
    let verified = if needs_verify {
        Some(verification::preflight(
            &result.root,
            &result.projects,
            pending,
            &result.resolver,
        )?)
    } else {
        None
    };
    if interactive {
        for change in &changes {
            print!("{}", changes::preview_diff(change));
        }
        for (_, updates) in pending {
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
            changes.iter().map(|c| c.count).sum::<usize>(),
            changes.len()
        );
        if verified.is_some() {
            println!(
                "Selected versions resolved successfully; runtime compatibility is not tested."
            );
        }
        if !interactive::confirm_changes()? {
            println!("Cancelled. No files were written.");
            return Ok(());
        }
    }
    if !args.bound.is_empty() && !interactive {
        for change in &changes {
            print!("{}", changes::preview_diff(change));
        }
    }
    if let Some(verified) = verified {
        verified.apply(&changes)?;
    } else {
        changes::write_changes(&changes)?;
        if !args.no_lock {
            locking::lock_project(&result.root)?;
        }
    }
    if result.policy_lock && (args.no_lock || !result.root.join("uv.lock").is_file()) {
        eprintln!(
            "Release policy filtered requirement selection only. The lockfile and transitive dependencies were not verified."
        );
    }
    Ok(())
}

fn run(args: &Args) -> Result<i32> {
    let result = collect(args)?;
    check_updates::check_interrupt()?;
    if args.format == "json" {
        let mut value = output::json_updates(&result.pending);
        if args.explain || args.show_locked {
            value["schema_version"] = json!(2);
            value["decisions"] = json!(result.reports);
        }
        println!("{value}");
    } else if args.diff {
        for (project, updates) in &result.pending {
            print!(
                "{}",
                changes::preview_diff(&changes::prepare_change(project, updates)?)
            );
        }
    } else if args.explain || args.show_locked {
        show_reports(&result.reports, args.show_locked);
    } else if result.pending.is_empty() {
        println!("No dependency updates available.");
    } else if args.interactive {
        let selected = interactive::select_updates(&result.pending)?;
        if selected.is_empty() {
            println!("No updates selected. No files were written.");
            return Ok(0);
        }
        apply_selection(args, &result, &selected, true)?;
    } else {
        output::show_updates(&result.pending, args.upgrade, args.workspace);
        if args.upgrade {
            apply_selection(args, &result, &result.pending, false)?;
        }
    }
    Ok(if args.check && !result.pending.is_empty() {
        3
    } else {
        0
    })
}

fn main() {
    let args = Args::parse();
    if named_constraints(&args.to, false).is_err() || named_constraints(&args.bound, true).is_err()
    {
        invalid(
            "Invalid or duplicate explicit package constraint. Use --to 'name==VERSION' or --bound 'name<VERSION'.",
        );
    }
    if cli_settings(&args).validate().is_err() {
        invalid("Invalid package glob.");
    }
    if args.no_lock && !(args.upgrade || args.interactive) {
        invalid("--no-lock requires --upgrade (-u) or --interactive (-i).");
    }
    if args.verify && !(args.upgrade || args.interactive) {
        invalid("--verify requires --upgrade (-u) or --interactive (-i).");
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
                changes::safe_text(&changes::redact_urls(&format!("{error:#}")), false)
            );
            debug(
                args.debug,
                "Command failed; Rust error context is shown above.",
            );
            std::process::exit(if interrupted { 130 } else { 1 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use check_updates::indexes::Index;

    #[test]
    fn locked_reporting_uses_the_same_implicit_public_fallback_as_lookup() {
        assert_eq!(report_indexes(&[])[0].url, "https://pypi.org/simple/");
        let configured = Index {
            url: "https://example.org/simple".into(),
            ..Default::default()
        };
        assert_eq!(report_indexes(std::slice::from_ref(&configured)).len(), 2);
        let default = Index {
            default: true,
            ..configured.clone()
        };
        assert_eq!(report_indexes(&[default]).len(), 1);
        let explicit_default = Index {
            default: true,
            explicit: true,
            ..configured
        };
        assert!(report_indexes(&[explicit_default]).is_empty());
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("uv.lock"), "version=1\nrevision=5\n[[package]]\nname='demo'\nversion='2.0'\nsource={registry='https://pypi.org/simple'}\n").unwrap();
        let report = locked::inspect(temp.path(), "demo", &report_indexes(&[]));
        assert_eq!(report["state"], "unverified");
        assert_eq!(report["versions"][0]["version"], "2.0");
    }
}
