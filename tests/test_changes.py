import pytest
import tomlkit
from packaging.version import Version

from check_updates.changes import Change
from check_updates.changes import check_snapshots
from check_updates.changes import prepare_change
from check_updates.changes import preview_diff
from check_updates.dependencies import Update
from check_updates.dependencies import dependencies
from check_updates.workspace import Project
from check_updates.workspace import read_project


@pytest.mark.parametrize("newline", [b"\n", b"\r\n"])
def test_prepare_selected_occurrence_preserves_bytes_and_document(tmp_path, newline):
    original = newline.join(
        [
            b"[project]",
            b'dependencies = ["typer>=1.0", \'typer[all]>=1.0; python_version < "3.14"\'] # Keep comment.',
            b"[project.optional-dependencies]",
            b'cli = ["typer>=1.0"]',
            b"[dependency-groups]",
            b'dev = [{include-group = "other"}, "typer>=1.0"]',
            b"",
        ]
    )
    path = tmp_path / "pyproject.toml"
    path.write_bytes(original)
    project = read_project(path)
    entries = list(dependencies(project.document))
    assert len({(entry.section, entry.index) for entry in entries}) == 4
    selected = entries[1]
    replacement = selected.requirement.replace("1.0", "2.0")
    change = prepare_change(project, [Update(selected, replacement, Version("2.0"))])
    assert change.content == original.replace(b"typer[all]>=1.0", b"typer[all]>=2.0")
    assert change.count == 1
    assert project.original == original
    assert tomlkit.dumps(project.document).encode() == original
    assert path.read_bytes() == original
    diff = preview_diff(change)
    assert "-dependencies" in diff
    assert "+dependencies" in diff
    assert "typer[all]>=2.0" in diff
    assert 'cli = ["typer>=2.0"]' not in diff


@pytest.mark.parametrize("quote", ['"', "'", '"""', "'''"])
def test_prepare_keeps_quote_style(tmp_path, quote):
    path = tmp_path / "pyproject.toml"
    original = f"[project]\ndependencies = [{quote}typer>=1.0{quote}]\n".encode()
    path.write_bytes(original)
    project = read_project(path)
    dependency = next(dependencies(project.document))
    change = prepare_change(project, [Update(dependency, "typer>=2.0", Version("2.0"))])
    assert change.content == original.replace(b"1.0", b"2.0")
    assert path.read_bytes() == original


def test_diff_with_no_final_newline(tmp_path):
    path = tmp_path / "pyproject.toml"
    original = b'[project]\ndependencies = ["typer>=1.0"]'
    path.write_bytes(original)
    project = read_project(path)
    dependency = next(dependencies(project.document))
    change = prepare_change(project, [Update(dependency, "typer>=2.0", Version("2.0"))])
    assert change.content == original.replace(b"1.0", b"2.0")
    diff = preview_diff(change)
    assert diff.count("\\ No newline at end of file") == 2
    assert '-dependencies = ["typer>=1.0"]\n' in diff
    assert '+dependencies = ["typer>=2.0"]\n' in diff


def test_snapshot_checks_do_not_write(tmp_path):
    path = tmp_path / "pyproject.toml"
    original = b'[project]\ndependencies = ["typer>=1.0"]\n'
    path.write_bytes(original)
    project = read_project(path)
    dependency = next(dependencies(project.document))
    change = prepare_change(project, [Update(dependency, "typer>=2.0", Version("2.0"))])
    check_snapshots([change])
    assert path.read_bytes() == original
    path.write_bytes(original + b"# User edit.\n")
    with pytest.raises(ValueError, match="changed during review"):
        check_snapshots([change])
    assert path.read_bytes() == original + b"# User edit.\n"
    path.unlink()
    with pytest.raises(OSError, match=r"Cannot read.*before writing"):
        check_snapshots([change])


def test_diff_escapes_terminal_controls_without_changing_content(tmp_path):
    original = b'[project]\r\ndependencies = ["typer>=1.0"] # \x1b[2J\x07\r\n'
    project = Project(tmp_path / "\x1b[2Jpyproject.toml", tomlkit.document(), original)
    content = original.replace(b"1.0", b"2.0")
    change = Change(project, content, 1)
    diff = preview_diff(change)
    assert "\x1b" not in diff
    assert "\x07" not in diff
    assert "\r" not in diff
    assert r"\x1b[2J" in diff
    assert r"\x07" in diff
    assert change.content == content
