"""Verify scoped debug diagnostics without real network requests."""

import io
import json
import logging

import pytest
from packaging.version import Version
from typer.testing import CliRunner

from check_updates import cli
from check_updates import dependencies

runner = CliRunner()


@pytest.fixture
def project(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    path.write_text('[project]\ndependencies = ["demo>=1.0", "demo==1.0", "demo>=2.0", "other<1.0"]\n')
    monkeypatch.setattr(cli, "latest_version", lambda _name: Version("2.0"))
    return path


def test_debug_details_only_on_stderr_and_scoped(project):
    original = project.read_bytes()
    logger = logging.getLogger("check_updates")
    state = (logger.level, logger.propagate, list(logger.handlers))
    normal = runner.invoke(cli.app)
    debug = runner.invoke(cli.app, ["--debug"])
    assert debug.exit_code == normal.exit_code == 0
    assert debug.stdout == normal.stdout
    assert "DEBUG" not in normal.stderr
    for message in (
        "Starting check:",
        "Discovering projects",
        "Reading project file",
        "Parsed",
        "Loaded 1 project files",
        "Checking demo in project.dependencies[0]",
        "cache miss for demo",
        "cache hit for demo",
        "Update available for demo",
        "No update for demo",
        "Skipping other: unsupported version constraint",
        "Check complete: 2 updates in 1 files; 1 cached packages",
        "Command completed",
    ):
        assert message in debug.stderr
    assert (logger.level, logger.propagate, logger.handlers) == state
    assert "DEBUG" not in runner.invoke(cli.app).stderr
    assert project.read_bytes() == original


@pytest.mark.parametrize("debug", [False, True])
def test_error_traceback_is_opt_in_and_logging_restored(project, monkeypatch, debug):
    def fail(_name):
        raise ValueError("lookup failed")

    monkeypatch.setattr(cli, "latest_version", fail)
    result = runner.invoke(cli.app, ["--debug"] if debug else [])
    assert result.exit_code == 1
    assert "Error: lookup failed" in result.stderr
    assert ("Traceback (most recent call last)" in result.stderr) is debug
    assert "DEBUG" not in runner.invoke(cli.app).stderr


def test_debug_upgrade_records_writes(project):
    result = runner.invoke(cli.app, ["--debug", "-u"])
    assert result.exit_code == 0, result.output
    assert "Writing 2 updates to pyproject.toml" in result.stderr
    assert "Write completed for pyproject.toml" in result.stderr
    assert '"demo==2.0"' in project.read_text()


def test_debug_pypi_filtering(project, monkeypatch):
    releases = {
        "1.0": [{"yanked": False}],
        "2.0": [{"yanked": False}],
        "3.0rc1": [{"yanked": False}],
        "4.0.dev1": [{"yanked": False}],
        "5.0": [{"yanked": True}],
        "6.0": [],
        "invalid": [],
    }
    monkeypatch.setattr(cli, "latest_version", dependencies.latest_version)
    monkeypatch.setattr(
        dependencies,
        "urlopen",
        lambda *_args, **_kwargs: io.BytesIO(json.dumps({"releases": releases}).encode()),
    )
    result = runner.invoke(cli.app, ["--debug"])
    assert result.exit_code == 0, result.output
    assert "Requesting https://pypi.org/pypi/demo/json (timeout: 15s)" in result.stderr
    assert "Received 7 release entries" in result.stderr
    assert "2 eligible, 1 invalid, 2 prerelease/dev, 2 empty or fully yanked" in result.stderr
    assert "Selected latest stable release for demo: 2.0" in result.stderr


def test_debug_workspace_discovery(project):
    project.write_text(
        '[project]\nname = "root"\ndependencies = ["demo>=1.0"]\n'
        '[tool.uv.workspace]\nmembers = ["packages/*"]\nexclude = ["packages/excluded"]\n'
        "[tool.uv.sources]\nlocal = {workspace = true}\n"
    )
    member = project.parent / "packages" / "member"
    member.mkdir(parents=True)
    (member / "pyproject.toml").write_text('[project]\ndependencies = ["demo>=1.0", "local"]\n')
    (member.parent / "excluded").mkdir()
    result = runner.invoke(cli.app, ["--debug", "-w"])
    assert result.exit_code == 0, result.output
    for message in (
        "Found workspace root",
        "Workspace member patterns",
        "Excluded workspace paths",
        "matched 2 paths",
        "Selected workspace member files",
        "Loaded 2 project files",
        "cache hit for demo",
        "Skipping local: workspace dependency",
    ):
        assert message in result.stderr


@pytest.mark.parametrize("confirmed", [False, True])
def test_debug_interactive_confirmation(project, monkeypatch, terminal, confirmed):
    original = project.read_bytes()
    monkeypatch.setattr(cli, "select_updates", lambda pending: pending)
    monkeypatch.setattr(cli, "confirm_changes", lambda: confirmed)
    result = runner.invoke(cli.app, ["--debug", "-i"])
    assert result.exit_code == 0, result.output
    assert "Opening interactive update selector" in result.stderr
    assert "Selected 2 updates in 1 files" in result.stderr
    if confirmed:
        assert "Write confirmation accepted" in result.stderr
        assert "File snapshots match" in result.stderr
        assert "Write completed" in result.stderr
        assert project.read_bytes() != original
    else:
        assert "Write confirmation declined" in result.stderr
        assert "Writing" not in result.stderr
        assert project.read_bytes() == original


def test_invalid_options_restore_logging(project):
    result = runner.invoke(cli.app, ["--debug", "-u", "-i"])
    assert result.exit_code == 2
    assert "cannot be combined" in result.output
    assert "DEBUG" not in runner.invoke(cli.app).stderr


def test_debug_help():
    result = runner.invoke(cli.app, ["--help"])
    assert result.exit_code == 0
    assert "--debug" in result.stdout
    assert "tracebacks" in result.stdout
