"""Route uv index sources for each optional-dependency occurrence."""

import json

import pytest
import tomlkit
from packaging.version import Version
from typer.testing import CliRunner

from check_updates import cli
from check_updates.indexes import Index
from check_updates.indexes import Release
from check_updates.indexes import ReleaseClient
from check_updates.workspace import package_indexes
from check_updates.workspace import read_project

runner = CliRunner()
PAIR = 'demo = [{index = "cpu", extra = "cpu"}, {index = "gpu", extra = "gpu"}]'
SINGLE = 'demo = {index = "cpu", extra = "cpu"}'
CONFIG = (
    '[[tool.uv.index]]\nname = "cpu"\nurl = "https://cpu.example/simple"\nexplicit = true\n'
    '[[tool.uv.index]]\nname = "gpu"\nurl = "https://gpu.example/simple"\nexplicit = true\n'
    "[tool.uv.sources]\n" + PAIR + "\n"
)


@pytest.fixture
def project(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    path.write_text(
        '[project]\ndependencies = ["demo>=1"]\n'
        '[project.optional-dependencies]\ncpu = ["demo>=1"]\ngpu = ["demo>=1"]\n'
        '[dependency-groups]\ndev = ["demo>=1"]\n' + CONFIG
    )
    calls = []

    def public(self, name):
        calls.append("public:" + name)
        return [Release(Version("1.5"))]

    def read(self, request):
        calls.append(request.full_url)
        version = "2" if "cpu.example" in request.full_url else "3"
        return json.dumps({"files": [{"filename": f"demo-{version}.tar.gz"}]}).encode(), "application/json"

    monkeypatch.setattr(ReleaseClient, "_public", public)
    monkeypatch.setattr(ReleaseClient, "_read", read)
    return path, calls


@pytest.mark.parametrize(
    ("flags", "versions", "calls"),
    [
        (["--extra", "cpu"], ["2"], ["https://cpu.example/simple/demo/"]),
        (["--extra", "gpu"], ["3"], ["https://gpu.example/simple/demo/"]),
        (
            ["--extra", "cpu", "--extra", "gpu"],
            ["2", "3"],
            ["https://cpu.example/simple/demo/", "https://gpu.example/simple/demo/"],
        ),
        (["--runtime"], ["1.5"], ["public:demo"]),
        (["--group", "dev"], ["1.5"], ["public:demo"]),
        (
            [],
            ["1.5", "2", "3", "1.5"],
            ["public:demo", "https://cpu.example/simple/demo/", "https://gpu.example/simple/demo/"],
        ),
    ],
)
def test_extra_routes_and_metadata_caches_are_separate(project, flags, versions, calls):
    path, seen = project
    original = path.read_bytes()
    result = runner.invoke(cli.app, ["--format", "json", *flags])
    assert result.exit_code == 0, result.output
    updates = json.loads(result.stdout)["updates"]
    assert [update["version"] for update in updates] == versions
    assert seen == calls
    assert path.read_bytes() == original


@pytest.mark.parametrize("single", [False, True])
def test_extra_upgrade_leaves_other_occurrences_unchanged(project, single):
    path, calls = project
    if single:
        path.write_text(path.read_text().replace(PAIR, SINGLE))
    original = path.read_bytes()
    result = runner.invoke(cli.app, ["--extra", "cpu", "-u"])
    assert result.exit_code == 0, result.output
    assert calls == ["https://cpu.example/simple/demo/"]
    assert path.read_bytes() == original.replace(b'cpu = ["demo>=1"]', b'cpu = ["demo>=2"]')


def test_single_extra_source_does_not_apply_to_runtime_or_groups(project):
    path, calls = project
    path.write_text(path.read_text().replace(PAIR, SINGLE))
    result = runner.invoke(cli.app, ["--format", "json"])
    assert result.exit_code == 0, result.output
    updates = json.loads(result.stdout)["updates"]
    assert [update["version"] for update in updates] == ["1.5", "2", "1.5", "1.5"]
    assert calls == ["public:demo", "https://cpu.example/simple/demo/"]


def test_all_occurrences_upgrade_from_their_own_sources(project):
    path, _ = project
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 0, result.output
    document = tomlkit.parse(path.read_text())
    assert document["project"]["dependencies"] == ["demo>=1.5"]
    assert document["project"]["optional-dependencies"]["cpu"] == ["demo>=2"]
    assert document["project"]["optional-dependencies"]["gpu"] == ["demo>=3"]
    assert document["dependency-groups"]["dev"] == ["demo>=1.5"]


def test_source_extra_names_use_normalized_matching(project):
    path, _ = project
    path.write_text(path.read_text().replace('extra = "cpu"', 'extra = "CPU_cache"'))
    loaded = read_project(path)
    found = package_indexes(loaded, loaded, "demo", [], section=("project", "optional-dependencies", "cpu-cache"))
    assert found == (Index("https://cpu.example/simple", "cpu", default=True),)
    assert package_indexes(loaded, loaded, "demo", [], section=("project", "dependencies")) == ()
    assert package_indexes(loaded, loaded, "demo", [], section=("dependency-groups", "cpu-cache")) == ()


def test_unqualified_source_still_applies_to_every_section(project):
    path, _ = project
    path.write_text(path.read_text().replace(PAIR, 'demo = {index = "cpu"}'))
    loaded = read_project(path)
    for section in (
        ("project", "dependencies"),
        ("project", "optional-dependencies", "gpu"),
        ("dependency-groups", "dev"),
    ):
        assert package_indexes(loaded, loaded, "demo", [], section=section) == (
            Index("https://cpu.example/simple", "cpu", default=True),
        )


@pytest.mark.parametrize(
    "source",
    [
        'demo = [{index = "cpu", extra = "cpu"}, {index = "gpu", extra = "cpu"}]',
        'demo = {index = "cpu", extra = "cpu", marker = "python_version < \'3.12\'"}',
        'demo = {index = "cpu", extra = ["cpu"]}',
    ],
)
def test_ambiguous_or_unsupported_selected_sources_do_not_write(project, source):
    path, calls = project
    path.write_text(path.read_text().replace(PAIR, source))
    original = path.read_bytes()
    result = runner.invoke(cli.app, ["--extra", "cpu", "-u"])
    assert result.exit_code == 1, result.output
    assert calls == []
    assert path.read_bytes() == original


def test_workspace_root_extra_sources_apply_to_member_occurrences(project):
    path, calls = project
    path.write_text(path.read_text() + '[tool.uv.workspace]\nmembers = ["member"]\n')
    member = path.parent / "member"
    member.mkdir()
    child = member / "pyproject.toml"
    child.write_text('[project.optional-dependencies]\ncpu = ["demo>=1"]\ngpu = ["demo>=1"]\n')
    result = runner.invoke(cli.app, ["-w", "--extra", "cpu", "--format", "json"])
    assert result.exit_code == 0, result.output
    updates = json.loads(result.stdout)["updates"]
    assert len(updates) == 2
    assert [update["version"] for update in updates] == ["2", "2"]
    assert calls == ["https://cpu.example/simple/demo/"]


def test_cli_index_override_still_applies_to_extra_occurrences(project):
    path, calls = project
    original = path.read_bytes()
    result = runner.invoke(cli.app, ["--extra", "cpu", "--index", "https://gpu.example/simple", "--format", "json"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["updates"][0]["version"] == "3"
    assert calls == ["https://gpu.example/simple/demo/"]
    assert path.read_bytes() == original


def test_normalized_extra_selector_keeps_qualified_index_routing(project):
    path, calls = project
    path.write_text(
        path.read_text()
        .replace('cpu = ["demo>=1"]', 'cpu-tools = ["demo>=1"]')
        .replace('extra = "cpu"', 'extra = "CPU_tools"')
    )
    result = runner.invoke(cli.app, ["--extra", "CPU.Tools", "--format", "json"])
    assert result.exit_code == 0, result.output
    updates = json.loads(result.stdout)["updates"]
    assert len(updates) == 1
    assert updates[0]["version"] == "2"
    assert updates[0]["section"] == ["project", "optional-dependencies", "cpu-tools"]
    assert calls == ["https://cpu.example/simple/demo/"]
