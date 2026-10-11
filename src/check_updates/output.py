"""Render dependency update tables and listing summaries."""

from rich import box
from rich.console import Console
from rich.table import Table
from rich.text import Text

from check_updates.dependencies import Update
from check_updates.dependencies import version_change
from check_updates.workspace import Project

_CHANGE_STYLES = {"major": "bold red", "minor": "yellow", "patch": "green", "unbounded": "cyan", "other": "magenta"}


def _text(value: str, style: str = "") -> Text:
    # Keep requirements and paths literal, including markup and control characters.
    safe = "".join(character if character.isprintable() else repr(character)[1:-1] for character in value)
    return Text(safe, style=style)


def show_updates(pending: list[tuple[Project, list[Update]]], *, upgrade: bool, workspace: bool) -> None:
    """Use terminal colors automatically; retain readable tables when redirected."""
    console = Console(highlight=False)
    console.print("Dependency updates", style="bold cyan")
    console.print()
    for project, updates in pending:
        console.print(_text(f"{project.path}:", "bold"))
        table = Table(box=box.ROUNDED, show_edge=True, padding=(0, 1))
        table.add_column("Current requirement", overflow="fold")
        table.add_column("Target", style="bold green", no_wrap=True)
        table.add_column("Change", no_wrap=True)
        table.add_column("Section", style="dim", overflow="fold")
        for update in updates:
            label = version_change(update)
            table.add_row(
                _text(update.dependency.requirement),
                _text(str(update.version)),
                Text(label, style=_CHANGE_STYLES[label]),
                _text(".".join(update.dependency.section)),
            )
        console.print(table)
        console.print()
    count = sum(len(updates) for _, updates in pending)
    dependencies = "dependency" if count == 1 else "dependencies"
    files = "file" if len(pending) == 1 else "files"
    console.print(f"{count} {dependencies} with updates in {len(pending)} {files}.", style="bold")
    console.print("Change labels compare requirement versions, not installed versions or compatibility.", style="dim")
    if not upgrade:
        suffix = " -w" if workspace else ""
        console.print(
            f"No files changed. Use -u{suffix} to apply all updates or -i{suffix} to select updates.", style="cyan"
        )
