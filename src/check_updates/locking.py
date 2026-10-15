"""Run uv lock after requirement files have been written successfully."""

import logging
import subprocess
from pathlib import Path

import typer

logger = logging.getLogger(__name__)


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
        typer.echo(f"Running uv lock in {directory}...")
        logger.debug("Running uv lock in %s", directory)
        # Inherit output and environment; closed input prevents subprocess prompts.
        # run() waits for the child and cleans it up if interrupted.
        subprocess.run(["uv", "lock"], cwd=directory, stdin=subprocess.DEVNULL, check=True)
    except KeyboardInterrupt as error:
        logger.debug("uv lock interrupted after writes", exc_info=True)
        typer.echo(f"Interrupted during uv lock; lock completion is not confirmed. {recovery}", err=True)
        raise typer.Exit(code=130) from error
    except (OSError, subprocess.CalledProcessError) as error:
        logger.debug("uv lock failed after writes", exc_info=True)
        typer.echo(f"Error: uv lock failed: {error}. {recovery}", err=True)
        raise typer.Exit(code=1) from error
    else:
        typer.echo("uv lock completed.")
        logger.debug("uv lock completed")
