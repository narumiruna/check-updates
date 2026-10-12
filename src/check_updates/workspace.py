"""Discover uv workspace projects without invoking uv."""

import logging
from collections.abc import Mapping
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

import tomlkit
from packaging.utils import canonicalize_name
from tomlkit.exceptions import ParseError
from tomlkit.toml_document import TOMLDocument

from check_updates.indexes import Index

logger = logging.getLogger(__name__)


@dataclass
class Project:
    path: Path
    document: TOMLDocument
    original: bytes


def read_project(path: Path) -> Project:
    logger.debug("Reading project file %s", path)
    try:
        original = path.read_bytes()
        project = Project(path, tomlkit.parse(original.decode("utf-8")), original)
    except (OSError, ValueError, ParseError) as error:
        msg = f"Cannot read {path}: {error}"
        raise ValueError(msg) from error
    else:
        logger.debug("Parsed %s (%d bytes)", path, len(original))
        return project


def _uv_config(project: Project) -> Mapping:
    tool = project.document.get("tool", {})
    if not isinstance(tool, Mapping) or not isinstance(tool.get("uv", {}), Mapping):
        msg = "tool and tool.uv must be tables."
        raise TypeError(msg)
    return tool.get("uv", {})


def _patterns(config: Mapping, key: str, *, required: bool = False) -> list[str]:
    value = config.get(key, None if required else [])
    if not isinstance(value, Sequence) or isinstance(value, str) or not all(isinstance(item, str) for item in value):
        msg = f"tool.uv.workspace.{key} must be an array of glob strings."
        raise ValueError(msg)
    return list(value)


def load_projects(workspace: bool) -> list[Project]:
    """Find the nearest workspace root and read its declared members."""
    logger.debug("Discovering projects from %s; workspace=%s", Path.cwd(), workspace)
    if not workspace:
        return [read_project(Path("pyproject.toml"))]
    current = Path.cwd()
    for directory in (current, *current.parents):
        path = directory / "pyproject.toml"
        logger.debug("Searching for workspace configuration in %s", path)
        if not path.is_file():
            continue
        root = read_project(path)
        config = _uv_config(root).get("workspace")
        if config is not None:
            logger.debug("Found workspace root %s", path)
            break
    else:
        msg = "No [tool.uv.workspace] found in the current directory or its parents."
        raise ValueError(msg)
    if not isinstance(config, Mapping):
        msg = "tool.uv.workspace must be a table."
        raise TypeError(msg)
    members = _patterns(config, "members", required=True)
    exclusions = _patterns(config, "exclude")
    logger.debug("Workspace member patterns: %s; exclude patterns: %s", members, exclusions)
    excluded = {path.resolve() for pattern in exclusions for path in directory.glob(pattern)}
    logger.debug("Excluded workspace paths: %s", sorted(str(path) for path in excluded))
    paths = {root.path.resolve()}
    for pattern in members:
        matches = list(directory.glob(pattern))
        logger.debug("Member pattern %r matched %d paths", pattern, len(matches))
        if not matches and not any(character in pattern for character in "*?["):
            msg = f"Workspace member not found: {directory / pattern}"
            raise ValueError(msg)
        for member in matches:
            if member.resolve() not in excluded:
                paths.add((member / "pyproject.toml").resolve())
    paths.discard(root.path.resolve())
    logger.debug("Selected workspace member files: %s", sorted(str(path) for path in paths))
    return [root, *(read_project(path) for path in sorted(paths))]


def _configured_indexes(config: Mapping) -> list[Index]:
    items = config.get("index", [])
    if not isinstance(items, list):
        msg = "tool.uv.index must be an array of tables."
        raise TypeError(msg)
    result = []
    for item in items:
        if not isinstance(item, Mapping) or not isinstance(item.get("url"), str):
            msg = "Each tool.uv.index entry needs a URL string."
            raise TypeError(msg)
        if item.get("name") is not None and not isinstance(item["name"], str):
            msg = "Index names must be strings."
            raise TypeError(msg)
        if any(not isinstance(item.get(flag, False), bool) for flag in ("explicit", "default")):
            msg = "Index explicit and default values must be booleans."
            raise TypeError(msg)
        index = Index(item["url"], item.get("name"), item.get("explicit", False), item.get("default", False))
        result.append(index)
    return result


def _sources(root: Project, project: Project) -> dict:
    sources = {}
    for config in (_uv_config(root), _uv_config(project)):
        entries = config.get("sources", {})
        if not isinstance(entries, Mapping):
            msg = "tool.uv.sources must be a table."
            raise TypeError(msg)
        sources.update({canonicalize_name(name): value for name, value in entries.items()})
    return sources


def package_indexes(root: Project, project: Project, name: str, urls: list[str]) -> tuple[Index, ...]:
    """Member definitions override root names; named sources never fall back to PyPI."""
    if urls:
        return tuple(Index(url, default=True) for url in urls)
    definitions: dict[str, Index] = {}
    for config in (_uv_config(root), _uv_config(project)):
        for index in _configured_indexes(config):
            definitions[index.name or index.url] = index
    sources = _sources(root, project)
    source = sources.get(name, {})
    entries = source if isinstance(source, list) else [source]
    names = {entry["index"] for entry in entries if isinstance(entry, Mapping) and "index" in entry}
    if any("marker" in entry for entry in entries if isinstance(entry, Mapping) and "index" in entry):
        msg = f"Marker-specific index routing for {name} is not supported."
        raise ValueError(msg)
    if len(names) > 1:
        msg = f"Multiple marker-specific indexes for {name} are not supported."
        raise ValueError(msg)
    if names:
        index_name = next(iter(names))
        if index_name not in definitions:
            msg = f"Undefined named index for {name}."
            raise ValueError(msg)
        index = definitions[index_name]
        return (Index(index.url, index.name, default=True),)
    return tuple(sorted((i for i in definitions.values() if not i.explicit), key=lambda i: i.default))


def workspace_dependencies(root: Project, project: Project) -> set[str]:
    """Respect root sources and member overrides for workspace dependencies."""
    sources = _sources(root, project)
    names = set()
    for name, source in sources.items():
        entries = source if isinstance(source, list) else [source]
        if any(isinstance(entry, Mapping) and entry.get("workspace") for entry in entries):
            names.add(canonicalize_name(name))
    return names
