"""List or apply dependency updates in a project or uv workspace."""

from typing import Annotated
from urllib.error import URLError

import tomlkit
import typer
from packaging.requirements import Requirement
from packaging.utils import canonicalize_name
from packaging.version import Version
from tomlkit.exceptions import ParseError

from check_updates.dependencies import Update
from check_updates.dependencies import apply_update
from check_updates.dependencies import dependencies
from check_updates.dependencies import latest_version
from check_updates.dependencies import replacement
from check_updates.dependencies import unsupported_reason
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


def _write_updates(project: Project, updates: list[Update]) -> None:
    for update in updates:
        apply_update(update)
    project.path.write_bytes(tomlkit.dumps(project.document).encode("utf-8"))
    typer.echo(f"Updated {len(updates)} dependencies in {project.path}.")


@app.command()
def main(
    upgrade: Annotated[
        bool, typer.Option("--upgrade", "-u", help="Write dependency updates to pyproject.toml.")
    ] = False,
    workspace: Annotated[
        bool, typer.Option("--workspace", "-w", help="Check the uv workspace root and all included members.")
    ] = False,
) -> None:
    """Check stable PyPI releases without running lock or sync commands."""
    try:
        projects = load_projects(workspace)
        pending: list[tuple[Project, list[Update]]] = []
        cache: dict[str, Version] = {}
        for project in projects:
            local_names = workspace_dependencies(projects[0], project) if workspace else set()
            updates = _check_project(project, local_names, cache, workspace=workspace)
            if updates:
                pending.append((project, updates))
        for project, updates in pending:
            if workspace:
                typer.echo(f"{project.path}:")
            for update in updates:
                typer.echo(f'"{update.dependency.requirement}" -> {update.version}')
        if not pending:
            typer.echo("No dependency updates available.")
        elif upgrade:
            for project, updates in pending:
                _write_updates(project, updates)
    except (OSError, URLError, ValueError, TypeError, ParseError) as error:
        typer.echo(f"Error: {error}", err=True)
        raise typer.Exit(code=1) from error
