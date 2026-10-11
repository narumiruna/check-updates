"""List or apply dependency updates in a project or uv workspace."""

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
from check_updates.interactive import confirm_changes
from check_updates.interactive import select_updates
from check_updates.output import show_updates
from check_updates.workspace import Project
from check_updates.workspace import load_projects
from check_updates.workspace import workspace_dependencies

app = typer.Typer(add_completion=False)


def _check_project(
    project: Project, local_names: set[str], cache: dict[str, Version], *, workspace: bool
) -> list[Update]:
    updates = []
    for dependency in dependencies(project.document):
        requirement = Requirement(dependency.requirement)
        name = canonicalize_name(requirement.name)
        reason = "workspace dependency" if name in local_names else unsupported_reason(requirement)
        if reason:
            prefix = f"{project.path}: " if workspace else ""
            typer.echo(f"{prefix}Skipped {dependency.requirement}: {reason}.", err=True)
            continue
        if name not in cache:
            cache[name] = latest_version(name)
        new_requirement = replacement(requirement, dependency.requirement, cache[name])
        if new_requirement is not None:
            updates.append(Update(dependency, new_requirement, cache[name]))
    return updates


def _collect_updates(workspace: bool) -> list[tuple[Project, list[Update]]]:
    projects = load_projects(workspace)
    pending = []
    cache: dict[str, Version] = {}
    for project in projects:
        local_names = workspace_dependencies(projects[0], project) if workspace else set()
        updates = _check_project(project, local_names, cache, workspace=workspace)
        if updates:
            pending.append((project, updates))
    return pending


def _interactive_changes(pending: list[tuple[Project, list[Update]]]) -> list[Change]:
    selected = select_updates(pending)
    if not selected:
        typer.echo("No updates selected. No files were written.")
        return []
    changes = [prepare_change(project, updates) for project, updates in selected]
    for change in changes:
        typer.echo(preview_diff(change), nl=False)
    typer.echo(f"Selected {sum(change.count for change in changes)} dependencies in {len(changes)} files.")
    if not confirm_changes():
        typer.echo("Cancelled. No files were written.")
        return []
    check_snapshots(changes)
    return changes


def _write_changes(changes: list[Change]) -> None:
    for change in changes:
        write_change(change)
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
) -> None:
    """Check stable PyPI releases without running lock or sync commands."""
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
            show_updates(pending, upgrade=upgrade, workspace=workspace)
            if upgrade:
                _write_changes([prepare_change(project, updates) for project, updates in pending])
    except KeyboardInterrupt as error:
        typer.echo("Interrupted.", err=True)
        raise typer.Exit(code=130) from error
    except EOFError:
        typer.echo("Cancelled. No files were written.")
    except (OSError, URLError, ValueError, TypeError, ParseError) as error:
        typer.echo(f"Error: {error}", err=True)
        raise typer.Exit(code=1) from error
