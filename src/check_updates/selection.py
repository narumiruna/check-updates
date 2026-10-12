"""Filter dependency occurrences and choose candidates without resolving packages."""

from dataclasses import dataclass
from dataclasses import field

from packaging.requirements import Requirement
from packaging.specifiers import InvalidSpecifier
from packaging.specifiers import SpecifierSet
from packaging.utils import canonicalize_name
from packaging.version import Version

from check_updates.dependencies import Dependency
from check_updates.dependencies import baseline
from check_updates.indexes import Release


@dataclass
class Selection:
    target: str = "latest"
    include: set[str] = field(default_factory=set)
    exclude: set[str] = field(default_factory=set)
    groups: set[str] = field(default_factory=set)
    extras: set[str] = field(default_factory=set)
    runtime: bool = False

    def accepts(self, dependency: Dependency, requirement: Requirement) -> bool:
        name = canonicalize_name(requirement.name)
        if name in self.exclude or (self.include and name not in self.include):
            return False
        if not (self.groups or self.extras or self.runtime):
            return True
        section = dependency.section
        return (
            (self.runtime and section == ("project", "dependencies"))
            or (section[0] == "dependency-groups" and section[-1] in self.groups)
            or (section[:2] == ("project", "optional-dependencies") and section[-1] in self.extras)
        )


def python_baseline(value: str | None) -> Version | None:
    """Use a declared inclusive floor; never infer an interpreter from the host."""
    if not value:
        return None
    specs = list(SpecifierSet(value))
    floors = [Version(spec.version) for spec in specs if spec.operator in {">=", "~="}]
    if not floors or any(spec.operator not in {">=", "~=", "<", "<=", "!="} for spec in specs):
        msg = "requires-python filtering needs an inclusive >= or ~= floor; unsupported Python range."
        raise ValueError(msg)
    floor = max(floors)
    if floor.is_prerelease or floor.is_postrelease or floor.local or floor not in SpecifierSet(value):
        msg = "requires-python floor must be a final version admitted by the declared range."
        raise ValueError(msg)
    return floor


def choose_version(
    requirement: Requirement, releases: list[Release], target: str, python: Version | None
) -> Version | None:
    spec = baseline(requirement)
    before = Version(spec.version) if spec else None
    limits = SpecifierSet(",".join(str(s) for s in requirement.specifier if s.operator in {"<", "<=", "!="}))
    candidates = []
    for release in releases:
        version = release.version
        # The target must be newer and valid for the output operator.
        if (before is not None and version <= before) or (version.local and (spec is None or spec.operator != "==")):
            continue
        if version not in limits:
            continue
        if target != "latest":
            if before is None or before.epoch != version.epoch:
                continue
            width = 2 if target == "patch" else 1
            if (*before.release, 0, 0)[:width] != (*version.release, 0, 0)[:width]:
                continue
        if python is not None and release.requires_python:
            try:
                if python not in SpecifierSet(release.requires_python):
                    continue
            except InvalidSpecifier:
                continue  # Unknown malformed metadata cannot prove baseline compatibility.
        # Compatible-release bounds move only for a single ~= requirement, as before.
        candidates.append(version)
    return max(candidates) if candidates else None


def release_url(name: str, version: Version) -> str:
    return f"https://pypi.org/project/{canonicalize_name(name)}/{version}/"
