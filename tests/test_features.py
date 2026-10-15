"""CLI feature contracts using deterministic release metadata."""

import json

import pytest
from packaging.version import Version
from typer.testing import CliRunner

from check_updates import cli
from check_updates.indexes import Release
from check_updates.indexes import ReleaseClient

runner = CliRunner()


@pytest.fixture
def catalog(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    path.write_text(
        '[project]\nrequires-python = ">=3.10"\ndependencies = ["Demo_Pkg>=1.2.3", "bare"]\n'
        '[project.optional-dependencies]\ntest = ["demo-pkg==1.2.3"]\n'
        '[dependency-groups]\ndev = ["demo-pkg~=1.2.3", {include-group = "other"}]\nother = ["other>=1"]\n'
    )
    calls = []

    def fetch(self, name):
        calls.append(name)
        self.notes[name] = "https://example.org/releases"
        return [Release(Version("1.2.9")), Release(Version("1.9"), ">=3.10"), Release(Version("2"), ">=3.12")]

    monkeypatch.setattr(ReleaseClient, "_public", fetch)
    return path, calls


@pytest.mark.parametrize(
    ("flags", "count", "calls"),
    [
        (["--include", "DEMO.pkg"], 3, ["demo-pkg"]),
        (["--include", "demo-pkg", "--exclude", "demo_pkg"], 0, []),
        (["--group", "dev"], 1, ["demo-pkg"]),
        (["--extra", "test"], 1, ["demo-pkg"]),
        (["--runtime"], 2, ["demo-pkg", "bare"]),
        (["--group", "dev", "--extra", "test"], 2, ["demo-pkg"]),
        (["--group", "dev", "--runtime"], 3, ["demo-pkg", "bare"]),
        (["--target", "patch", "--include", "bare"], 0, []),
    ],
)
def test_filters_apply_before_requests_and_json_is_read_only(catalog, flags, count, calls):
    path, seen = catalog
    before = path.read_bytes()
    result = runner.invoke(cli.app, ["--format", "json", *flags])
    assert result.exit_code == 0, result.output
    data = json.loads(result.stdout)
    assert data["schema_version"] == 1
    assert len(data["updates"]) == count
    assert seen == calls
    assert path.read_bytes() == before
    assert "\x1b" not in result.stdout


def test_json_records_occurrences_links_and_metadata_filtered_version(catalog):
    result = runner.invoke(cli.app, ["--format", "json", "--include", "demo-pkg"])
    updates = json.loads(result.stdout)["updates"]
    assert {u["version"] for u in updates} == {"1.9"}
    assert updates[0] == {
        "file": "pyproject.toml",
        "requirement": "Demo_Pkg>=1.2.3",
        "replacement": "Demo_Pkg>=1.9",
        "version": "1.9",
        "section": ["project", "dependencies"],
        "index": 0,
        "change": "minor",
        "release_url": "https://pypi.org/project/demo-pkg/1.9/",
        "project_release_url": "https://example.org/releases",
    }
    assert len({tuple(u["section"]) for u in updates}) == 3


@pytest.mark.parametrize(("target", "expected"), [("patch", "1.2.9"), ("minor", "1.9"), ("latest", "1.9")])
def test_upgrade_target_and_python_metadata(catalog, target, expected):
    path, _ = catalog
    lock = path.with_name("uv.lock")
    lock.write_bytes(b"unchanged")
    before = path.read_bytes()
    result = runner.invoke(cli.app, ["-u", "--no-lock", "--target", target, "--include", "demo-pkg"])
    assert result.exit_code == 0, result.output
    assert path.read_bytes() == before.replace(b"1.2.3", expected.encode())
    assert lock.read_bytes() == b"unchanged"


@pytest.mark.parametrize("flags", [["--check"], ["--check", "--format", "json"], ["--check", "--diff"]])
def test_check_exit_code_is_read_only(catalog, flags):
    path, _ = catalog
    before = path.read_bytes()
    result = runner.invoke(cli.app, flags)
    assert result.exit_code == 3, result.output
    assert path.read_bytes() == before
    assert runner.invoke(cli.app, [*flags, "--include", "absent"]).exit_code == 0


def test_diff_preview_has_no_tables_or_summaries(catalog):
    path, _ = catalog
    before = path.read_bytes()
    result = runner.invoke(cli.app, ["--diff", "--group", "dev"])
    assert result.exit_code == 0
    assert result.stdout.startswith("--- pyproject.toml\n+++ pyproject.toml\n")
    assert "Current requirement" not in result.stdout
    assert "demo-pkg~=1.9" in result.stdout
    assert path.read_bytes() == before
    assert runner.invoke(cli.app, ["--diff", "--include", "absent"]).stdout == ""


@pytest.mark.parametrize(
    "flags",
    [
        ["--target", "major"],
        ["--format", "yaml"],
        ["--format", "json", "--diff"],
        ["--check", "-u"],
        ["--diff", "-i"],
        ["--format", "json", "-u"],
        ["--check", "-i"],
    ],
)
def test_invalid_feature_combinations_do_not_read_or_fetch(catalog, flags):
    path, calls = catalog
    path.unlink()
    result = runner.invoke(cli.app, flags)
    assert result.exit_code == 2
    assert calls == []


def test_composite_crlf_quotes_and_markers(catalog):
    path, _ = catalog
    before = b"[project]\r\ndependencies = ['demo-pkg[all] (>=1.2.3,<2,!=1.8); python_version >= \"3.10\"'] # keep\r\n"
    path.write_bytes(before)
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 0, result.output
    assert path.read_bytes() == before.replace(b"1.2.3", b"1.9")


def test_interactive_release_and_project_notes(catalog, monkeypatch, terminal):
    path, _ = catalog
    before = path.read_bytes()
    monkeypatch.setattr(cli, "select_updates", lambda pending: pending)
    monkeypatch.setattr(cli, "confirm_changes", lambda: False)
    result = runner.invoke(cli.app, ["-i", "--include", "demo-pkg"])
    assert result.exit_code == 0, result.output
    assert "Release: https://pypi.org/project/demo-pkg/1.9/" in result.stdout
    assert "Release notes: https://example.org/releases" in result.stdout
    assert path.read_bytes() == before


def test_workspace_python_selection_is_not_cached_as_one_version(catalog):
    path, calls = catalog
    path.write_text(
        '[project]\nrequires-python = ">=3.10"\ndependencies = ["demo-pkg>=1"]\n'
        '[tool.uv.workspace]\nmembers = ["member"]\n'
    )
    member = path.parent / "member"
    member.mkdir()
    child = member / "pyproject.toml"
    child.write_text('[project]\nrequires-python = ">=3.12"\ndependencies = ["demo-pkg>=1"]\n')
    result = runner.invoke(cli.app, ["-w", "--format", "json"])
    assert result.exit_code == 0, result.output
    assert [u["version"] for u in json.loads(result.stdout)["updates"]] == ["1.9", "2"]
    assert calls == ["demo-pkg"]


def test_invalid_python_range_does_not_write(catalog):
    path, calls = catalog
    path.write_text('[project]\nrequires-python = ">3.10"\ndependencies = ["demo-pkg>=1"]\n')
    before = path.read_bytes()
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 1
    assert "requires-python" in result.stderr
    assert calls == []
    assert path.read_bytes() == before


def test_no_cache_option_is_forwarded(catalog, monkeypatch):
    seen = []
    initialize = ReleaseClient.__init__

    def create(self, *, no_cache=False):
        seen.append(no_cache)
        initialize(self, no_cache=no_cache)

    monkeypatch.setattr(ReleaseClient, "__init__", create)
    assert runner.invoke(cli.app, ["--no-cache"]).exit_code == 0
    assert seen == [True]


def test_excluded_all_candidates_does_not_change_composite(catalog):
    path, _ = catalog
    path.write_text('[project]\ndependencies = ["demo-pkg>=1.2.3,<1.3,!=1.2.9"]\n')
    before = path.read_bytes()
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 0, result.output
    assert path.read_bytes() == before


@pytest.mark.parametrize(
    ("declared", "selector"),
    [
        ("qa-tools", "qa_tools"),
        ("qa_tools", "QA.Tools"),
        ("QA.Tools", "qa-tools"),
        ("qa-tools", "QA.._-TOOLS"),
    ],
)
@pytest.mark.parametrize("option", ["--group", "--extra"])
@pytest.mark.parametrize("upgrade", [False, True])
def test_section_filters_normalize_both_names_and_preserve_original_sections(
    catalog, declared, selector, option, upgrade
):
    path, calls = catalog
    extra_block = f'[project.optional-dependencies]\n"{declared}" = ["demo-pkg>=1.2.3"]\n'
    group_block = f'[dependency-groups]\n"{declared}" = ["demo-pkg>=1.2.3"]\n'
    path.write_text('[project]\ndependencies = ["untouched>=1"]\n' + extra_block + group_block)
    original = path.read_bytes()
    flags = [option, selector, "-u"] if upgrade else [option, selector, "--format", "json"]
    result = runner.invoke(cli.app, flags)
    assert result.exit_code == 0, result.output
    assert calls == ["demo-pkg"]
    if upgrade:
        block = group_block if option == "--group" else extra_block
        assert path.read_bytes() == original.replace(block.encode(), block.replace("1.2.3", "2").encode())
    else:
        updates = json.loads(result.stdout)["updates"]
        assert len(updates) == 1
        section = (
            ["dependency-groups", declared] if option == "--group" else ["project", "optional-dependencies", declared]
        )
        assert updates[0]["section"] == section
        assert path.read_bytes() == original
