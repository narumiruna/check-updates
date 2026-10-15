"""Automatic locking tests use mocked processes and release lookups."""

import json
import subprocess
from pathlib import Path

import pytest
from rich.text import Text
from typer.testing import CliRunner

from check_updates import cli
from check_updates import locking
from tests.test_cli import project as project

runner = CliRunner()


@pytest.mark.parametrize("flags", [["-u"], ["--upgrade"], ["-i"]])
def test_locks_once_after_writes(project, terminal, monkeypatch, flags):
    lock = project.with_name("uv.lock")
    lock.write_bytes(b"old lock\n")
    original = project.read_bytes()
    calls = []
    monkeypatch.setattr(cli, "select_updates", lambda pending: pending)
    monkeypatch.setattr(cli, "confirm_changes", lambda: True)

    def run(args, **kwargs):
        assert project.read_bytes() == original.replace(b"0.27.1", b"0.27.3")
        calls.append((args, kwargs))
        lock.write_bytes(b"new lock\n")
        return subprocess.CompletedProcess(args, 0)

    monkeypatch.setattr(locking.subprocess, "run", run)
    result = runner.invoke(cli.app, flags)
    assert result.exit_code == 0, result.output
    assert calls == [(["uv", "lock"], {"cwd": project.parent, "stdin": subprocess.DEVNULL, "check": True})]
    assert lock.read_bytes() == b"new lock\n"
    assert "Running uv lock" in result.stdout
    assert "uv lock completed." in result.stdout


@pytest.mark.parametrize("flags", [[], ["--format", "json"], ["--diff"], ["--check"]])
def test_read_only_never_locks(project, lock_process, flags):
    project.with_name("uv.lock").write_bytes(b"old lock\n")
    original = project.read_bytes()
    result = runner.invoke(cli.app, flags)
    assert result.exit_code == (3 if "--check" in flags else 0), result.output
    assert lock_process == []
    assert project.read_bytes() == original
    assert "Running uv lock" not in result.output
    if "json" in flags:
        assert json.loads(result.stdout)["schema_version"] == 1


@pytest.mark.parametrize("flags", [["-u"], ["-i"]])
@pytest.mark.parametrize("lock_kind", ["missing", "directory"])
def test_no_regular_lockfile_skips_locking(project, terminal, monkeypatch, lock_process, flags, lock_kind):
    if lock_kind == "directory":
        project.with_name("uv.lock").mkdir()
    monkeypatch.setattr(cli, "select_updates", lambda pending: pending)
    monkeypatch.setattr(cli, "confirm_changes", lambda: True)
    result = runner.invoke(cli.app, flags)
    assert result.exit_code == 0, result.output
    assert "0.27.3" in project.read_text()
    assert lock_process == []
    assert "uv lock completed" not in result.output
    assert (
        project.with_name("uv.lock").is_dir() if lock_kind == "directory" else not project.with_name("uv.lock").exists()
    )


@pytest.mark.parametrize("flags", [["-u"], ["-i"]])
def test_no_updates_skips_locking(project, terminal, lock_process, flags):
    project.with_name("uv.lock").write_bytes(b"old lock\n")
    project.write_text('[project]\ndependencies = ["typer>=0.27.3"]\n')
    result = runner.invoke(cli.app, flags)
    assert result.exit_code == 0, result.output
    assert lock_process == []


@pytest.mark.parametrize("force_color", [False, True])
@pytest.mark.parametrize("flags", [[], ["--format", "json"], ["--diff"], ["--check"], ["-w"]])
def test_no_lock_requires_write_mode_before_reads(monkeypatch, flags, force_color):
    monkeypatch.delenv("FORCE_COLOR", raising=False)
    monkeypatch.delenv("NO_COLOR", raising=False)
    monkeypatch.setenv("FORCE_COLOR" if force_color else "NO_COLOR", "1")
    monkeypatch.setattr(cli, "load_projects", lambda *_args: pytest.fail("Must not read projects."))
    result = runner.invoke(cli.app, [*flags, "--no-lock"])
    assert result.exit_code == 2, result.output
    assert "--no-lock requires" in Text.from_ansi(result.output).plain


@pytest.mark.parametrize("debug", [False, True])
@pytest.mark.parametrize(
    ("failure", "code"),
    [
        (FileNotFoundError("uv not found"), 1),
        (PermissionError("cannot start uv"), 1),
        (subprocess.CalledProcessError(7, ["uv", "lock"]), 1),
        (KeyboardInterrupt(), 130),
    ],
)
def test_lock_failure_retains_updates_and_reports_recovery(project, monkeypatch, failure, code, debug):
    lock = project.with_name("uv.lock")
    lock.write_bytes(b"old lock\n")
    original = project.read_bytes()

    def run(*_args, **_kwargs):
        # Even a failed resolver can change the lockfile. Do not restore it.
        lock.write_bytes(b"partial lock\n")
        raise failure

    monkeypatch.setattr(locking.subprocess, "run", run)
    result = runner.invoke(cli.app, ["-u", *(["--debug"] if debug else [])])
    assert result.exit_code == code, result.output
    assert project.read_bytes() == original.replace(b"0.27.1", b"0.27.3")
    assert lock.read_bytes() == b"partial lock\n"
    assert "Requirement files were updated" in result.stderr
    assert "uv.lock may be stale or changed" in result.stderr
    assert f"run uv lock in {project.parent}" in result.stderr
    assert "review and restore the affected files" in result.stderr
    assert "uv lock completed" not in result.output
    assert ("Traceback (most recent call last)" in result.stderr) is debug
    if code == 130:
        assert "lock completion is not confirmed" in result.stderr


def test_lockfile_inspection_failure_is_post_write_error(project, monkeypatch):
    is_file = Path.is_file

    def inspect(path):
        if path.name == "uv.lock":
            raise PermissionError("cannot inspect lockfile")
        return is_file(path)

    monkeypatch.setattr(Path, "is_file", inspect)
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 1, result.output
    assert "0.27.3" in project.read_text()
    assert "Requirement files were updated" in result.stderr


def test_lookup_options_are_not_forwarded_to_uv(project, monkeypatch, lock_process):
    project.with_name("uv.lock").write_bytes(b"old lock\n")
    # Bypass remote lookups, but retain the real collection and write flow.
    monkeypatch.setattr(cli.ReleaseClient, "get", lambda self, name, indexes: self._public(name))
    result = runner.invoke(
        cli.app,
        ["-u", "--index", "https://user:secret@example.com/simple/", "--include", "typer", "--no-cache"],
    )
    assert result.exit_code == 0, result.output
    assert lock_process == [(["uv", "lock"], {"cwd": project.parent, "stdin": subprocess.DEVNULL, "check": True})]
    assert "secret" not in result.output
