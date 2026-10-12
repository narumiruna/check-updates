"""Read dependencies and find stable PyPI updates without resolving a project."""

import re
from collections.abc import Iterator
from dataclasses import dataclass

import tomlkit
from packaging.requirements import Requirement
from packaging.specifiers import Specifier
from packaging.version import Version
from tomlkit.items import Array
from tomlkit.items import String
from tomlkit.toml_document import TOMLDocument

from check_updates.indexes import ReleaseClient


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
    release_link: str | None = None
    project_link: str | None = None


def version_change(update: Update) -> str:
    """Describe the requirement baseline, not compatibility or an installed version."""
    spec = baseline(Requirement(update.dependency.requirement))
    if spec is None:
        return "unbounded"
    before = Version(spec.version)
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
    """Return the latest stable version without a persistent cache."""
    releases = ReleaseClient(no_cache=True).get(name)
    if not releases:
        msg = f"No stable, non-yanked release found for {name}."
        raise ValueError(msg)
    return max(release.version for release in releases)


def baseline(requirement: Requirement) -> Specifier | None:
    return next((spec for spec in requirement.specifier if spec.operator in {">=", "==", "~="}), None)


def replacement(requirement: Requirement, original: str, latest: Version) -> str | None:
    """Raise the baseline without changing upper bounds, exclusions, or formatting."""
    spec = baseline(requirement)
    if spec is None:
        before, separator, marker = original.partition(";")
        return f"{before.rstrip()}>={latest}{' ' + separator + marker if separator else ''}"
    if latest <= Version(spec.version):
        return None
    pattern = rf"({re.escape(spec.operator)}\s*){re.escape(spec.version)}(?=\s|,|;|\)|$)"
    return re.sub(pattern, lambda match: f"{match[1]}{latest}", original, count=1)


def unsupported_reason(requirement: Requirement) -> str | None:
    if requirement.url:
        return "URL dependency"
    specs = list(requirement.specifier)
    if len(specs) > 1:
        if (
            sum(spec.operator == ">=" for spec in specs) == 1
            and all(spec.operator in {">=", "<", "<=", "!="} for spec in specs)
            and not any("*" in spec.version for spec in specs if spec.operator == ">=")
        ):
            return None
        return "unsupported composite version constraints"
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
