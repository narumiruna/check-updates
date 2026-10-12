from types import SimpleNamespace

import pytest

from check_updates import cli


@pytest.fixture(autouse=True)
def isolated_cache(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "cache"))


@pytest.fixture
def terminal(monkeypatch):
    """Replace only the CLI's terminal check; prompts still use mocked boundaries."""
    streams = SimpleNamespace(
        stdin=SimpleNamespace(isatty=lambda: True),
        stdout=SimpleNamespace(isatty=lambda: True),
    )
    monkeypatch.setattr(cli, "sys", streams)
    return streams
