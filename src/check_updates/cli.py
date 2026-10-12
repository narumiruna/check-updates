"""List or apply dependency updates in a project or uv workspace."""

import logging
import sys
from typing import Annotated
from urllib.error import URLError

import typer
from packaging.requirements import Requirement
from packaging.utils import canonicalize_name
from packaging.version import Version
from tomlkit.exceptions import ParseError

from check_updates.changes import Change
from check_updates.changes import check_snapshots
from check_updates.changes import prepare_change
from check_updates.changes import preview_diff
from check_updates.changes import write_change
from check_updates.dependencies import Update
from check_updates.dependencies import dependencies
from check_updates.dependencies import latest_version
from check_updates.dependencies import replacement
from check_updates.dependencies import unsupported_reason
from check_updates.diagnostics import debug_logging
from check_updates.interactive import confirm_changes
from check_updates.interactive import select_updates
from check_updates.output import show_updates
from check_updates.workspace import Project
from check_updates.workspace import load_projects
from check_updates.workspace import workspace_dependencies

app = typer.Typer(add_completion=False)
logger = logging.getLogger(__name__)


def _check_project(
    project: Project, local_names: set[str], cache: dict[str, Version], *, workspace: bool
) -> list[Update]:
    logger.debug("Checking project %s; workspace dependencies: %s", project.path, sorted(local_names))
    updates = []
    for dependency in dependencies(project.document):
        requirement = Requirement(dependency.requirement)
        name = canonicalize_name(requirement.name)
        logger.debug("Checking %s in %s[%d]", name, ".".join(dependency.section), dependency.index)
        reason = "workspace dependency" if name in local_names else unsupported_reason(requirement)
        if reason:
            logger.debug("Skipping %s: %s", name, reason)
            prefix = f"{project.path}: " if workspace else ""
            hint = " Use -w to also check workspace members." if name in local_names and not workspace else ""
            typer.echo(f"{prefix}Skipped {dependency.requirement}: {reason}.{hint}", err=True)
            continue
        if name not in cache:
            logger.debug("PyPI cache miss for %s; fetching latest stable release", name)
            cache[name] = latest_version(name)
        else:
            logger.debug("PyPI cache hit for %s: %s", name, cache[name])
        new_requirement = replacement(requirement, dependency.requirement, cache[name])
        if new_requirement is not None:
            logger.debug(
                "Update available for %s: %s -> %s", name, str(requirement.specifier) or "unbounded", cache[name]
            )
            updates.append(Update(dependency, new_requirement, cache[name]))
        else:
            logger.debug("No update for %s: constraint %s, latest %s", name, requirement.specifier, cache[name])
    logger.debug("Found %d updates in %s", len(updates), project.path)
    return updates


def _collect_updates(workspace: bool) -> list[tuple[Project, list[Update]]]:
    projects = load_projects(workspace)
    logger.debug("Loaded %d project files", len(projects))
    pending = []
    cache: dict[str, Version] = {}
    for project in projects:
        local_names = workspace_dependencies(projects[0], project)
        updates = _check_project(project, local_names, cache, workspace=workspace)
        if updates:
            pending.append((project, updates))
    logger.debug(
        "Check complete: %d updates in %d files; %d cached packages",
        sum(len(u) for _, u in pending),
        len(pending),
        len(cache),
    )
    return pending


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
    debug: Annotated[
        bool, typer.Option("--debug", help="Write detailed diagnostics and error tracebacks to stderr.")
    ] = False,
) -> None:
    """Check stable PyPI releases without running lock or sync commands."""
    with debug_logging(debug):
        _run(upgrade=upgrade, workspace=workspace, interactive=interactive)


def _run(*, upgrade: bool, workspace: bool, interactive: bool) -> None:
    logger.debug("Starting check: upgrade=%s, workspace=%s, interactive=%s", upgrade, workspace, interactive)
    if interactive and upgrade:
        raise typer.BadParameter("--interactive (-i) cannot be combined with --upgrade (-u).")
    if interactive and not (sys.stdin.isatty() and sys.stdout.isatty()):
        raise typer.BadParameter("--interactive (-i) requires terminal input and output. Use -u for automatic updates.")
    try:
        pending = _collect_updates(workspace)
        if not pending:
            typer.echo("No dependency updates available.")
        elif interactive:
            _write_changes(_interactive_changes(pending))
        else:
            logger.debug("Displaying available updates")
            show_updates(pending, upgrade=upgrade, workspace=workspace)
            if upgrade:
                _write_changes([prepare_change(project, updates) for project, updates in pending])
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
