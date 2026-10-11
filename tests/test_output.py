import io
from pathlib import Path

import pytest
import tomlkit
from packaging.version import Version
from rich.console import Console

from check_updates import output
from check_updates.dependencies import Update
from check_updates.dependencies import dependencies
from check_updates.workspace import Project


def render(monkeypatch, requirements, *, terminal=False, width=120, upgrade=False):
    document = tomlkit.parse(f"[project]\ndependencies = {requirements!r}\n")
    project = Project(Path("pyproject.toml"), document, b"")
    updates = [Update(dependency, "unused", Version("2.3.4")) for dependency in dependencies(document)]
    stream = io.StringIO()
    monkeypatch.setattr(
        output, "Console", lambda **kwargs: Console(file=stream, width=width, force_terminal=terminal, **kwargs)
    )
    output.show_updates([(project, updates)], upgrade=upgrade, workspace=False)
    return stream.getvalue()


@pytest.mark.parametrize(
    ("requirement", "label"),
    [("demo>=1.0", "major"), ("demo~=2.2", "minor"), ("demo==2.3.3", "patch"), ("demo", "unbounded")],
)
def test_table_labels(monkeypatch, requirement, label):
    text = render(monkeypatch, [requirement])
    assert requirement in text
    assert label in text
    assert "2.3.4" in text
    assert "project.dependencies" in text
    assert "\x1b[" not in text


def test_terminal_has_color(monkeypatch):
    assert "\x1b[" in render(monkeypatch, ["demo>=1.0"], terminal=True)


def test_extras_and_markers_are_literal(monkeypatch):
    requirement = 'demo[red]>=1.0; python_version < "3.14"'
    text = render(monkeypatch, [requirement], width=160)
    assert requirement in text
    assert "[/" not in text


def test_narrow_output_retains_long_requirement(monkeypatch):
    requirement = "very-long-dependency-name[extra]>=1.0"
    text = render(monkeypatch, [requirement], width=60)
    assert "…" not in text
    # Ignore borders and wrapping whitespace when checking that no text was lost.
    compact = "".join(line.split("│")[1].strip() for line in text.splitlines() if line.startswith("│"))
    assert requirement in compact


def test_upgrade_does_not_claim_no_files_changed(monkeypatch):
    text = render(monkeypatch, ["demo>=1.0"], upgrade=True)
    assert "No files changed" not in text
    assert "Use -u" not in text


def test_control_characters_are_escaped():
    assert output._text("path\x1b[31m\n").plain == r"path\x1b[31m\n"
