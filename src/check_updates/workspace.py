"""Discover uv workspace projects without invoking uv."""

from collections.abc import Mapping
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

import tomlkit
from packaging.utils import canonicalize_name
from tomlkit.exceptions import ParseError
from tomlkit.toml_document import TOMLDocument


@dataclass
class Project:
    path: Path
    document: TOMLDocument


def read_project(path: Path) -> Project:
    try:
        return Project(path, tomlkit.parse(path.read_bytes().decode("utf-8")))
    except (OSError, ValueError, ParseError) as error:
        msg = f"Cannot read {path}: {error}"
        raise ValueError(msg) from error


def _uv_config(project: Project) -> Mapping:
    return project.document.get("tool", {}).get("uv", {})


def _patterns(config: Mapping, key: str, *, required: bool = False) -> list[str]:
    value = config.get(key, None if required else [])
    if not isinstance(value, Sequence) or isinstance(value, str) or not all(isinstance(item, str) for item in value):
        msg = f"tool.uv.workspace.{key} must be an array of glob strings."
        raise ValueError(msg)
    return list(value)


def load_projects(workspace: bool) -> list[Project]:
    """Find the nearest workspace root and read its declared members."""
    if not workspace:
        return [read_project(Path("pyproject.toml"))]
    current = Path.cwd()
    for directory in (current, *current.parents):
        path = directory / "pyproject.toml"
        if not path.is_file():
            continue
        root = read_project(path)
        config = _uv_config(root).get("workspace")
        if config is not None:
            break
    else:
        msg = "No [tool.uv.workspace] found in the current directory or its parents."
        raise ValueError(msg)
    if not isinstance(config, Mapping):
        msg = "tool.uv.workspace must be a table."
        raise TypeError(msg)
    members = _patterns(config, "members", required=True)
    exclusions = _patterns(config, "exclude")
    excluded = {path.resolve() for pattern in exclusions for path in directory.glob(pattern)}
    paths = {root.path.resolve()}
    for pattern in members:
        matches = list(directory.glob(pattern))
        if not matches and not any(character in pattern for character in "*?["):
            msg = f"Workspace member not found: {directory / pattern}"
            raise ValueError(msg)
        for member in matches:
            if member.resolve() not in excluded:
                paths.add((member / "pyproject.toml").resolve())
    paths.discard(root.path.resolve())
    return [root, *(read_project(path) for path in sorted(paths))]


def workspace_dependencies(root: Project, project: Project) -> set[str]:
    """Respect root sources and member overrides for workspace dependencies."""
    sources = dict(_uv_config(root).get("sources", {}))
    sources.update(_uv_config(project).get("sources", {}))
    names = set()
    for name, source in sources.items():
        entries = source if isinstance(source, list) else [source]
        if any(isinstance(entry, Mapping) and entry.get("workspace") for entry in entries):
            names.add(canonicalize_name(name))
    return names
