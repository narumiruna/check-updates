"""Read release metadata from PyPI or first-match Simple indexes."""

import base64
import hashlib
import json
import logging
import os
import time
from dataclasses import dataclass
from html.parser import HTMLParser
from http.client import HTTPMessage
from pathlib import Path
from typing import IO
from urllib.error import HTTPError
from urllib.error import URLError
from urllib.parse import unquote
from urllib.parse import urljoin
from urllib.parse import urlsplit
from urllib.parse import urlunsplit
from urllib.request import HTTPRedirectHandler
from urllib.request import Request
from urllib.request import build_opener

from packaging.utils import InvalidSdistFilename
from packaging.utils import InvalidWheelFilename
from packaging.utils import canonicalize_name
from packaging.utils import parse_sdist_filename
from packaging.utils import parse_wheel_filename
from packaging.version import InvalidVersion
from packaging.version import Version

logger = logging.getLogger(__name__)
CACHE_TTL = 3600


@dataclass(frozen=True)
class Release:
    version: Version
    requires_python: str | None = None


@dataclass(frozen=True)
class Index:
    url: str
    name: str | None = None
    explicit: bool = False
    default: bool = False

    def request(self, package: str) -> Request:
        if not isinstance(self.url, str) or any(not c.isprintable() or c.isspace() for c in self.url):
            msg = "Invalid index URL."
            raise ValueError(msg)
        parsed = urlsplit(self.url)
        if parsed.scheme not in {"https", "http"} or not parsed.hostname or parsed.query or parsed.fragment:
            msg = "Index must be an HTTP(S) URL without a query or fragment."
            raise ValueError(msg)
        username = unquote(parsed.username or "")
        password = unquote(parsed.password or "")
        if self.name:
            prefix = "UV_INDEX_" + self.name.upper().replace("-", "_")
            username = os.environ.get(prefix + "_USERNAME", username)
            password = os.environ.get(prefix + "_PASSWORD", password)
        if (username or password) and parsed.scheme != "https":
            msg = "Authenticated indexes require HTTPS."
            raise ValueError(msg)
        host = parsed.hostname
        if ":" in host:
            host = f"[{host}]"
        if parsed.port:
            host += f":{parsed.port}"
        clean = urlunsplit((parsed.scheme, host, parsed.path.rstrip("/") + "/", "", ""))
        request = Request(urljoin(clean, canonicalize_name(package) + "/"))
        request.add_header("Accept", "application/vnd.pypi.simple.v1+json, text/html;q=0.9")
        if username or password:
            token = base64.b64encode(f"{username}:{password}".encode()).decode()
            request.add_header("Authorization", f"Basic {token}")
        return request


class _Redirects(HTTPRedirectHandler):
    def redirect_request(
        self, req: Request, fp: IO[bytes], code: int, msg: str, headers: HTTPMessage, newurl: str
    ) -> Request:
        # Fail closed: no cross-origin or HTTPS downgrade redirects, even unauthenticated.
        old, new = urlsplit(req.full_url), urlsplit(newurl)
        if (old.scheme, old.netloc) != (new.scheme, new.netloc) or new.username or new.password:
            raise URLError("Index redirect to another origin is not allowed.")
        result = super().redirect_request(req, fp, code, msg, headers, newurl)
        if result is None:
            raise URLError("Unsupported index redirect.")
        return result


class _Links(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.files: list[dict] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        values = dict(attrs)
        href = values.get("href")
        if tag == "a" and href:
            self.files.append(
                {
                    "filename": unquote(urlsplit(href).path.rsplit("/", 1)[-1]),
                    "requires-python": values.get("data-requires-python"),
                    "yanked": "data-yanked" in values,
                }
            )


def _stable(value: str) -> Version | None:
    try:
        version = Version(value)
    except InvalidVersion:
        return None
    return None if version.is_prerelease or version.is_devrelease else version


def _simple_releases(files: list[dict], name: str) -> list[Release]:
    if not isinstance(files, list) or not all(isinstance(file, dict) for file in files):
        msg = "Invalid file metadata."
        raise ValueError(msg)
    releases = []
    for file in files:
        if file.get("yanked", False) is not False:
            continue
        filename = file.get("filename", "")
        if not isinstance(filename, str):
            continue
        try:
            if filename.endswith(".whl"):
                package, version, *_rest = parse_wheel_filename(filename)
            else:
                package, version = parse_sdist_filename(filename)
        except (InvalidWheelFilename, InvalidSdistFilename):
            continue
        if canonicalize_name(package) == name and not version.is_prerelease and not version.is_devrelease:
            python = file.get("requires-python")
            if python is None or isinstance(python, str):
                releases.append(Release(version, python))
    return releases


def _release_notes(urls: dict | None) -> str | None:
    if not isinstance(urls, dict):
        return None
    for key, value in urls.items():
        if key.lower() not in {"changelog", "release notes", "releases"} or not isinstance(value, str):
            continue
        try:
            parsed = urlsplit(value)
        except ValueError:
            continue
        if (
            parsed.scheme in {"https", "http"}
            and parsed.hostname
            and not parsed.username
            and not parsed.password
            and not parsed.query
            and all(c.isprintable() and not c.isspace() for c in value)
        ):
            return value
    return None


def _cache_path(name: str) -> Path:
    root = Path(os.environ.get("XDG_CACHE_HOME", str(Path.home() / ".cache")))
    digest = hashlib.sha256(canonicalize_name(name).encode()).hexdigest()
    return root / "check-updates" / f"v1-{digest}.json"


def _public_releases(data: object, name: str) -> tuple[list[Release], str | None]:
    if not isinstance(data, dict) or not isinstance(data.get("releases"), dict):
        msg = f"Invalid PyPI release metadata for {name}."
        raise TypeError(msg)
    entries = data["releases"]
    info = data.get("info") or {}
    note = _release_notes(info.get("project_urls", {})) if isinstance(info, dict) else None
    logger.debug("Received %d release entries for %s", len(entries), name)
    releases = []
    for value, files in entries.items():
        version = _stable(value)
        if version is None:
            continue
        if not isinstance(files, list) or not all(isinstance(file, dict) for file in files):
            msg = f"Invalid PyPI file metadata for {name}."
            raise TypeError(msg)
        for file in files:
            python = file.get("requires_python")
            if not file.get("yanked", False) and (python is None or isinstance(python, str)):
                releases.append(Release(version, python))
    return releases, note


def _load_cache(path: Path) -> tuple[list[Release], str | None] | None:
    try:
        cached = json.loads(path.read_text())
        if not isinstance(cached, dict) or not isinstance(cached.get("releases"), list):
            return None
        rows = cached["releases"]
        if not all(
            isinstance(row, list)
            and len(row) == 2
            and isinstance(row[0], str)
            and (row[1] is None or isinstance(row[1], str))
            for row in rows
        ):
            return None
        if 0 <= time.time() - cached["time"] < CACHE_TTL:
            releases = [Release(Version(v), p) for v, p in rows]
            return releases, _release_notes(cached.get("project_urls", {}))
    except (OSError, ValueError, KeyError, TypeError):
        pass
    return None


class ReleaseClient:
    def __init__(self, *, no_cache: bool = False) -> None:
        self.no_cache = no_cache
        self.cache: dict[tuple[str, tuple[Index, ...]], list[Release]] = {}
        self.links: dict[tuple[str, tuple[Index, ...]], str] = {}
        self.notes: dict[str, str] = {}
        self.opener = build_opener(_Redirects())

    def _read(self, request: Request) -> tuple[bytes, str]:
        for attempt in range(3):
            try:
                with self.opener.open(request, timeout=15) as response:
                    return response.read(), response.headers.get("Content-Type", "")
            except HTTPError as error:
                if error.code not in {408, 429, 500, 502, 503, 504} or attempt == 2:
                    raise
            except (URLError, TimeoutError, ConnectionError):
                if attempt == 2:
                    raise
            logger.debug("Retrying metadata request (attempt %d of 3)", attempt + 2)
            time.sleep(0.25 * (attempt + 1))
        raise AssertionError("Unreachable retry state")

    def _public(self, name: str) -> list[Release]:
        path = _cache_path(name)
        cached = _load_cache(path) if not self.no_cache else None
        if cached is not None:
            releases, note = cached
            if note:
                self.notes[name] = note
            logger.debug("Persistent cache hit for %s", name)
            return releases
        url = f"https://pypi.org/pypi/{canonicalize_name(name)}/json"
        logger.debug("Requesting %s (timeout: 15s)", url)
        try:
            body, _ = self._read(Request(url))
        except HTTPError as error:
            msg = (
                f"Package {name} was not found on public PyPI (HTTP 404). Check the package name; "
                "it may be private or unpublished. Use --index or configure tool.uv.index for private packages. "
                "For uv workspace dependencies, declare workspace = true in [tool.uv.sources] "
                "and use -w to include workspace root sources and check members. No files were written."
                if error.code == 404
                else f"PyPI lookup failed for {name} (HTTP {error.code})."
            )
            raise ValueError(msg) from None
        releases, note = _public_releases(json.loads(body), name)
        if note:
            self.notes[name] = note
        if not self.no_cache:
            try:
                path.parent.mkdir(parents=True, exist_ok=True)
                payload = {
                    "time": time.time(),
                    "releases": [(str(r.version), r.requires_python) for r in releases],
                    "project_urls": {"Releases": note} if note else {},
                }
                # A failed or concurrent write only loses a disposable cache entry.
                path.write_text(json.dumps(payload))
            except OSError:
                logger.debug("Persistent cache unavailable for %s", name)
        return releases

    def _private(self, name: str, indexes: tuple[Index, ...]) -> tuple[list[Release] | None, str | None]:
        releases = None
        for index in indexes:
            try:
                request = index.request(name)
            except ValueError:
                msg = "Invalid index URL or insecure authentication configuration."
                raise ValueError(msg) from None
            try:
                body, content_type = self._read(request)
            except HTTPError as error:
                if error.code == 404:
                    continue
                msg = f"Index lookup failed for {name} (HTTP {error.code})."
                raise ValueError(msg) from None
            except (OSError, URLError, ValueError):
                # Do not expose private response URLs, bodies, or credentials, even in debug tracebacks.
                msg = f"Index lookup failed for {name}. Check connectivity and index credentials."
                raise ValueError(msg) from None
            try:
                if "json" in content_type.lower():
                    releases = _simple_releases(json.loads(body)["files"], name)
                else:
                    parser = _Links()
                    parser.feed(body.decode("utf-8"))
                    releases = _simple_releases(parser.files, name)
            except (ValueError, KeyError, TypeError):
                msg = f"Invalid Simple index metadata for {name}."
                raise ValueError(msg) from None
            return releases, request.full_url  # Never merge versions from different indexes.
        return None, None

    def get(self, name: str, indexes: tuple[Index, ...] = ()) -> list[Release]:
        name = canonicalize_name(name)
        key = (name, indexes)
        if key in self.cache:
            logger.debug("Release cache hit for %s", name)
            return self.cache[key]
        logger.debug("Release cache miss for %s", name)
        releases, link = self._private(name, indexes)
        if link:
            self.links[key] = link
        if releases is None:
            if any(index.default for index in indexes):
                msg = f"Package {name} was not found on configured indexes. No files were written."
                raise ValueError(msg)
            releases = self._public(name)
            self.links[key] = f"https://pypi.org/project/{name}/"
        self.cache[key] = releases
        return releases
