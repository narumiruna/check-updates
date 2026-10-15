from pathlib import Path

import pytest
from prompt_toolkit.application import create_app_session
from prompt_toolkit.input import create_pipe_input
from prompt_toolkit.output import DummyOutput
from typer.testing import CliRunner

from check_updates import cli
from check_updates import interactive
from check_updates.indexes import Release
from tests.test_cli import project as project
from tests.test_workspace import workspace as workspace

runner = CliRunner()


@pytest.fixture
def select_all(monkeypatch):
    monkeypatch.setattr(cli, "select_updates", lambda pending: pending)
    monkeypatch.setattr(cli, "confirm_changes", lambda: True)


def run_confirmation(keys):
    with create_pipe_input() as pipe, create_app_session(input=pipe, output=DummyOutput()):
        pipe.send_text(keys)
        return interactive.confirm_changes()


@pytest.mark.parametrize("flag", ["-i", "--interactive"])
def test_interactive_cli_with_real_keyboard_and_default_no(project, terminal, monkeypatch, flag):
    original = project.read_bytes()
    monkeypatch.setattr(cli, "confirm_changes", lambda: run_confirmation("\r"))
    with create_pipe_input() as pipe, create_app_session(input=pipe, output=DummyOutput()):
        # Select the project and review with Enter; confirmation uses its own stream.
        pipe.send_text(" \r")
        result = runner.invoke(cli.app, [flag])
    assert result.exit_code == 0, result.output
    assert "--- pyproject.toml" in result.output
    assert '+  "typer>=0.27.3"' in result.output
    assert "Selected 1 dependencies in 1 files." in result.output
    assert "Cancelled" in result.output
    assert project.read_bytes() == original


def test_interactive_cli_with_real_keyboard_confirmation(project, terminal, monkeypatch):
    original = project.read_bytes()
    monkeypatch.setattr(cli, "confirm_changes", lambda: run_confirmation("y"))
    with create_pipe_input() as pipe, create_app_session(input=pipe, output=DummyOutput()):
        pipe.send_text(" \r")
        result = runner.invoke(cli.app, ["-i"])
    assert result.exit_code == 0, result.output
    assert project.read_bytes() == original.replace(b"0.27.1", b"0.27.3")


@pytest.mark.parametrize("flags", [["-i", "-u"], ["--interactive", "--upgrade"], ["-i", "-u", "-w"]])
def test_invalid_flags_fail_before_reads_or_lookups(project, terminal, monkeypatch, flags):
    original = project.read_bytes()
    monkeypatch.setattr(cli, "load_projects", lambda *_args: pytest.fail("Must not read files."))
    monkeypatch.setattr(
        cli.ReleaseClient,
        "_public",
        lambda self, name: [Release((lambda *_args: pytest.fail("Must not look up packages."))(name))],
    )
    result = runner.invoke(cli.app, flags)
    assert result.exit_code == 2
    assert "cannot be combined" in result.output
    assert project.read_bytes() == original


@pytest.mark.parametrize("stream", ["stdin", "stdout"])
def test_non_terminal_fails_before_reads_or_lookups(project, terminal, monkeypatch, stream):
    original = project.read_bytes()
    getattr(terminal, stream).isatty = lambda: False
    monkeypatch.setattr(cli, "load_projects", lambda *_args: pytest.fail("Must not read files."))
    result = runner.invoke(cli.app, ["-i"])
    assert result.exit_code == 2
    assert "requires terminal input and output" in result.output
    assert project.read_bytes() == original


def test_empty_selection_does_not_confirm_or_write(project, terminal, monkeypatch):
    original = project.read_bytes()
    monkeypatch.setattr(cli, "select_updates", lambda _pending: [])
    monkeypatch.setattr(cli, "confirm_changes", lambda: pytest.fail("Must not confirm."))
    result = runner.invoke(cli.app, ["-i"])
    assert result.exit_code == 0, result.output
    assert "No updates selected" in result.output
    assert project.read_bytes() == original


@pytest.mark.parametrize("selection", ["empty", "declined", "accepted-no-lock"])
def test_interactive_skips_locking(project, terminal, monkeypatch, lock_process, selection):
    lock = project.with_name("uv.lock")
    lock.write_bytes(b"old lock\n")
    original = project.read_bytes()
    monkeypatch.setattr(cli, "select_updates", lambda pending: [] if selection == "empty" else pending)
    monkeypatch.setattr(cli, "confirm_changes", lambda: selection == "accepted-no-lock")
    flags = ["-i", *(["--no-lock"] if selection == "accepted-no-lock" else [])]
    result = runner.invoke(cli.app, flags)
    assert result.exit_code == 0, result.output
    assert lock_process == []
    assert lock.read_bytes() == b"old lock\n"
    assert project.read_bytes() == (
        original.replace(b"0.27.1", b"0.27.3") if selection == "accepted-no-lock" else original
    )


def test_no_updates_does_not_open_selector(project, terminal, monkeypatch):
    project.write_text('[project]\ndependencies = ["typer>=0.27.3"]\n')
    original = project.read_bytes()
    monkeypatch.setattr(cli, "select_updates", lambda _pending: pytest.fail("Must not open a selector."))
    result = runner.invoke(cli.app, ["-i"])
    assert result.exit_code == 0
    assert "No dependency updates available." in result.output
    assert project.read_bytes() == original


@pytest.mark.parametrize("boundary", ["select_updates", "confirm_changes"])
@pytest.mark.parametrize(("exception", "code"), [(KeyboardInterrupt, 130), (EOFError, 0)])
def test_prompt_interrupt_or_eof_does_not_write(project, terminal, select_all, monkeypatch, boundary, exception, code):
    original = project.read_bytes()

    def abort(*_args):
        raise exception

    monkeypatch.setattr(cli, boundary, abort)
    result = runner.invoke(cli.app, ["-i"])
    assert result.exit_code == code, result.output
    assert project.read_bytes() == original


@pytest.mark.parametrize("problem", ["edited", "deleted", "unreadable"])
def test_file_changed_during_confirmation_is_not_overwritten(project, terminal, select_all, monkeypatch, problem):
    original = project.read_bytes()
    read_bytes = Path.read_bytes

    def confirm():
        if problem == "edited":
            project.write_bytes(original + b"# User edit.\n")
        elif problem == "deleted":
            project.unlink()
        else:

            def fail_read(path):
                if path.name == "pyproject.toml":
                    raise PermissionError("unreadable")
                return read_bytes(path)

            monkeypatch.setattr(Path, "read_bytes", fail_read)
        return True

    monkeypatch.setattr(cli, "confirm_changes", confirm)
    monkeypatch.setattr(cli, "write_change", lambda _change: pytest.fail("Must not write stale files."))
    result = runner.invoke(cli.app, ["-i"])
    assert result.exit_code == 1, result.output
    assert "pyproject.toml" in result.output
    if problem == "edited":
        assert project.read_bytes() == original + b"# User edit.\n"
    elif problem == "deleted":
        assert not project.exists()
    else:
        assert read_bytes(project) == original


def test_single_occurrence_selected_and_previewed_bytes_are_written(project, terminal, monkeypatch):
    project.write_text('[project]\ndependencies = ["typer>=0.27.1", "typer>=0.27.1"]\n')
    original = project.read_bytes()
    prepared = []
    prepare = cli.prepare_change

    def prepare_selected(selected_project, updates):
        change = prepare(selected_project, updates)
        prepared.append(change)
        return change

    monkeypatch.setattr(cli, "prepare_change", prepare_selected)
    monkeypatch.setattr(cli, "select_updates", lambda pending: [(pending[0][0], [pending[0][1][1]])])
    monkeypatch.setattr(cli, "confirm_changes", lambda: True)
    result = runner.invoke(cli.app, ["-i"])
    assert result.exit_code == 0, result.output
    expected = original.replace(b'"typer>=0.27.1"]', b'"typer>=0.27.3"]')
    assert project.read_bytes() == expected == prepared[0].content
    assert '+dependencies = ["typer>=0.27.1", "typer>=0.27.3"]' in result.output


def test_file_write_error_identifies_path(project, terminal, select_all, monkeypatch):
    original = project.read_bytes()

    def fail_write(_path, _content):
        raise PermissionError("denied")

    monkeypatch.setattr(Path, "write_bytes", fail_write)
    result = runner.invoke(cli.app, ["-i"])
    assert result.exit_code == 1
    assert "Cannot write pyproject.toml" in result.output
    assert "denied" in result.output
    assert project.read_bytes() == original


@pytest.mark.parametrize("include_root", [False, True])
def test_workspace_partial_selection_and_shared_lookups(workspace, terminal, monkeypatch, include_root, lock_process):
    root, member, excluded, unrelated, lock, calls = workspace
    originals = {path: path.read_bytes() for path in (root, member, excluded, unrelated, lock)}

    def select(pending):
        assert len(pending) == 2
        assert calls == ["typer", "pytest"]
        selected = [(pending[1][0], [pending[1][1][0]])]
        return [pending[0], *selected] if include_root else selected

    monkeypatch.setattr(cli, "select_updates", select)
    monkeypatch.setattr(cli, "confirm_changes", lambda: True)
    result = runner.invoke(cli.app, ["-i", "-w"])
    assert result.exit_code == 0, result.output
    assert member.read_bytes() == originals[member].replace(b"typer[all]>=1.0", b"typer[all]>=2.0")
    for path in (excluded, unrelated, lock):
        assert path.read_bytes() == originals[path]
    assert root.read_bytes() == (originals[root].replace(b"1.0", b"2.0") if include_root else originals[root])
    assert f"--- {member}" in result.output
    assert (f"--- {root}" in result.output) == include_root
    assert 'dev = ["pytest==2.0"]' not in result.output
    assert len(lock_process) == 1
    assert lock_process[0][1]["cwd"] == root.parent


def test_workspace_member_override_is_preserved(workspace, terminal, select_all):
    _root, member, _excluded, _unrelated, _lock, calls = workspace
    with member.open("a") as stream:
        stream.write("[tool.uv.sources]\nbird-feeder = {}\n")
    result = runner.invoke(cli.app, ["-i", "-w"])
    assert result.exit_code == 0, result.output
    assert calls == ["typer", "bird-feeder", "pytest"]
    assert "bird-feeder>=2.0" in member.read_text()
    assert "bird-feeder = {}" in member.read_text()


def test_workspace_stale_last_file_blocks_all_writes(workspace, terminal, select_all, monkeypatch, lock_process):
    root, member, *_rest = workspace
    root_original = root.read_bytes()
    member_original = member.read_bytes()

    def confirm():
        member.write_bytes(member_original + b"# User edit.\n")
        return True

    monkeypatch.setattr(cli, "confirm_changes", confirm)
    result = runner.invoke(cli.app, ["-i", "-w"])
    assert result.exit_code == 1
    assert "changed during review" in result.output
    assert root.read_bytes() == root_original
    assert member.read_bytes() == member_original + b"# User edit.\n"
    assert lock_process == []


def test_workspace_write_failure_reports_prior_success(workspace, terminal, select_all, monkeypatch, lock_process):
    root, member, *_rest = workspace
    root_original = root.read_bytes()
    member_original = member.read_bytes()
    write_bytes = Path.write_bytes

    def write(path, content):
        if path == member:
            raise PermissionError("denied")
        return write_bytes(path, content)

    monkeypatch.setattr(Path, "write_bytes", write)
    result = runner.invoke(cli.app, ["-i", "-w"])
    assert result.exit_code == 1
    assert f"Updated 1 dependencies in {root}." in result.output
    assert f"Cannot write {member}" in result.output
    assert root.read_bytes() == root_original.replace(b"1.0", b"2.0")
    assert member.read_bytes() == member_original
    assert lock_process == []


def test_workspace_lookup_failure_does_not_open_selector(workspace, terminal, monkeypatch):
    root, member, *_rest = workspace
    originals = {path: path.read_bytes() for path in (root, member)}

    def fetch(_name):
        raise TimeoutError("timeout")

    monkeypatch.setattr(cli.ReleaseClient, "_public", lambda self, name: [Release(fetch(name))])
    monkeypatch.setattr(cli, "select_updates", lambda _pending: pytest.fail("Must not open selector."))
    result = runner.invoke(cli.app, ["-i", "-w"])
    assert result.exit_code == 1
    assert all(path.read_bytes() == original for path, original in originals.items())


def test_file_changed_during_selection_is_not_overwritten(project, terminal, monkeypatch):
    original = project.read_bytes()

    def select(pending):
        project.write_bytes(original + b"# User edit during selection.\n")
        return pending

    monkeypatch.setattr(cli, "select_updates", select)
    monkeypatch.setattr(cli, "confirm_changes", lambda: True)
    result = runner.invoke(cli.app, ["-i"])
    assert result.exit_code == 1
    assert "changed during review" in result.output
    assert project.read_bytes() == original + b"# User edit during selection.\n"


@pytest.mark.parametrize("content", ["[project", '[project]\ndependencies = ["invalid ???"]\n'])
def test_interactive_invalid_input_does_not_open_selector(project, terminal, monkeypatch, content):
    project.write_text(content)
    monkeypatch.setattr(cli, "select_updates", lambda _pending: pytest.fail("Must not open selector."))
    result = runner.invoke(cli.app, ["-i"])
    assert result.exit_code == 1
    assert "Error:" in result.output
    assert project.read_text() == content


@pytest.mark.parametrize("boundary", ["selection", "confirmation"])
def test_real_keyboard_interrupt_returns_130(project, terminal, monkeypatch, boundary):
    original = project.read_bytes()
    if boundary == "confirmation":
        monkeypatch.setattr(cli, "select_updates", lambda pending: pending)
        monkeypatch.setattr(cli, "confirm_changes", lambda: run_confirmation("\x03"))
        result = runner.invoke(cli.app, ["-i"])
    else:
        with create_pipe_input() as pipe, create_app_session(input=pipe, output=DummyOutput()):
            pipe.send_text("\x03")
            result = runner.invoke(cli.app, ["-i"])
    assert result.exit_code == 130, result.output
    assert project.read_bytes() == original
