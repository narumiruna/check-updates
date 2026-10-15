"""Run uv lock after requirement files have been written successfully."""

import logging
import subprocess
from collections.abc import Mapping
from pathlib import Path

import typer

from check_updates.workspace import read_project

logger = logging.getLogger(__name__)


def _parent_workspace(directory: Path) -> Path | None:
    """Stop at a local workspace; otherwise guard against parent discovery."""
    directory = directory.resolve()
    for candidate in (directory, *directory.parents):
        path = candidate / "pyproject.toml"
        if not path.is_file():
            continue
        tool = read_project(path).document.get("tool", {})
        uv = tool.get("uv", {}) if isinstance(tool, Mapping) else {}
        if isinstance(uv, Mapping) and "workspace" in uv:
            return None if candidate == directory else candidate
    return None


def lock_project(directory: Path) -> None:
    """Lock an existing uv project without installing or upgrading all packages."""
    recovery = (
        f"Requirement files were updated. uv.lock may be stale or changed. "
        f"Resolve the error and run uv lock in {directory}, or review and restore the affected files."
    )
    try:
        if not (directory / "uv.lock").is_file():
            logger.debug("Skipping uv lock: no regular uv.lock in %s", directory)
            return
        parent = _parent_workspace(directory)
        if parent is not None:
            typer.echo(
                f"Skipping uv lock: parent workspace found at {parent}. "
                "Requirement files were updated; lockfiles were not changed. "
                "Use -w to update and lock the workspace, or review and lock manually.",
                err=True,
            )
            return
        typer.echo(f"Running uv lock in {directory}...")
        logger.debug("Running uv lock in %s", directory)
        # Inherit output and environment; closed input prevents subprocess prompts.
        # run() waits for the child and cleans it up if interrupted.
        subprocess.run(["uv", "lock"], cwd=directory, stdin=subprocess.DEVNULL, check=True)
    except KeyboardInterrupt as error:
        logger.debug("uv lock interrupted after writes", exc_info=True)
        typer.echo(f"Interrupted during uv lock; lock completion is not confirmed. {recovery}", err=True)
        raise typer.Exit(code=130) from error
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        logger.debug("uv lock failed after writes", exc_info=True)
        typer.echo(f"Error: uv lock failed: {error}. {recovery}", err=True)
        raise typer.Exit(code=1) from error
    else:
        typer.echo("uv lock completed.")
        logger.debug("uv lock completed")
