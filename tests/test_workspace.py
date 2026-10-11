from urllib.error import URLError

import pytest
from packaging.version import Version
from typer.testing import CliRunner

from check_updates import cli

runner = CliRunner()


@pytest.fixture
def workspace(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = tmp_path / "pyproject.toml"
    root.write_text(
        '[project]\nname = "albatross"\ndependencies = ["bird-feeder", "typer>=1.0"] # Root comment.\n'
        "[tool.uv.sources]\nbird-feeder = { workspace = true }\n"
        '[tool.uv.workspace]\nmembers = ["packages/*", "packages/bird-feeder"]\n'
        'exclude = ["packages/seeds*"]\n'
    )
    member = tmp_path / "packages" / "bird-feeder" / "pyproject.toml"
    member.parent.mkdir(parents=True)
    member.write_text(
        "[project]\nname = 'bird-feeder'\ndependencies = ['typer[all]>=1.0', 'bird-feeder']\n"
        '[dependency-groups]\ndev = ["pytest==1.0"]\n'
    )
    excluded = tmp_path / "packages" / "seeds" / "pyproject.toml"
    excluded.parent.mkdir()
    excluded.write_text("Invalid TOML in an excluded project.")
    unrelated = tmp_path / "other" / "pyproject.toml"
    unrelated.parent.mkdir()
    unrelated.write_text('[project]\ndependencies = ["other>=1.0"]\n')
    lock = tmp_path / "uv.lock"
    lock.write_bytes(b"Do not modify.\n")
    calls = []

    def fetch(name):
        calls.append(name)
        return Version("2.0")

    monkeypatch.setattr(cli, "latest_version", fetch)
    return root, member, excluded, unrelated, lock, calls


@pytest.mark.parametrize("flag", ["-w", "--workspace"])
def test_workspace_listing_does_not_edit(workspace, flag):
    root, member, excluded, unrelated, lock, calls = workspace
    originals = {path: path.read_bytes() for path in (root, member, excluded, unrelated, lock)}
    result = runner.invoke(cli.app, [flag])
    assert result.exit_code == 0, result.output
    assert f"{root}:" in result.output
    assert f"{member}:" in result.output
    assert "workspace dependency" in result.output
    assert "3 dependencies with updates in 2 files." in result.output
    assert "Use -u -w" in result.output
    assert "-i -w" in result.output
    assert calls == ["typer", "pytest"]
    assert all(path.read_bytes() == content for path, content in originals.items())


@pytest.mark.parametrize("flags", [["-u", "-w"], ["--upgrade", "--workspace"]])
def test_upgrade_workspace_preserves_format_and_selection(workspace, flags):
    root, member, excluded, unrelated, lock, calls = workspace
    originals = {path: path.read_bytes() for path in (root, member, excluded, unrelated, lock)}
    result = runner.invoke(cli.app, flags)
    assert result.exit_code == 0, result.output
    for path in (root, member):
        assert path.read_bytes() == originals[path].replace(b"1.0", b"2.0")
    for path in (excluded, unrelated, lock):
        assert path.read_bytes() == originals[path]
    assert calls == ["typer", "pytest"]
    assert result.output.count(f"Updated 2 dependencies in {member}.") == 1


def test_workspace_root_discovery_from_member_subdirectory(workspace, monkeypatch):
    root, member, *_rest = workspace
    directory = member.parent / "src"
    directory.mkdir()
    monkeypatch.chdir(directory)
    result = runner.invoke(cli.app, ["-u", "-w"])
    assert result.exit_code == 0, result.output
    assert "typer>=2.0" in root.read_text()
    assert "typer[all]>=2.0" in member.read_text()


def test_without_workspace_flag_updates_only_current_project(workspace):
    root, member, _excluded, _unrelated, _lock, calls = workspace
    original = member.read_bytes()
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 0, result.output
    assert "typer>=2.0" in root.read_text()
    assert member.read_bytes() == original
    # Without -w, uv configuration remains outside the command's scope.
    assert calls == ["bird-feeder", "typer"]


def test_workspace_lookup_failure_does_not_edit_any_project(workspace, monkeypatch):
    root, member, *_rest = workspace
    originals = {path: path.read_bytes() for path in (root, member)}

    def fetch(name):
        if name == "pytest":
            raise URLError("unavailable")
        return Version("2.0")

    monkeypatch.setattr(cli, "latest_version", fetch)
    result = runner.invoke(cli.app, ["-u", "-w"])
    assert result.exit_code == 1
    assert "unavailable" in result.output
    assert all(path.read_bytes() == content for path, content in originals.items())


@pytest.mark.parametrize("problem", ["missing-file", "invalid-toml", "missing-directory"])
def test_invalid_workspace_member_does_not_edit_root(workspace, problem):
    root, member, *_rest = workspace
    original = root.read_bytes()
    member.unlink()
    if problem == "invalid-toml":
        member.write_text("[project")
    elif problem == "missing-directory":
        member.parent.rmdir()
    result = runner.invoke(cli.app, ["-u", "-w"])
    assert result.exit_code == 1
    assert "bird-feeder" in result.output
    assert root.read_bytes() == original


def test_no_workspace_is_an_error(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    path.write_text('[project]\nname = "standalone"\n')
    result = runner.invoke(cli.app, ["-u", "-w"])
    assert result.exit_code == 1
    assert "No [tool.uv.workspace]" in result.output
    assert path.read_text() == '[project]\nname = "standalone"\n'


@pytest.mark.parametrize(
    "config",
    [
        "[tool.uv.workspace]\n",
        '[tool.uv.workspace]\nmembers = "packages/*"\n',
        "[tool.uv.workspace]\nmembers = [1]\n",
        '[tool.uv.workspace]\nmembers = []\nexclude = "packages/*"\n',
        "[tool.uv]\nworkspace = true\n",
    ],
)
def test_invalid_workspace_configuration(tmp_path, monkeypatch, config):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    path.write_text(config)
    result = runner.invoke(cli.app, ["-u", "-w"])
    assert result.exit_code == 1
    assert "tool.uv.workspace" in result.output
    assert path.read_text() == config


def test_virtual_root_and_recursive_member_globs(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = tmp_path / "pyproject.toml"
    root.write_text('[dependency-groups]\ndev = ["typer>=1.0"]\n[tool.uv.workspace]\nmembers = ["packages/**/app"]\n')
    member = tmp_path / "packages" / "nested" / "app" / "pyproject.toml"
    member.parent.mkdir(parents=True)
    member.write_text('[project]\nname = "app"\ndependencies = ["typer>=1.0"]\n')
    monkeypatch.setattr(cli, "latest_version", lambda _name: Version("2.0"))
    result = runner.invoke(cli.app, ["-u", "-w"])
    assert result.exit_code == 0, result.output
    assert "typer>=2.0" in root.read_text()
    assert "typer>=2.0" in member.read_text()


def test_member_source_overrides_root_source(workspace):
    _root, member, _excluded, _unrelated, _lock, calls = workspace
    with member.open("a") as stream:
        stream.write('[tool.uv.sources]\nbird-feeder = { index = "pypi" }\n')
    result = runner.invoke(cli.app, ["-w"])
    assert result.exit_code == 0, result.output
    assert calls == ["typer", "bird-feeder", "pytest"]


def test_workspace_source_lists_and_external_workspaces(workspace):
    _root, member, _excluded, _unrelated, _lock, calls = workspace
    with member.open("a") as stream:
        stream.write(
            "[tool.uv.sources]\npytest = [{ workspace = true, marker = \"sys_platform == 'linux'\" }]\n"
            'typer = { workspace = "../../external" }\n'
        )
    result = runner.invoke(cli.app, ["-w"])
    assert result.exit_code == 0, result.output
    assert calls == ["typer"]
