"""List or apply dependency updates in a project or uv workspace."""

import json
import logging
import sys
from typing import Annotated
from urllib.error import URLError

import typer
from packaging.requirements import Requirement
from packaging.utils import canonicalize_name
from tomlkit.exceptions import ParseError

from check_updates.changes import Change
from check_updates.changes import check_snapshots
from check_updates.changes import prepare_change
from check_updates.changes import preview_diff
from check_updates.changes import write_change
from check_updates.dependencies import Update
from check_updates.dependencies import baseline
from check_updates.dependencies import dependencies
from check_updates.dependencies import replacement
from check_updates.dependencies import unsupported_reason
from check_updates.dependencies import version_change
from check_updates.diagnostics import debug_logging
from check_updates.indexes import ReleaseClient
from check_updates.interactive import confirm_changes
from check_updates.interactive import select_updates
from check_updates.locking import lock_project
from check_updates.output import show_updates
from check_updates.selection import Selection
from check_updates.selection import choose_version
from check_updates.selection import python_baseline
from check_updates.selection import release_url
from check_updates.workspace import Project
from check_updates.workspace import load_projects
from check_updates.workspace import package_indexes
from check_updates.workspace import workspace_dependencies

app = typer.Typer(add_completion=False)
logger = logging.getLogger(__name__)


def _check_project(
    project: Project,
    root: Project,
    local_names: set[str],
    client: ReleaseClient,
    selection: Selection,
    urls: list[str],
    *,
    workspace: bool,
) -> list[Update]:
    logger.debug("Checking project %s; workspace dependencies: %s", project.path, sorted(local_names))
    updates = []
    python = python_baseline(project.document.get("project", {}).get("requires-python"))
    for dependency in dependencies(project.document):
        requirement = Requirement(dependency.requirement)
        name = canonicalize_name(requirement.name)
        if not selection.accepts(dependency, requirement):
            continue
        logger.debug("Checking %s in %s[%d]", name, ".".join(dependency.section), dependency.index)
        reason = "workspace dependency" if name in local_names else unsupported_reason(requirement)
        if reason is None and selection.target != "latest" and baseline(requirement) is None:
            reason = f"no version baseline for {selection.target} updates"
        if reason:
            logger.debug("Skipping %s: %s", name, reason)
            prefix = f"{project.path}: " if workspace else ""
            hint = " Use -w to also check workspace members." if name in local_names and not workspace else ""
            typer.echo(f"{prefix}Skipped {dependency.requirement}: {reason}.{hint}", err=True)
            continue
        indexes = package_indexes(root, project, name, urls, section=dependency.section)
        releases = client.get(name, indexes)
        if not releases:
            msg = f"No stable, non-yanked release found for {name}."
            raise ValueError(msg)
        version = choose_version(requirement, releases, selection.target, python)
        new_requirement = replacement(requirement, dependency.requirement, version) if version is not None else None
        if new_requirement is not None:
            logger.debug("Update available for %s: %s -> %s", name, str(requirement.specifier) or "unbounded", version)
            assert version is not None
            link = client.links.get((name, indexes))
            if link == f"https://pypi.org/project/{name}/":
                link = release_url(name, version)
            public = link is not None and link.startswith(f"https://pypi.org/project/{name}/")
            updates.append(
                Update(dependency, new_requirement, version, link, client.notes.get(name) if public else None)
            )
        else:
            logger.debug("No update for %s: constraint %s", name, requirement.specifier)
    logger.debug("Found %d updates in %s", len(updates), project.path)
    return updates


def _collect_updates(
    workspace: bool, selection: Selection, urls: list[str], *, no_cache: bool
) -> tuple[Project, list[tuple[Project, list[Update]]]]:
    projects = load_projects(workspace)
    logger.debug("Loaded %d project files", len(projects))
    pending = []
    client = ReleaseClient(no_cache=no_cache)
    for project in projects:
        local_names = workspace_dependencies(projects[0], project)
        updates = _check_project(project, projects[0], local_names, client, selection, urls, workspace=workspace)
        if updates:
            pending.append((project, updates))
    logger.debug(
        "Check complete: %d updates in %d files; %d cached packages",
        sum(len(u) for _, u in pending),
        len(pending),
        len(client.cache),
    )
    return projects[0], pending


def _interactive_changes(pending: list[tuple[Project, list[Update]]]) -> list[Change]:
    logger.debug("Opening interactive update selector")
    selected = select_updates(pending)
    logger.debug("Selected %d updates in %d files", sum(len(u) for _, u in selected), len(selected))
    if not selected:
        typer.echo("No updates selected. No files were written.")
        return []
    changes = [prepare_change(project, updates) for project, updates in selected]
    for change in changes:
        typer.echo(preview_diff(change), nl=False)
    for _project, updates in selected:
        for update in updates:
            if update.release_link:
                typer.echo(f"Release: {update.release_link}")
            if update.project_link:
                typer.echo(f"Release notes: {update.project_link}")
    typer.echo(f"Selected {sum(change.count for change in changes)} dependencies in {len(changes)} files.")
    if not confirm_changes():
        logger.debug("Write confirmation declined")
        typer.echo("Cancelled. No files were written.")
        return []
    logger.debug("Write confirmation accepted; checking %d file snapshots", len(changes))
    check_snapshots(changes)
    logger.debug("File snapshots match")
    return changes


def _write_changes(changes: list[Change]) -> None:
    for change in changes:
        logger.debug("Writing %d updates to %s", change.count, change.project.path)
        write_change(change)
        logger.debug("Write completed for %s", change.project.path)
        typer.echo(f"Updated {change.count} dependencies in {change.project.path}.")


@app.command()
def main(
    upgrade: Annotated[
        bool, typer.Option("--upgrade", "-u", help="Write dependency updates to pyproject.toml.")
    ] = False,
    workspace: Annotated[
        bool, typer.Option("--workspace", "-w", help="Check the uv workspace root and all included members.")
    ] = False,
    interactive: Annotated[
        bool, typer.Option("--interactive", "-i", help="Select updates, preview changes, and confirm before writing.")
    ] = False,
    no_lock: Annotated[
        bool, typer.Option("--no-lock", help="Skip automatic uv lock after -u or -i writes updates.")
    ] = False,
    target: Annotated[str, typer.Option(help="Update limit: patch, minor, or latest.")] = "latest",
    include: Annotated[list[str] | None, typer.Option(help="Include package (repeatable).")] = None,
    exclude: Annotated[list[str] | None, typer.Option(help="Exclude package (repeatable).")] = None,
    group: Annotated[list[str] | None, typer.Option(help="Select dependency group (repeatable).")] = None,
    extra: Annotated[list[str] | None, typer.Option(help="Select optional extra (repeatable).")] = None,
    runtime: Annotated[bool, typer.Option("--runtime", help="Select project.dependencies.")] = False,
    output_format: Annotated[str, typer.Option("--format", help="Output: table or json (read-only).")] = "table",
    check: Annotated[bool, typer.Option("--check", help="Read-only; exit 3 when updates exist.")] = False,
    diff: Annotated[bool, typer.Option("--diff", help="Show a read-only unified diff.")] = False,
    index: Annotated[list[str] | None, typer.Option(help="Simple index URL (repeatable, first-match order).")] = None,
    no_cache: Annotated[bool, typer.Option("--no-cache", help="Bypass the one-hour persistent PyPI cache.")] = False,
    debug: Annotated[
        bool, typer.Option("--debug", help="Write detailed diagnostics and error tracebacks to stderr.")
    ] = False,
) -> None:
    """Check stable releases; lock existing uv projects after updates, without installing packages."""
    with debug_logging(debug):
        if no_lock and not (upgrade or interactive):
            raise typer.BadParameter("--no-lock requires --upgrade (-u) or --interactive (-i).")
        if target not in {"patch", "minor", "latest"}:
            raise typer.BadParameter("--target must be patch, minor, or latest.")
        if output_format not in {"table", "json"}:
            raise typer.BadParameter("--format must be table or json.")
        if (check or diff or output_format == "json") and (upgrade or interactive):
            raise typer.BadParameter("--check, --diff, and --format json cannot be combined with -u or -i.")
        if diff and output_format == "json":
            raise typer.BadParameter("--diff cannot be combined with --format json.")
        selection = Selection(
            target,
            {canonicalize_name(n) for n in include or []},
            {canonicalize_name(n) for n in exclude or []},
            set(group or []),
            set(extra or []),
            runtime,
        )
        _run(
            upgrade=upgrade,
            workspace=workspace,
            interactive=interactive,
            selection=selection,
            urls=index or [],
            no_cache=no_cache,
            no_lock=no_lock,
            output_format=output_format,
            check=check,
            diff=diff,
        )


def _display_updates(
    pending: list[tuple[Project, list[Update]]],
    *,
    output_format: str,
    diff: bool,
    interactive: bool,
    upgrade: bool,
    workspace: bool,
) -> bool:
    """Display updates and report whether a nonempty set of writes completed."""
    if output_format == "json":
        typer.echo(
            json.dumps(
                {
                    "schema_version": 1,
                    "updates": [
                        {
                            "file": str(project.path),
                            "requirement": u.dependency.requirement,
                            "replacement": u.replacement,
                            "version": str(u.version),
                            "section": list(u.dependency.section),
                            "index": u.dependency.index,
                            "change": version_change(u),
                            "release_url": u.release_link,
                            "project_release_url": u.project_link,
                        }
                        for project, updates in pending
                        for u in updates
                    ],
                },
                ensure_ascii=True,
            )
        )
    elif diff:
        for project, updates in pending:
            typer.echo(preview_diff(prepare_change(project, updates)), nl=False)
    elif not pending:
        typer.echo("No dependency updates available.")
    elif interactive:
        changes = _interactive_changes(pending)
        _write_changes(changes)
        return bool(changes)
    else:
        logger.debug("Displaying available updates")
        show_updates(pending, upgrade=upgrade, workspace=workspace)
        if upgrade:
            changes = [prepare_change(project, updates) for project, updates in pending]
            check_snapshots(changes)
            _write_changes(changes)
            return bool(changes)
    return False


def _run(
    *,
    upgrade: bool,
    workspace: bool,
    interactive: bool,
    selection: Selection,
    urls: list[str],
    no_cache: bool,
    no_lock: bool,
    output_format: str,
    check: bool,
    diff: bool,
) -> None:
    logger.debug("Starting check: upgrade=%s, workspace=%s, interactive=%s", upgrade, workspace, interactive)
    if interactive and upgrade:
        raise typer.BadParameter("--interactive (-i) cannot be combined with --upgrade (-u).")
    if interactive and not (sys.stdin.isatty() and sys.stdout.isatty()):
        raise typer.BadParameter("--interactive (-i) requires terminal input and output. Use -u for automatic updates.")
    try:
        root, pending = _collect_updates(workspace, selection, urls, no_cache=no_cache)
        written = _display_updates(
            pending,
            output_format=output_format,
            diff=diff,
            interactive=interactive,
            upgrade=upgrade,
            workspace=workspace,
        )
        if written and not no_lock:
            lock_project(root.path.absolute().parent)
        if check and pending:
            raise typer.Exit(code=3)
        logger.debug("Command completed")
    except KeyboardInterrupt as error:
        logger.debug("Command interrupted", exc_info=True)
        typer.echo("Interrupted.", err=True)
        raise typer.Exit(code=130) from error
    except EOFError:
        logger.debug("Input closed; cancelling", exc_info=True)
        typer.echo("Cancelled. No files were written.")
    except (OSError, URLError, ValueError, TypeError, ParseError) as error:
        logger.debug("Command failed", exc_info=True)
        typer.echo(f"Error: {error}", err=True)
        raise typer.Exit(code=1) from error
