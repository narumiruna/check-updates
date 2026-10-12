"""Candidate policy tests independent of network and file writes."""

import pytest
from packaging.requirements import Requirement
from packaging.version import Version

from check_updates.dependencies import replacement
from check_updates.dependencies import unsupported_reason
from check_updates.indexes import Release
from check_updates.selection import choose_version
from check_updates.selection import python_baseline


def releases(*versions):
    return [Release(Version(version)) for version in versions]


@pytest.mark.parametrize(("target", "expected"), [("patch", "1.2.9"), ("minor", "1.9"), ("latest", "2.0")])
def test_target_selects_highest_allowed_not_just_global_latest(target, expected):
    found = choose_version(Requirement("demo>=1.2.3"), releases("1.2.9", "1.9", "2.0"), target, None)
    assert found == Version(expected)


@pytest.mark.parametrize(
    ("requirement", "target", "expected"),
    [
        ("demo", "patch", None),
        ("demo", "minor", None),
        ("demo", "latest", "2.0"),
        ("demo>=1.2.3", "patch", None),
        ("demo==2.0", "latest", None),
        ("demo>=3.0", "latest", None),
        ("demo>=1!1.2", "minor", None),
        ("demo>=0.2.0", "minor", "0.9"),
        ("demo>=1.2", "patch", "1.2.9"),
    ],
)
def test_target_edge_cases(requirement, target, expected):
    values = releases("2.0")
    if expected == "0.9":
        values = releases("0.9", "1.0")
    elif expected == "1.2.9":
        values = releases("1.2.9", "1.3")
    found = choose_version(Requirement(requirement), values, target, None)
    assert found == (Version(expected) if expected else None)


@pytest.mark.parametrize(
    ("requirement", "expected"),
    [
        ("demo>=1.0,<2", "1.9"),
        ("demo>=1.0,<=2", "2"),
        ("demo>=1.0,<2,!=1.9", "1.8"),
        ("demo>=1.0,<2,!=1.9.*", "1.8"),
    ],
)
def test_upper_bounds_and_exclusions_limit_candidates(requirement, expected):
    assert unsupported_reason(Requirement(requirement)) is None
    assert choose_version(Requirement(requirement), releases("1.8", "1.9", "2", "3"), "latest", None) == Version(
        expected
    )


@pytest.mark.parametrize(
    "original",
    [
        "Demo[extra] >= 1.0 , < 2 ; python_version >= '3.10'",
        "Demo (>=1.0,<2,!=1.5)",
        "Demo<2,>=1.0",
        "Demo<=2, >=1.0",
    ],
)
def test_composite_replacement_changes_only_floor(original):
    assert replacement(Requirement(original), original, Version("1.8")) == original.replace("1.0", "1.8")


@pytest.mark.parametrize("value", ["demo>=1,>=1.2", "demo==1,<2", "demo~=1.0,<2", "demo>1,<2", "demo<2,!=1"])
def test_ambiguous_constraints_still_skipped(value):
    assert unsupported_reason(Requirement(value)) is not None


def test_python_uses_per_file_metadata_and_older_compatible_release():
    values = [Release(Version("1.8"), ">=3.10"), Release(Version("2"), ">=3.12"), Release(Version("3"), "nonsense")]
    req = Requirement("demo>=1")
    assert choose_version(req, values, "latest", Version("3.10")) == Version("1.8")
    assert choose_version(req, values, "latest", Version("3.12")) == Version("2")
    assert choose_version(req, [*values, Release(Version("2"), ">=3.10")], "latest", Version("3.10")) == Version("2")
    assert choose_version(req, [Release(Version("2"))], "latest", Version("3.10")) == Version("2")
    assert choose_version(req, [Release(Version("2"), ">=3.12")], "latest", Version("3.10")) is None


@pytest.mark.parametrize(
    ("value", "expected"), [(None, None), (">=3.10,<4", "3.10"), ("~=3.10", "3.10"), (">=3.10,>=3.11", "3.11")]
)
def test_python_floor(value, expected):
    assert python_baseline(value) == (Version(expected) if expected else None)


@pytest.mark.parametrize("value", [">3.10", "<4", "==3.10.*", ">=3.10,!=3.10.*", ">=3.10rc1", ">=3.10,<3.9"])
def test_python_floor_never_guesses(value):
    with pytest.raises(ValueError):
        python_baseline(value)


@pytest.mark.parametrize("original", ["demo", "demo>=1.2.3", "demo~=1.2.3", "demo>=1.2.3,<2"])
def test_local_versions_cannot_enter_ordered_requirements(original):
    req = Requirement(original)
    found = choose_version(req, releases("1.2.4", "1.2.5+internal"), "latest", None)
    assert found == Version("1.2.4")
    assert found is not None
    updated = replacement(req, original, found)
    assert updated is not None
    assert Requirement(updated).name == "demo"
    assert choose_version(req, releases("1.2.5+internal"), "latest", None) is None


@pytest.mark.parametrize("target", ["patch", "minor", "latest"])
def test_local_exact_pins_remain_valid_for_all_targets(target):
    original = "demo==1.2.3+old"
    found = choose_version(Requirement(original), releases("1.2.4", "1.2.5+internal"), target, None)
    assert found == Version("1.2.5+internal")
    assert found is not None
    updated = replacement(Requirement(original), original, found)
    assert updated == "demo==1.2.5+internal"
    assert Requirement(updated).specifier.contains(found)
