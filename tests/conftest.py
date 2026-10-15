import subprocess
from types import SimpleNamespace

import pytest

from check_updates import cli
from check_updates import locking


@pytest.fixture(autouse=True)
def isolated_cache(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "cache"))


@pytest.fixture(autouse=True)
def forbid_uv_process(monkeypatch):
    """Tests must opt into a process mock; never resolve real dependencies."""
    monkeypatch.setattr(locking.subprocess, "run", lambda *_args, **_kwargs: pytest.fail("Unexpected uv process."))


@pytest.fixture
def lock_process(monkeypatch):
    calls = []

    def run(args, **kwargs):
        calls.append((args, kwargs))
        return subprocess.CompletedProcess(args, 0)

    monkeypatch.setattr(locking.subprocess, "run", run)
    return calls


@pytest.fixture
def terminal(monkeypatch):
    """Replace only the CLI's terminal check; prompts still use mocked boundaries."""
    streams = SimpleNamespace(
        stdin=SimpleNamespace(isatty=lambda: True),
        stdout=SimpleNamespace(isatty=lambda: True),
    )
    monkeypatch.setattr(cli, "sys", streams)
    return streams
