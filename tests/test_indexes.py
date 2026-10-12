"""Index, authentication, cache, and retry tests without external requests."""

import base64
import io
import json
from email.message import Message
from http.client import HTTPMessage
from pathlib import Path
from urllib.error import HTTPError
from urllib.error import URLError
from urllib.request import Request

import pytest
from packaging.version import Version
from typer.testing import CliRunner

from check_updates import cli
from check_updates import indexes
from check_updates.indexes import Index
from check_updates.indexes import Release
from check_updates.indexes import ReleaseClient
from check_updates.workspace import package_indexes
from check_updates.workspace import read_project

runner = CliRunner()


class Response(io.BytesIO):
    headers = Message()


def http_error(code, url="https://example.org/simple/demo/"):
    return HTTPError(url, code, "failure", Message(), None)


@pytest.mark.parametrize("content_type", ["application/vnd.pypi.simple.v1+json", "text/html"])
def test_simple_json_and_html_filter_versions_and_metadata(monkeypatch, content_type):
    files = [
        {"filename": "demo-1.0.tar.gz", "requires-python": ">=3.10"},
        {"filename": "demo-1.9-py3-none-any.whl", "requires-python": ">=3.12"},
        {"filename": "demo-2.0.tar.gz", "yanked": True},
        {"filename": "demo-2.1.tar.gz", "yanked": ""},
        {"filename": "demo-3.0rc1.tar.gz"},
        {"filename": "demo-4.0.dev1.tar.gz"},
        {"filename": "other-5.0.tar.gz"},
        {"filename": "junk"},
    ]
    if "json" in content_type:
        body = json.dumps({"files": files}).encode()
    else:
        body = "".join(
            f'<a href="../files/{f["filename"]}#hash" '
            + (
                f'data-requires-python="{str(f["requires-python"]).replace(">", "&gt;")}" '
                if "requires-python" in f
                else ""
            )
            + ('data-yanked="" ' if "yanked" in f else "")
            + ">file</a>"
            for f in files
        ).encode()
    monkeypatch.setattr(ReleaseClient, "_read", lambda self, req: (body, content_type))
    found = ReleaseClient().get("Demo", (Index("https://example.org/simple", default=True),))
    assert found == [Release(Version("1.0"), ">=3.10"), Release(Version("1.9"), ">=3.12")]


def test_first_index_does_not_merge_versions(monkeypatch):
    seen = []

    def read(self, req):
        seen.append(req.full_url)
        return b'{"files": [{"filename": "demo-1.0.tar.gz"}]}', "application/json"

    monkeypatch.setattr(ReleaseClient, "_read", read)
    client = ReleaseClient()
    sources = (Index("https://one.example/simple"), Index("https://two.example/simple"))
    assert client.get("demo", sources) == [Release(Version("1"))]
    assert seen == ["https://one.example/simple/demo/"]
    assert client.get("demo", sources) == [Release(Version("1"))]
    assert len(seen) == 1


def test_404_moves_to_next_index_and_default_blocks_public(monkeypatch):
    seen = []

    def read(self, req):
        seen.append(req.full_url)
        raise http_error(404, req.full_url)

    monkeypatch.setattr(ReleaseClient, "_read", read)
    sources = (Index("https://one.example/simple"), Index("https://two.example/simple", default=True))
    with pytest.raises(ValueError, match="configured indexes"):
        ReleaseClient().get("demo", sources)
    assert seen == ["https://one.example/simple/demo/", "https://two.example/simple/demo/"]


def test_optional_indexes_fall_back_to_public_only_on_404(monkeypatch):
    monkeypatch.setattr(ReleaseClient, "_read", lambda self, req: (_ for _ in ()).throw(http_error(404)))
    monkeypatch.setattr(ReleaseClient, "_public", lambda self, name: [Release(Version("2"))])
    assert ReleaseClient().get("demo", (Index("https://example.org/simple"),)) == [Release(Version("2"))]


def test_empty_first_index_never_falls_back(monkeypatch):
    monkeypatch.setattr(ReleaseClient, "_read", lambda self, req: (b'{"files": []}', "application/json"))
    monkeypatch.setattr(ReleaseClient, "_public", lambda self, name: pytest.fail("No fallback allowed"))
    assert ReleaseClient().get("demo", (Index("https://example.org/simple"),)) == []


@pytest.mark.parametrize("code", [401, 403, 429, 500])
def test_private_errors_do_not_try_another_source(monkeypatch, code):
    seen = []

    def read(self, req):
        seen.append(req.full_url)
        raise http_error(code)

    monkeypatch.setattr(ReleaseClient, "_read", read)
    with pytest.raises(ValueError, match=f"HTTP {code}"):
        ReleaseClient().get("demo", (Index("https://one.example/simple"), Index("https://two.example/simple")))
    assert len(seen) == 1


def test_auth_url_encoding_and_named_environment(monkeypatch):
    req = Index("https://user%40example:secret%3Avalue@example.org/simple").request("Demo_Pkg")
    assert req.full_url == "https://example.org/simple/demo-pkg/"
    assert req.get_header("Authorization") == "Basic " + base64.b64encode(b"user@example:secret:value").decode()
    monkeypatch.setenv("UV_INDEX_INTERNAL_USERNAME", "env-user")
    monkeypatch.setenv("UV_INDEX_INTERNAL_PASSWORD", "env-password")
    req = Index("https://example.org/simple", "internal").request("demo")
    assert req.get_header("Authorization") == "Basic " + base64.b64encode(b"env-user:env-password").decode()


@pytest.mark.parametrize(
    "url",
    [
        "http://user:secret@example.org/simple",
        "file:///tmp/simple",
        "https://example.org/simple?token=secret",
        "https://example.org/\x1b[31m",
    ],
)
def test_invalid_or_insecure_indexes_are_rejected_without_network(monkeypatch, url):
    monkeypatch.setattr(ReleaseClient, "_read", lambda *args: pytest.fail("No network expected"))
    with pytest.raises(ValueError):
        ReleaseClient().get("demo", (Index(url),))


@pytest.mark.parametrize("failure", ["http", "network", "json", "config"])
def test_cli_private_secret_redaction_even_in_debug(tmp_path, monkeypatch, failure):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    path.write_text('[project]\ndependencies = ["demo>=1"]\n')
    before = path.read_bytes()
    secret = "never-print-this-token"
    url = f"https://user:{secret}@example.org/simple"
    if failure == "config":
        url = f"http://user:{secret}@example.org/simple"

    def read(self, req):
        if failure == "http":
            raise http_error(403, url)
        if failure == "network":
            raise URLError(f"failed {url}")
        return (f'{{"secret": "{secret}"}}'.encode(), "application/json")

    monkeypatch.setattr(ReleaseClient, "_read", read)
    result = runner.invoke(cli.app, ["--index", url, "--debug", "-u"])
    assert result.exit_code == 1, result.output
    assert secret not in result.output
    assert "user:" not in result.output
    assert "Traceback" in result.stderr
    assert path.read_bytes() == before


def test_private_results_are_never_persisted(tmp_path, monkeypatch):
    monkeypatch.setattr(
        ReleaseClient, "_read", lambda self, req: (b'{"files": [{"filename": "demo-2.tar.gz"}]}', "application/json")
    )
    client = ReleaseClient()
    key = ("demo", (Index("https://user:secret@example.org/simple", default=True),))
    assert client.get(*key) == [Release(Version("2"))]
    assert client.links[key] == "https://example.org/simple/demo/"
    assert not (tmp_path / "cache").exists()


def test_public_cache_hit_expiry_corruption_and_bypass(tmp_path, monkeypatch):
    calls = []
    clock = [10000]
    monkeypatch.setattr(indexes.time, "time", lambda: clock[0])

    def read(self, req):
        calls.append(req.full_url)
        return (
            b'{"releases": {"2": [{"requires_python": ">=3.10"}]}, "info": {"project_urls": {"Changelog": "https://example.org/releases"}}}',
            "application/json",
        )

    monkeypatch.setattr(ReleaseClient, "_read", read)
    expected = [Release(Version("2"), ">=3.10")]
    assert ReleaseClient().get("Demo") == expected
    cached = ReleaseClient()
    assert cached.get("demo") == expected
    assert cached.notes["demo"] == "https://example.org/releases"
    assert len(calls) == 1
    clock[0] += 3601
    assert ReleaseClient().get("demo") == expected
    assert len(calls) == 2
    cache = indexes._cache_path("demo")
    cache.write_text("broken")
    assert ReleaseClient().get("demo") == expected
    assert len(calls) == 3
    before = cache.read_bytes()
    assert ReleaseClient(no_cache=True).get("demo") == expected
    assert len(calls) == 4
    assert cache.read_bytes() == before


def test_cache_write_failure_is_nonfatal(monkeypatch):
    monkeypatch.setattr(ReleaseClient, "_read", lambda self, req: (b'{"releases": {"2": [{}]}}', "application/json"))
    monkeypatch.setattr(Path, "mkdir", lambda *args, **kwargs: (_ for _ in ()).throw(OSError("denied")))
    assert ReleaseClient().get("demo") == [Release(Version("2"))]


@pytest.mark.parametrize("error", [http_error(429), http_error(503), URLError("offline"), TimeoutError("slow")])
def test_transient_requests_retry_twice_with_bounded_delay(monkeypatch, error):
    attempts = []
    delays = []
    monkeypatch.setattr(indexes.time, "sleep", delays.append)
    client = ReleaseClient()

    def open_request(req, timeout):
        attempts.append(timeout)
        if len(attempts) < 3:
            raise error
        return Response(b"done")

    monkeypatch.setattr(client.opener, "open", open_request)
    assert client._read(Request("https://example.org/")) == (b"done", "")
    assert attempts == [15, 15, 15]
    assert delays == [0.25, 0.5]


@pytest.mark.parametrize("code", [401, 403, 404])
def test_permanent_request_errors_are_not_retried(monkeypatch, code):
    monkeypatch.setattr(indexes.time, "sleep", lambda *args: pytest.fail("Do not retry"))
    client = ReleaseClient()
    monkeypatch.setattr(client.opener, "open", lambda *args, **kwargs: (_ for _ in ()).throw(http_error(code)))
    with pytest.raises(HTTPError):
        client._read(Request("https://example.org/"))


def test_retry_exhaustion_is_finite(monkeypatch):
    attempts = []
    monkeypatch.setattr(indexes.time, "sleep", lambda *args: None)
    client = ReleaseClient()

    def fail(*args, **kwargs):
        attempts.append(1)
        raise URLError("offline")

    monkeypatch.setattr(client.opener, "open", fail)
    with pytest.raises(URLError):
        client._read(Request("https://example.org/"))
    assert len(attempts) == 3


@pytest.mark.parametrize(
    "destination",
    ["https://other.org/simple/", "http://example.org/simple/", "https://user:secret@example.org/simple/"],
)
def test_redirect_never_forwards_credentials_to_another_origin(destination):
    req = Index("https://user:secret@example.org/simple").request("demo")
    with pytest.raises(URLError):
        indexes._Redirects().redirect_request(req, io.BytesIO(), 302, "redirect", HTTPMessage(), destination)


def test_same_origin_redirect_keeps_auth():
    req = Index("https://user:secret@example.org/simple").request("demo")
    redirected = indexes._Redirects().redirect_request(
        req, io.BytesIO(), 302, "redirect", HTTPMessage(), "https://example.org/new/"
    )
    assert redirected.get_header("Authorization") == req.get_header("Authorization")


def test_named_index_routing_member_override_explicit_and_default(tmp_path):
    root = tmp_path / "pyproject.toml"
    root.write_text(
        '[[tool.uv.index]]\nname = "internal"\nurl = "https://root.example/simple"\nexplicit = true\n'
        '[[tool.uv.index]]\nname = "fallback"\nurl = "https://fallback.example/simple"\ndefault = true\n'
        '[tool.uv.sources]\ndemo = {index = "internal"}\n'
    )
    child = tmp_path / "child.toml"
    child.write_text('[[tool.uv.index]]\nname = "internal"\nurl = "https://member.example/simple"\nexplicit = true\n')
    a, b = read_project(root), read_project(child)
    assert package_indexes(a, b, "demo", []) == (Index("https://member.example/simple", "internal", default=True),)
    assert package_indexes(a, b, "other", []) == (Index("https://fallback.example/simple", "fallback", default=True),)
    assert package_indexes(a, b, "demo", ["https://cli.example/simple"]) == (
        Index("https://cli.example/simple", default=True),
    )


def test_workspace_same_name_different_indexes_have_separate_cache(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = tmp_path / "pyproject.toml"
    root.write_text(
        '[project]\ndependencies = ["demo>=1"]\n[tool.uv.workspace]\nmembers = ["member"]\n'
        '[[tool.uv.index]]\nname = "internal"\nurl = "https://root.example/simple"\ndefault = true\n'
    )
    member = tmp_path / "member"
    member.mkdir()
    (member / "pyproject.toml").write_text(
        '[project]\ndependencies = ["demo>=1"]\n[[tool.uv.index]]\n'
        'name = "internal"\nurl = "https://member.example/simple"\ndefault = true\n'
    )
    seen = []

    def read(self, req):
        seen.append(req.full_url)
        version = "2" if "root.example" in req.full_url else "3"
        return json.dumps({"files": [{"filename": f"demo-{version}.tar.gz"}]}).encode(), "application/json"

    monkeypatch.setattr(ReleaseClient, "_read", read)
    result = runner.invoke(cli.app, ["-w", "--format", "json"])
    assert result.exit_code == 0, result.output
    assert [u["version"] for u in json.loads(result.stdout)["updates"]] == ["2", "3"]
    assert len(seen) == 2


@pytest.mark.parametrize(
    "config",
    [
        '[tool.uv]\nindex = "invalid"\n',
        '[[tool.uv.index]]\nname = "internal"\n',
        '[[tool.uv.index]]\nname = 3\nurl = "https://example.org/simple"\n',
        '[tool.uv.sources]\ndemo = {index = "undefined"}\n',
        '[tool.uv.sources]\ndemo = {index = "internal", marker = "python_version < \'3.12\'"}\n',
    ],
)
def test_invalid_index_configuration_is_actionable_and_read_only(tmp_path, monkeypatch, config):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    path.write_text('[project]\ndependencies = ["demo>=1"]\n' + config)
    before = path.read_bytes()
    monkeypatch.setattr(ReleaseClient, "_read", lambda *args: pytest.fail("No lookup expected"))
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 1, result.output
    assert "Error:" in result.stderr
    assert path.read_bytes() == before


@pytest.mark.parametrize("payload", [b"[]", b'{"releases": []}', b'{"releases": {"1": null}}'])
def test_invalid_public_metadata_fails_without_raw_crash(monkeypatch, payload):
    monkeypatch.setattr(ReleaseClient, "_read", lambda self, req: (payload, "application/json"))
    with pytest.raises(TypeError, match="Invalid PyPI"):
        ReleaseClient(no_cache=True).get("demo")


@pytest.mark.parametrize("payload", [b"[]", b'{"files": null}', b'{"files": ["bad"]}', b"not JSON"])
def test_invalid_private_metadata_has_sanitized_error(monkeypatch, payload):
    monkeypatch.setattr(ReleaseClient, "_read", lambda self, req: (payload, "application/json"))
    with pytest.raises(ValueError, match="Invalid Simple index metadata"):
        ReleaseClient().get("demo", (Index("https://example.org/simple"),))


def test_cli_private_success_has_safe_link_and_keeps_files(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    path.write_text(
        '[project]\nrequires-python = ">=3.10"\ndependencies = ["demo>=1"]\n'
        '[[tool.uv.index]]\nname = "internal"\nurl = "https://example.org/simple"\nexplicit = true\n'
        '[tool.uv.sources]\ndemo = {index = "internal"}\n'
    )
    monkeypatch.setenv("UV_INDEX_INTERNAL_USERNAME", "user")
    monkeypatch.setenv("UV_INDEX_INTERNAL_PASSWORD", "secret")
    before = path.read_bytes()

    def read(self, req):
        assert req.get_header("Authorization") == "Basic " + base64.b64encode(b"user:secret").decode()
        return b'{"files": [{"filename": "demo-2.tar.gz", "requires-python": ">=3.10"}]}', "application/json"

    monkeypatch.setattr(ReleaseClient, "_read", read)
    result = runner.invoke(cli.app, ["--format", "json", "--check", "--debug"])
    assert result.exit_code == 3, result.output
    record = json.loads(result.stdout)["updates"][0]
    assert record["release_url"] == "https://example.org/simple/demo/"
    assert record["project_release_url"] is None
    assert "secret" not in result.output
    assert path.read_bytes() == before
    assert not (tmp_path / "cache").exists()


def test_private_failure_after_success_does_not_write(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    path.write_text('[project]\ndependencies = ["demo>=1", "other>=1"]\n')
    before = path.read_bytes()

    def read(self, req):
        if "/other/" in req.full_url:
            raise http_error(403)
        return b'{"files": [{"filename": "demo-2.tar.gz"}]}', "application/json"

    monkeypatch.setattr(ReleaseClient, "_read", read)
    result = runner.invoke(cli.app, ["--index", "https://example.org/simple", "-u"])
    assert result.exit_code == 1
    assert path.read_bytes() == before


@pytest.mark.parametrize(
    "url",
    [
        "https://user:secret@example.org/notes",
        "javascript:alert(1)",
        "https://example.org/notes?token=secret",
        "https://example.org/\x1b[31m",
    ],
)
def test_untrusted_project_release_links_are_not_displayed(url):
    assert indexes._release_notes({"Changelog": url}) is None


@pytest.mark.parametrize(
    ("name", "key"),
    [
        ("internal.proxy", "INTERNAL_PROXY"),
        ("internal-proxy", "INTERNAL_PROXY"),
        ("internal.proxy-cache_2", "INTERNAL_PROXY_CACHE_2"),
    ],
)
def test_credential_keys_replace_all_non_alphanumeric_characters(monkeypatch, name, key):
    monkeypatch.setenv(f"UV_INDEX_{key}_USERNAME", "user")
    monkeypatch.setenv(f"UV_INDEX_{key}_PASSWORD", "secret")
    request = Index("https://example.org/simple", name).request("demo")
    assert request.get_header("Authorization") == "Basic " + base64.b64encode(b"user:secret").decode()


@pytest.mark.parametrize(
    ("original", "expected"),
    [
        ("demo", "demo>=1.9"),
        ("demo>=1", "demo>=1.9"),
        ("demo~=1.0", "demo~=1.9"),
        ("demo>=1,<3", "demo>=1.9,<3"),
        ("demo==1", "demo==2.0+internal"),
    ],
)
@pytest.mark.parametrize("distribution", ["tar.gz", "whl"])
def test_private_local_builds_do_not_write_invalid_requirements(
    tmp_path, monkeypatch, original, expected, distribution
):
    import tomlkit
    from packaging.requirements import Requirement

    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    path.write_text("[project]\ndependencies = [" + json.dumps(original) + "]\n")
    suffix = "-py3-none-any.whl" if distribution == "whl" else ".tar.gz"
    body = json.dumps(
        {"files": [{"filename": f"demo-{version}{suffix}"} for version in ("1.9", "2.0+internal")]}
    ).encode()
    monkeypatch.setattr(ReleaseClient, "_read", lambda self, req: (body, "application/json"))
    result = runner.invoke(cli.app, ["--index", "https://example.org/simple", "-u"])
    assert result.exit_code == 0, result.output
    written = tomlkit.parse(path.read_text())["project"]["dependencies"][0]
    assert written == expected
    assert Requirement(written).name == "demo"


@pytest.mark.parametrize("with_other", [False, True])
def test_explicit_default_blocks_public_even_when_no_index_is_eligible(tmp_path, monkeypatch, with_other):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    content = (
        '[project]\ndependencies = ["demo>=1"]\n[[tool.uv.index]]\nname = "private"\n'
        'url = "https://private.example/simple"\nexplicit = true\ndefault = true\n'
    )
    if with_other:
        content += '[[tool.uv.index]]\nname = "other"\nurl = "https://other.example/simple"\n'
    path.write_text(content)
    before = path.read_bytes()
    seen = []

    def read(self, req):
        seen.append(req.full_url)
        assert req.full_url == "https://other.example/simple/demo/"
        raise http_error(404, req.full_url)

    monkeypatch.setattr(ReleaseClient, "_read", read)
    monkeypatch.setattr(ReleaseClient, "_public", lambda *args: pytest.fail("Public PyPI must remain disabled"))
    result = runner.invoke(cli.app, ["-u", "--debug"])
    assert result.exit_code == 1, result.output
    assert "not found on configured indexes" in result.stderr
    assert seen == (["https://other.example/simple/demo/"] if with_other else [])
    assert path.read_bytes() == before
    assert not (tmp_path / "cache").exists()


def test_explicit_default_named_source_can_still_use_its_index(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    path.write_text(
        '[project]\ndependencies = ["demo>=1"]\n[[tool.uv.index]]\nname = "private"\n'
        'url = "https://private.example/simple"\nexplicit = true\ndefault = true\n'
        '[tool.uv.sources]\ndemo = {index = "private"}\n'
    )
    seen = []

    def read(self, req):
        seen.append(req.full_url)
        return b'{"files": [{"filename": "demo-2.tar.gz"}]}', "application/json"

    monkeypatch.setattr(ReleaseClient, "_read", read)
    result = runner.invoke(cli.app, ["--format", "json"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["updates"][0]["version"] == "2"
    assert seen == ["https://private.example/simple/demo/"]


@pytest.mark.parametrize("named", [False, True])
@pytest.mark.parametrize("credential_source", ["url", "environment"])
def test_authenticate_never_rejects_credentials_before_network(tmp_path, monkeypatch, named, credential_source):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    secret = "must-not-leak"
    url = f"https://user:{secret}@example.org/simple" if credential_source == "url" else "https://example.org/simple"
    content = (
        '[project]\ndependencies = ["demo>=1"]\n[[tool.uv.index]]\nname = "private"\n'
        f'url = "{url}"\nauthenticate = "never"\n'
    )
    if named:
        content += 'explicit = true\n[tool.uv.sources]\ndemo = {index = "private"}\n'
    path.write_text(content)
    before = path.read_bytes()
    if credential_source == "environment":
        monkeypatch.setenv("UV_INDEX_PRIVATE_USERNAME", "user")
        monkeypatch.setenv("UV_INDEX_PRIVATE_PASSWORD", secret)
    monkeypatch.setattr(ReleaseClient, "_read", lambda *args: pytest.fail("Policy errors must not send requests"))
    result = runner.invoke(cli.app, ["-u", "--debug"])
    assert result.exit_code == 1, result.output
    assert "authentication policy" in result.stderr
    assert secret not in result.output
    assert path.read_bytes() == before


def test_authenticate_never_allows_only_anonymous_requests():
    request = Index("https://example.org/simple", authenticate="never").request("demo")
    assert not request.has_header("Authorization")


def test_authenticate_always_requires_direct_credentials(monkeypatch):
    with pytest.raises(ValueError, match="requires URL or environment credentials"):
        Index("https://example.org/simple", "required", authenticate="always").request("demo")
    monkeypatch.setenv("UV_INDEX_REQUIRED_USERNAME", "user")
    monkeypatch.setenv("UV_INDEX_REQUIRED_PASSWORD", "secret")
    request = Index("https://example.org/simple", "required", authenticate="always").request("demo")
    assert request.get_header("Authorization") == "Basic " + base64.b64encode(b"user:secret").decode()


@pytest.mark.parametrize("policy", ['"sometimes"', '["never"]', "false"])
def test_invalid_authenticate_configuration_does_not_send_requests(tmp_path, monkeypatch, policy):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    path.write_text(
        '[project]\ndependencies = ["demo>=1"]\n[[tool.uv.index]]\n'
        'url = "https://example.org/simple"\n' + f"authenticate = {policy}\n"
    )
    before = path.read_bytes()
    monkeypatch.setattr(ReleaseClient, "_read", lambda *args: pytest.fail("Invalid policies must not send requests"))
    result = runner.invoke(cli.app, ["-u"])
    assert result.exit_code == 1, result.output
    assert "authenticate must be auto, always, or never" in result.stderr
    assert path.read_bytes() == before


@pytest.mark.parametrize("setting", [None, "", "relative-cache", ".", "absolute"])
def test_cache_location_uses_only_absolute_paths(tmp_path, monkeypatch, setting):
    home = tmp_path / "home"
    monkeypatch.setattr(Path, "home", lambda: home)
    if setting is None:
        monkeypatch.delenv("XDG_CACHE_HOME", raising=False)
    elif setting == "absolute":
        monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "configured-cache"))
    else:
        monkeypatch.setenv("XDG_CACHE_HOME", setting)
    root = tmp_path / "configured-cache" if setting == "absolute" else home / ".cache"
    project = tmp_path / "project"
    project.mkdir()
    monkeypatch.chdir(project)
    calls = []

    def read(self, request):
        calls.append(request.full_url)
        return b'{"releases": {"2": [{}]}}', "application/json"

    monkeypatch.setattr(ReleaseClient, "_read", read)
    cache = indexes._cache_path("demo")
    assert cache.is_absolute()
    assert cache.parent == root / "check-updates"
    assert ReleaseClient().get("demo") == [Release(Version("2"))]
    assert cache.is_file()
    assert list(project.iterdir()) == []
    other = tmp_path / "other-project"
    other.mkdir()
    monkeypatch.chdir(other)
    assert indexes._cache_path("demo") == cache
    assert ReleaseClient().get("demo") == [Release(Version("2"))]
    assert calls == ["https://pypi.org/pypi/demo/json"]
    assert list(other.iterdir()) == []


@pytest.mark.parametrize("host", ["packages.example", "[::1]"])
def test_index_port_zero_is_rejected(host):
    with pytest.raises(ValueError, match="Index port must be between 1 and 65535"):
        Index(f"https://user:secret@{host}:0/simple").request("demo")


@pytest.mark.parametrize("port", [1, 443, 8443, 65535])
def test_nonzero_index_ports_are_preserved(port):
    request = Index(f"https://user:secret@packages.example:{port}/simple").request("demo")
    assert request.full_url == f"https://packages.example:{port}/simple/demo/"
    assert request.get_header("Authorization") == "Basic " + base64.b64encode(b"user:secret").decode()


@pytest.mark.parametrize("credential_source", ["url", "environment"])
def test_zero_port_does_not_send_credentials_or_write_files(tmp_path, monkeypatch, credential_source):
    monkeypatch.chdir(tmp_path)
    path = tmp_path / "pyproject.toml"
    secret = "zero-port-secret"
    url = (
        f"https://user:{secret}@packages.example:0/simple"
        if credential_source == "url"
        else "https://packages.example:0/simple"
    )
    path.write_text(
        f'[project]\ndependencies = ["demo>=1"]\n[[tool.uv.index]]\nname = "private"\nurl = "{url}"\ndefault = true\n'
    )
    before = path.read_bytes()
    if credential_source == "environment":
        monkeypatch.setenv("UV_INDEX_PRIVATE_USERNAME", "user")
        monkeypatch.setenv("UV_INDEX_PRIVATE_PASSWORD", secret)
    monkeypatch.setattr(ReleaseClient, "_read", lambda *args: pytest.fail("Port zero must not send a request"))
    result = runner.invoke(cli.app, ["-u", "--debug"])
    assert result.exit_code == 1, result.output
    assert "Invalid index URL" in result.stderr
    assert secret not in result.output
    assert path.read_bytes() == before
