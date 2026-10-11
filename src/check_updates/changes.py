"""Prepare exact update bytes, display diffs, and check files before writing."""

from dataclasses import dataclass
from difflib import unified_diff

import tomlkit

from check_updates.dependencies import Update
from check_updates.dependencies import apply_update
from check_updates.dependencies import dependencies
from check_updates.workspace import Project


@dataclass
class Change:
    project: Project
    content: bytes
    count: int


def prepare_change(project: Project, updates: list[Update]) -> Change:
    """Apply selected updates to a fresh parse without changing the loaded document."""
    document = tomlkit.parse(project.original.decode("utf-8"))
    locations = {(item.section, item.index): item for item in dependencies(document)}
    for update in updates:
        target = locations[update.dependency.section, update.dependency.index]
        apply_update(Update(target, update.replacement, update.version))
    return Change(project, tomlkit.dumps(document).encode("utf-8"), len(updates))


def preview_diff(change: Change) -> str:
    """Display a unified diff, including missing final-newline markers."""
    lines = unified_diff(
        change.project.original.decode("utf-8").splitlines(keepends=True),
        change.content.decode("utf-8").splitlines(keepends=True),
        fromfile=str(change.project.path),
        tofile=str(change.project.path),
    )
    text = "".join(line if line.endswith("\n") else f"{line}\n\\ No newline at end of file\n" for line in lines)
    # Display file content as text, not terminal commands. Written bytes stay unchanged.
    return "".join(
        character if character.isprintable() or character in "\n\t" else repr(character)[1:-1]
        for character in text.replace("\r\n", "\n")
    )


def check_snapshots(changes: list[Change]) -> None:
    """Check all affected files before the first write, without overwriting edits."""
    for change in changes:
        path = change.project.path
        try:
            current = path.read_bytes()
        except OSError as error:
            msg = f"Cannot read {path} before writing: {error}"
            raise OSError(msg) from error
        if current != change.project.original:
            msg = f"{path} changed during review. No files were written; run the command again."
            raise ValueError(msg)


def write_change(change: Change) -> None:
    try:
        change.project.path.write_bytes(change.content)
    except OSError as error:
        msg = f"Cannot write {change.project.path}: {error}"
        raise OSError(msg) from error
