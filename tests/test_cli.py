import io
import json
from email.message import Message
from urllib.error import HTTPError
from urllib.error import URLError

import pytest
from packaging.version import Version
from typer.testing import CliRunner

from check_updates import cli
from check_updates import dependencies as dependency_module
from check_updates.indexes import Release
from check_updates.indexes import ReleaseClient

public_lookup = ReleaseClient._public

runner = CliRunner()


@pytest.fixture
def project(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(cli.ReleaseClient, "_public", lambda self, name: [Release(Version("0.27.3"))])
    path = tmp_path / "pyproject.toml"
    path.write_text('[project]\ndependencies = [\n  "typer>=0.27.1", # Keep this comment.\n]\n')
    return path


def test_list_does_not_edit(project):
    original = project.read_bytes()
    result = runner.invoke(cli.app)
    assert result.exit_code == 0, result.output
    assert "Current requirement" in result.output
    assert "typer>=0.27.1" in result.output
    assert "0.27.3" in result.output
    assert "patch" in result.output
    assert "1 dependency with updates in 1 file." in result.output
    assert "No files changed." in result.output
    assert "\x1b[" not in result.output
    assert project.read_bytes() == original


@pytest.mark.parametrize("flag", ["-u", "--upgrade"])
def test_upgrade_preserves_format_and_does_not_touch_lock(project, flag):
    original = project.read_bytes()
    lock = project.with_name("uv.lock")
    lock.write_bytes(b"leave unchanged\n")
    result = runner.invoke(cli.app, [flag])
    assert result.exit_code == 0, result.output
    assert project.read_bytes() == original.replace(b"0.27.1", b"0.27.3")
    assert lock.read_bytes() == b"leave unchanged\n"


def test_groups_extras_markers_quotes_and_cache(project, monkeypatch):
    original = (
        b"[project]\r\n"
        b"dependencies = ['Typer[all]>=0.27.1; python_version < \"3.14\"']\r\n"
        b"[project.optional-dependencies]\r\n"
        b'cli = ["typer==0.27.1"]\r\n'
        b"[dependency-groups]\r\n"
        b'dev = [{include-group = "cli"}, "typer~=0.27.1"]\r\n'
    )
    project.write_bytes(original)
    calls = []

    def fetch(name):
        calls.append(name)
        return Version("0.27.3")

    monkeypatch.setattr(cli.ReleaseClient, "_public", lambda self, name: [Release(fetch(name))])
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 0, result.output
    assert project.read_bytes() == original.replace(b"0.27.1", b"0.27.3")
    assert calls == ["typer"]


@pytest.mark.parametrize(
    ("requirement", "expected"),
    [
        ("typer", "typer>=0.27.3"),
        ("typer[all]; python_version > '3.10'", "typer[all]>=0.27.3 ; python_version > '3.10'"),
        ("typer >= 0.27.1", "typer >= 0.27.3"),
        ("typer (>=0.27.1)", "typer (>=0.27.3)"),
        ("typer>=0.27.3", "typer>=0.27.3"),
        ("typer>=0.28.0", "typer>=0.28.0"),
    ],
)
def test_supported_requirements(project, requirement, expected):
    project.write_text(f"[project]\ndependencies = [{json.dumps(requirement)}]\n")
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 0, result.output
    import tomlkit

    assert tomlkit.parse(project.read_text())["project"]["dependencies"][0] == expected


@pytest.mark.parametrize(
    "requirement",
    [
        "typer==0.27.*",
        "typer!=0.27.1",
        "typer<0.28",
        "typer>0.27.1",
        "typer @ https://example.com/typer.whl",
    ],
)
def test_unsupported_entries_are_skipped_without_network(project, monkeypatch, requirement):
    project.write_text(f"[project]\ndependencies = [{json.dumps(requirement)}]\n")
    original = project.read_bytes()

    def unexpected_fetch(_name):
        pytest.fail("Skipped dependencies must not trigger network requests.")

    monkeypatch.setattr(cli.ReleaseClient, "_public", lambda self, name: [Release(unexpected_fetch(name))])
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 0, result.output
    assert "Skipped" in result.output
    assert project.read_bytes() == original


@pytest.mark.parametrize("content", ["[project", '[project]\ndependencies = ["not valid ???"]'])
def test_invalid_input_is_not_written(project, content):
    project.write_text(content)
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 1
    assert "Error:" in result.output
    assert project.read_text() == content


def test_missing_file(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(cli.app)
    assert result.exit_code == 1
    assert "pyproject.toml" in result.output


def test_network_failure_does_not_apply_partial_updates(project, monkeypatch):
    project.write_text('[project]\ndependencies = ["typer>=0.27.1", "other>=1.0"]\n')
    original = project.read_bytes()

    def fetch(name):
        if name == "other":
            raise URLError("unavailable")
        return Version("0.27.3")

    monkeypatch.setattr(cli.ReleaseClient, "_public", lambda self, name: [Release(fetch(name))])
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 1
    assert "unavailable" in result.output
    assert project.read_bytes() == original


def test_empty_project(project):
    project.write_text("[project]\nname = 'empty'\n")
    result = runner.invoke(cli.app)
    assert result.exit_code == 0
    assert "No dependency updates available." in result.output


def test_pypi_uses_highest_stable_non_yanked_release(monkeypatch):
    releases = {
        "1.0": [{"yanked": False}],
        "1.5": [{"yanked": True}, {"yanked": False}],
        "1.4": [{"yanked": False}],
        "2.0": [{"yanked": True}],
        "3.0rc1": [{"yanked": False}],
        "4.0.dev1": [{"yanked": False}],
        "5.0": [],
        "invalid": [{"yanked": False}],
    }
    calls = []

    def open_url(url, timeout):
        calls.append((url, timeout))
        return io.BytesIO(json.dumps({"releases": releases}).encode())

    monkeypatch.setattr(ReleaseClient, "_read", lambda self, request: (open_url(request.full_url, 15).read(), ""))
    assert dependency_module.latest_version("Some_Package") == Version("1.5")
    assert calls == [("https://pypi.org/pypi/some-package/json", 15)]


@pytest.mark.parametrize("flags", [[], ["-u"], ["--debug", "-u"]])
def test_pypi_not_found_has_actionable_error_without_writes(project, monkeypatch, flags):
    project.write_text('[project]\ndependencies = ["typer>=0.27.1", "private-package"]\n')
    original = project.read_bytes()
    monkeypatch.setattr(ReleaseClient, "_public", public_lookup)

    def open_url(url, timeout):
        if "private-package" in url:
            raise HTTPError(url, 404, "Not Found", Message(), None)
        return io.BytesIO(b'{"releases": {"0.27.3": [{"yanked": false}]}}')

    monkeypatch.setattr(ReleaseClient, "_read", lambda self, request: (open_url(request.full_url, 15).read(), ""))
    result = runner.invoke(cli.app, flags)
    assert result.exit_code == 1
    for message in (
        "Package private-package was not found on public PyPI (HTTP 404)",
        "Check the package name",
        "private or unpublished",
        "Use --index",
        "[tool.uv.sources]",
        "use -w",
        "No files were written",
    ):
        assert message in result.stderr
    assert ("Traceback" in result.stderr) is ("--debug" in flags)
    assert project.read_bytes() == original


@pytest.mark.parametrize("code", [401, 403, 429, 500])
def test_other_http_errors_are_not_reclassified(monkeypatch, code):
    error = HTTPError("https://pypi.org/pypi/example/json", code, "failure", Message(), None)

    def open_url(url, timeout):
        raise error

    monkeypatch.setattr(ReleaseClient, "_read", lambda self, request: (open_url(request.full_url, 15).read(), ""))
    with pytest.raises(ValueError, match=f"HTTP {code}"):
        dependency_module.latest_version("example")


def test_no_stable_release(monkeypatch):
    monkeypatch.setattr(
        ReleaseClient, "_read", lambda self, request: (b'{"releases": {"1.0rc1": [{"yanked": false}]}}', "")
    )
    with pytest.raises(ValueError, match="No stable"):
        dependency_module.latest_version("example")
