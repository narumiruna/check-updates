//! Filter dependency occurrences and choose releases without dependency resolution.

use anyhow::{Context, Result, bail, ensure};
use pep440_rs::{Operator, Version, VersionSpecifier, VersionSpecifiers};
use pep508_rs::Requirement;
use std::collections::HashSet;

use crate::dependencies::{Dependency, baseline, normalize, specifiers};
use crate::indexes::Release;

#[derive(Clone, Debug)]
pub struct Selection {
    pub target: String,
    pub include: HashSet<String>,
    pub exclude: HashSet<String>,
    pub groups: HashSet<String>,
    pub extras: HashSet<String>,
    pub runtime: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            target: "latest".into(),
            include: HashSet::new(),
            exclude: HashSet::new(),
            groups: HashSet::new(),
            extras: HashSet::new(),
            runtime: false,
        }
    }
}

impl Selection {
    pub fn accepts(&self, dependency: &Dependency, requirement: &Requirement) -> bool {
        fn contains(names: &HashSet<String>, name: &str) -> bool {
            names.iter().any(|candidate| normalize(candidate) == name)
        }
        let name = normalize(requirement.name.as_ref());
        if contains(&self.exclude, &name)
            || (!self.include.is_empty() && !contains(&self.include, &name))
        {
            return false;
        }
        if self.groups.is_empty() && self.extras.is_empty() && !self.runtime {
            return true;
        }
        let section: Vec<&str> = dependency.section.iter().map(String::as_str).collect();
        match section.as_slice() {
            ["project", "dependencies"] => self.runtime,
            ["dependency-groups", name] => contains(&self.groups, &normalize(name)),
            ["project", "optional-dependencies", name] => contains(&self.extras, &normalize(name)),
            _ => false,
        }
    }
}

// pep440_rs 0.7 compares wildcard prefixes without zero-padding short versions.
// PEP 440 requires that 2.0 matches 2.0.0.*, but not 2.0.1.*.
fn specifier_contains(spec: &VersionSpecifier, version: &Version) -> bool {
    if matches!(
        spec.operator(),
        Operator::EqualStar | Operator::NotEqualStar
    ) {
        let prefix = spec.version();
        let matches = prefix.epoch() == version.epoch()
            && prefix
                .release()
                .iter()
                .enumerate()
                .all(|(index, component)| {
                    *component == version.release().get(index).copied().unwrap_or(0)
                });
        return if *spec.operator() == Operator::EqualStar {
            matches
        } else {
            !matches
        };
    }
    spec.contains(version)
}

fn specifiers_contain(specs: &VersionSpecifiers, version: &Version) -> bool {
    specs.iter().all(|spec| specifier_contains(spec, version))
}

/// Use the declared inclusive Python floor. Never use the host interpreter.
pub fn python_baseline(value: Option<&str>) -> Result<Option<Version>> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let specs = value
        .parse::<VersionSpecifiers>()
        .context("Invalid requires-python range")?;
    let floor = specs
        .iter()
        .filter(|spec| {
            matches!(
                spec.operator(),
                Operator::GreaterThanEqual | Operator::TildeEqual
            )
        })
        .map(|spec| spec.version())
        .max();
    if floor.is_none()
        || specs.iter().any(|spec| {
            !matches!(
                spec.operator(),
                Operator::GreaterThanEqual
                    | Operator::TildeEqual
                    | Operator::LessThan
                    | Operator::LessThanEqual
                    | Operator::NotEqual
                    | Operator::NotEqualStar
            )
        })
    {
        bail!(
            "requires-python filtering needs an inclusive >= or ~= floor; unsupported Python range."
        );
    }
    let floor = floor.unwrap();
    ensure!(
        !floor.any_prerelease()
            && !floor.is_post()
            && !floor.is_local()
            && specifiers_contain(&specs, floor),
        "requires-python floor must be a final version admitted by the declared range."
    );
    Ok(Some(floor.clone()))
}

/// Pick the highest newer release allowed by bounds, target, and Python metadata.
/// Release discovery supplies stable, non-yanked releases.
pub fn choose_version(
    requirement: &Requirement,
    releases: &[Release],
    target: &str,
    python: Option<&Version>,
) -> Option<Version> {
    let spec = baseline(requirement);
    let before = spec.map(|spec| spec.version());
    releases
        .iter()
        .filter(|release| {
            let version = &release.version;
            if before.is_some_and(|before| version <= before)
                || (version.is_local()
                    && spec.is_none_or(|spec| *spec.operator() != Operator::Equal))
            {
                return false;
            }
            if !specifiers(requirement)
                .iter()
                .filter(|spec| {
                    matches!(
                        spec.operator(),
                        Operator::LessThan
                            | Operator::LessThanEqual
                            | Operator::NotEqual
                            | Operator::NotEqualStar
                    )
                })
                .all(|spec| specifier_contains(spec, version))
            {
                return false;
            }
            if target != "latest" {
                let Some(before) = before else {
                    return false;
                };
                if before.epoch() != version.epoch() {
                    return false;
                }
                let width = if target == "patch" { 2 } else { 1 };
                if (0..width).any(|index| {
                    before.release().get(index).copied().unwrap_or(0)
                        != version.release().get(index).copied().unwrap_or(0)
                }) {
                    return false;
                }
            }
            if let (Some(python), Some(metadata)) = (python, &release.requires_python)
                && !metadata.is_empty()
            {
                // Invalid metadata cannot prove compatibility with the Python floor.
                return metadata
                    .parse::<VersionSpecifiers>()
                    .is_ok_and(|specs| specifiers_contain(&specs, python));
            }
            true
        })
        .map(|release| &release.version)
        .max()
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependencies::{replacement, unsupported_reason};

    fn version(value: &str) -> Version {
        value.parse().unwrap()
    }

    fn releases(values: &[&str]) -> Vec<Release> {
        values
            .iter()
            .map(|value| Release {
                version: version(value),
                requires_python: None,
            })
            .collect()
    }

    fn choose(requirement: &str, values: &[&str], target: &str) -> Option<Version> {
        choose_version(
            &requirement.parse().unwrap(),
            &releases(values),
            target,
            None,
        )
    }

    #[test]
    fn targets_choose_highest_allowed_not_global_latest() {
        for (target, expected) in [("patch", "1.2.9"), ("minor", "1.9"), ("latest", "2.0")] {
            assert_eq!(
                choose("demo>=1.2.3", &["1.2.9", "1.9", "2.0"], target),
                Some(version(expected))
            );
        }
    }

    #[test]
    fn target_edge_cases() {
        for (requirement, target, values, expected) in [
            ("demo", "patch", vec!["2.0"], None),
            ("demo", "minor", vec!["2.0"], None),
            ("demo", "latest", vec!["2.0"], Some("2.0")),
            ("demo>=1.2.3", "patch", vec!["2.0"], None),
            ("demo==2.0", "latest", vec!["2.0"], None),
            ("demo>=3.0", "latest", vec!["2.0"], None),
            ("demo>=1!1.2", "minor", vec!["2.0"], None),
            ("demo>=0.2.0", "minor", vec!["0.9", "1.0"], Some("0.9")),
            ("demo>=1.2", "patch", vec!["1.2.9", "1.3"], Some("1.2.9")),
            (
                "demo>=1!1.2",
                "patch",
                vec!["1!1.2.1", "2!1.2.2"],
                Some("1!1.2.1"),
            ),
            ("demo>=1", "patch", vec!["1.0.2", "1.1"], Some("1.0.2")),
        ] {
            assert_eq!(
                choose(requirement, &values, target),
                expected.map(version),
                "{requirement} {target}"
            );
        }
        assert_eq!(choose("demo", &[], "latest"), None);
    }

    #[test]
    fn upper_bounds_and_exclusions() {
        for (requirement, expected) in [
            ("demo>=1.0,<2", "1.9"),
            ("demo>=1.0,<=2", "2"),
            ("demo>=1.0,<2,!=1.9", "1.8"),
            ("demo>=1.0,<2,!=1.9.*", "1.8"),
        ] {
            assert_eq!(unsupported_reason(&requirement.parse().unwrap()), None);
            assert_eq!(
                choose(requirement, &["1.8", "1.9", "2", "3"], "latest"),
                Some(version(expected))
            );
        }
        assert_eq!(
            choose("demo>=1,<2,!=1.9.*", &["1.9", "1.9.1", "2"], "latest"),
            None
        );
        // A single compatible-release constraint moves its implicit upper bound.
        assert_eq!(
            choose("demo~=1.2.3", &["1.9", "2"], "latest"),
            Some(version("2"))
        );
    }

    #[test]
    fn python_uses_per_file_metadata_and_all_release_files() {
        let mut values = vec![
            Release {
                version: version("1.8"),
                requires_python: Some(">=3.10".into()),
            },
            Release {
                version: version("2"),
                requires_python: Some(">=3.12".into()),
            },
            Release {
                version: version("3"),
                requires_python: Some("nonsense".into()),
            },
        ];
        let req = "demo>=1".parse().unwrap();
        assert_eq!(
            choose_version(&req, &values, "latest", Some(&version("3.10"))),
            Some(version("1.8"))
        );
        assert_eq!(
            choose_version(&req, &values, "latest", Some(&version("3.12"))),
            Some(version("2"))
        );
        values.push(Release {
            version: version("2"),
            requires_python: Some(">=3.10".into()),
        });
        assert_eq!(
            choose_version(&req, &values, "latest", Some(&version("3.10"))),
            Some(version("2"))
        );
        assert_eq!(
            choose_version(&req, &releases(&["2"]), "latest", Some(&version("3.10"))),
            Some(version("2"))
        );
        assert_eq!(
            choose_version(&req, &values[1..2], "latest", Some(&version("3.10"))),
            None
        );
        assert_eq!(
            choose_version(&req, &values, "latest", None),
            Some(version("3"))
        );
    }

    #[test]
    fn python_floor() {
        for (value, expected) in [
            (None, None),
            (Some(""), None),
            (Some(">=3.10,<4"), Some("3.10")),
            (Some("~=3.10"), Some("3.10")),
            (Some(">=3.10,>=3.11"), Some("3.11")),
            (Some(">=3.10,!=3.11.*"), Some("3.10")),
        ] {
            assert_eq!(python_baseline(value).unwrap(), expected.map(version));
        }
    }

    #[test]
    fn wildcard_prefixes_zero_pad_short_versions() {
        assert_eq!(
            choose("demo>=1,!=2.0.1.*", &["2.0", "2.0.1", "2.0.1.1"], "latest"),
            Some(version("2.0"))
        );
        assert_eq!(choose("demo>=1,!=2.0.0.*", &["2.0"], "latest"), None);
        assert_eq!(
            python_baseline(Some(">=3.10,!=3.10.1.*")).unwrap(),
            Some(version("3.10"))
        );
        assert!(python_baseline(Some(">=3.10,!=3.10.0.*")).is_err());
        let req = "demo>=1".parse().unwrap();
        let releases = vec![Release {
            version: version("2.0"),
            requires_python: Some(">=3.10,!=3.10.1.*".into()),
        }];
        assert_eq!(
            choose_version(&req, &releases, "latest", Some(&version("3.10"))),
            Some(version("2.0"))
        );
    }

    #[test]
    fn python_floor_never_guesses() {
        for value in [
            ">3.10",
            "<4",
            "==3.10.*",
            ">=3.10,!=3.10.*",
            ">=3.10rc1",
            ">=3.10,<3.9",
            ">=3.10.dev1",
            ">=3.10.post1",
            "nonsense",
        ] {
            assert!(python_baseline(Some(value)).is_err(), "{value}");
        }
    }

    #[test]
    fn local_versions_cannot_enter_ordered_requirements() {
        for original in ["demo", "demo>=1.2.3", "demo~=1.2.3", "demo>=1.2.3,<2"] {
            let req: Requirement = original.parse().unwrap();
            let found = choose(original, &["1.2.4", "1.2.5+internal"], "latest").unwrap();
            assert_eq!(found, version("1.2.4"));
            let updated = replacement(&req, original, &found).unwrap();
            assert!(updated.parse::<Requirement>().is_ok());
            assert_eq!(choose(original, &["1.2.5+internal"], "latest"), None);
        }
    }

    #[test]
    fn local_exact_pins_work_with_all_targets() {
        let original = "demo==1.2.3+old";
        let req: Requirement = original.parse().unwrap();
        for target in ["patch", "minor", "latest"] {
            let found = choose(original, &["1.2.4", "1.2.5+internal"], target).unwrap();
            assert_eq!(found, version("1.2.5+internal"));
            let updated = replacement(&req, original, &found).unwrap();
            assert_eq!(updated, "demo==1.2.5+internal");
            assert!(updated.parse::<Requirement>().is_ok());
        }
    }

    fn dependency(section: &[&str]) -> Dependency {
        Dependency {
            requirement: "Demo_Pkg>=1".into(),
            section: section.iter().map(|value| (*value).into()).collect(),
            index: 0,
        }
    }

    fn set(values: &[&str]) -> HashSet<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn package_filters_apply_before_section_filters() {
        let entry = dependency(&["project", "dependencies"]);
        let req = entry.requirement.parse().unwrap();
        let mut selection = Selection::default();
        assert!(selection.accepts(&entry, &req));
        selection.include = set(&["DEMO.pkg"]);
        assert!(selection.accepts(&entry, &req));
        selection.exclude = set(&["demo_pkg"]);
        assert!(!selection.accepts(&entry, &req));
        selection.exclude.clear();
        selection.include = set(&["absent"]);
        assert!(!selection.accepts(&entry, &req));
    }

    #[test]
    fn section_filters_are_unions_and_normalize_both_names() {
        for declared in ["qa-tools", "qa_tools", "QA.Tools"] {
            for selector in ["qa-tools", "qa_tools", "QA.Tools", "QA.._-TOOLS"] {
                let runtime = dependency(&["project", "dependencies"]);
                let group = dependency(&["dependency-groups", declared]);
                let extra = dependency(&["project", "optional-dependencies", declared]);
                let other = dependency(&["dependency-groups", "other"]);
                let req = runtime.requirement.parse().unwrap();
                let mut selection = Selection {
                    groups: set(&[selector]),
                    ..Selection::default()
                };
                assert!(selection.accepts(&group, &req));
                assert!(!selection.accepts(&runtime, &req));
                assert!(!selection.accepts(&extra, &req));
                assert!(!selection.accepts(&other, &req));
                selection.extras = set(&[selector]);
                assert!(selection.accepts(&extra, &req));
                selection.runtime = true;
                assert!(selection.accepts(&runtime, &req));
                assert!(!selection.accepts(&other, &req));
            }
        }
    }
}
