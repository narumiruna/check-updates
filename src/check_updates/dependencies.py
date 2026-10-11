"""Read dependencies and find stable PyPI updates without resolving a project."""

import json
import re
from collections.abc import Iterator
from dataclasses import dataclass
from urllib.parse import quote
from urllib.request import urlopen

import tomlkit
from packaging.requirements import Requirement
from packaging.utils import canonicalize_name
from packaging.version import InvalidVersion
from packaging.version import Version
from tomlkit.items import Array
from tomlkit.items import String
from tomlkit.toml_document import TOMLDocument


@dataclass
class Dependency:
    array: Array
    index: int
    requirement: str
    section: tuple[str, ...]


@dataclass
class Update:
    dependency: Dependency
    replacement: str
    version: Version


def version_change(update: Update) -> str:
    """Describe the requirement baseline, not compatibility or an installed version."""
    specs = list(Requirement(update.dependency.requirement).specifier)
    if not specs:
        return "unbounded"
    before = Version(specs[0].version)
    after = update.version
    if before.epoch != after.epoch or any(
        version.is_prerelease or version.is_postrelease or version.local or len(version.release) > 3
        for version in (before, after)
    ):
        return "other"
    old = (*before.release, 0, 0)[:3]
    new = (*after.release, 0, 0)[:3]
    return next((label for index, label in enumerate(("major", "minor", "patch")) if old[index] != new[index]), "other")


def dependencies(document: TOMLDocument) -> Iterator[Dependency]:
    """Visit package requirements, excluding dependency-group include tables."""
    project = document.get("project", {})
    arrays = [(("project", "dependencies"), project.get("dependencies", []))]
    arrays.extend(
        (("project", "optional-dependencies", name), array)
        for name, array in project.get("optional-dependencies", {}).items()
    )
    arrays.extend((("dependency-groups", name), array) for name, array in document.get("dependency-groups", {}).items())
    for section, array in arrays:
        if not isinstance(array, Array):
            continue
        for index, item in enumerate(array):
            if isinstance(item, str):
                yield Dependency(array, index, str(item), section)


def latest_version(name: str) -> Version:
    """Return the highest stable release with at least one non-yanked file."""
    url = f"https://pypi.org/pypi/{quote(canonicalize_name(name), safe='')}/json"
    with urlopen(url, timeout=15) as response:
        data = json.load(response)
    versions = []
    for value, files in data["releases"].items():
        try:
            version = Version(value)
        except InvalidVersion:
            continue
        if version.is_prerelease or version.is_devrelease:
            continue
        if any(not file.get("yanked", False) for file in files):
            versions.append(version)
    if not versions:
        msg = f"No stable, non-yanked release found for {name}."
        raise ValueError(msg)
    return max(versions)


def replacement(requirement: Requirement, original: str, latest: Version) -> str | None:
    """Replace a single lower bound or exact pin, or add a bare-name lower bound."""
    specs = list(requirement.specifier)
    if not specs:
        before, separator, marker = original.partition(";")
        return f"{before.rstrip()}>={latest}{' ' + separator + marker if separator else ''}"
    spec = specs[0]
    if latest <= Version(spec.version):
        return None
    pattern = rf"({re.escape(spec.operator)}\s*){re.escape(spec.version)}(?=\s|;|\)|$)"
    return re.sub(pattern, lambda match: f"{match[1]}{latest}", original, count=1)


def unsupported_reason(requirement: Requirement) -> str | None:
    if requirement.url:
        return "URL dependency"
    specs = list(requirement.specifier)
    if len(specs) > 1:
        return "multiple version constraints"
    if specs and (specs[0].operator not in {"==", ">=", "~="} or "*" in specs[0].version):
        return "unsupported version constraint"
    return None


def apply_update(update: Update) -> None:
    """Keep the original TOML string's quote style."""
    item = update.dependency.array[update.dependency.index]
    assert isinstance(item, String)
    serialized = item.as_string()
    update.dependency.array[update.dependency.index] = tomlkit.string(
        update.replacement,
        literal=serialized.startswith("'"),
        multiline=serialized.startswith(('"""', "'''")),
    )
