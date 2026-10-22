//! Filter dependency occurrences and choose releases without dependency resolution.

use anyhow::{Context, Result, bail, ensure};
use pep440_rs::{Operator, Version, VersionSpecifier, VersionSpecifiers};
use pep508_rs::Requirement;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::dependencies::{Dependency, baseline, normalize, specifiers, unsupported_reason};
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

/// Match the full declared range, including compatible bounds and exclusions.
/// URL requirements do not describe a version range.
pub fn requirement_contains(requirement: &Requirement, version: &Version) -> bool {
    !matches!(
        requirement.version_or_url,
        Some(pep508_rs::VersionOrUrl::Url(_))
    ) && specifiers(requirement)
        .iter()
        .all(|spec| specifier_contains(spec, version))
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

/// Candidate filters. A cutoff is an inclusive Unix timestamp in seconds.
#[derive(Clone, Debug)]
pub struct CandidatePolicy {
    pub target: String,
    pub cutoff: Option<i64>,
    pub allow_prerelease: bool,
    pub minimal: bool,
    pub exact: Option<Version>,
}

impl Default for CandidatePolicy {
    fn default() -> Self {
        Self {
            target: "latest".into(),
            cutoff: None,
            allow_prerelease: false,
            minimal: false,
            exact: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Decision {
    pub selected: Option<Version>,
    /// Highest observed stable, non-yanked version, before policy filters.
    pub latest: Option<Version>,
    /// Highest observed non-yanked alpha, beta, or RC version, excluding dev releases.
    pub prerelease: Option<Version>,
    /// Rejected newer versions, highest first, with all artifact rejection codes.
    pub rejected: Vec<(Version, Vec<&'static str>)>,
    /// Summary code when no requirement change is selected.
    pub reason: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Rejection {
    UpperBound,
    Excluded,
    Target,
    Python,
    Prerelease,
    Development,
    Yanked,
    Cooldown,
    UnknownUploadTime,
    LocalVersion,
    ExactTarget,
}

impl Rejection {
    fn code(self) -> &'static str {
        match self {
            Self::UpperBound => "upper_bound",
            Self::Excluded => "excluded",
            Self::Target => "target",
            Self::Python => "python",
            Self::Prerelease => "prerelease",
            Self::Development => "development",
            Self::Yanked => "yanked",
            Self::Cooldown => "cooldown",
            Self::UnknownUploadTime => "unknown_upload_time",
            Self::LocalVersion => "local_version",
            Self::ExactTarget => "exact_target",
        }
    }
}

/// Evaluate newer versions without resolving dependencies.
/// A version qualifies only if one artifact passes every active filter.
/// With prereleases enabled, PEP 440 ordering selects the highest candidate;
/// a final release takes precedence over its own prereleases, not newer RCs.
pub fn evaluate(
    requirement: &Requirement,
    releases: &[Release],
    policy: &CandidatePolicy,
    python: Option<&Version>,
) -> Decision {
    let mut decision = Decision {
        selected: None,
        latest: releases
            .iter()
            .filter(|release| !release.yanked && !release.version.any_prerelease())
            .map(|release| &release.version)
            .max()
            .cloned(),
        prerelease: releases
            .iter()
            .filter(|release| {
                !release.yanked && release.version.is_pre() && !release.version.is_dev()
            })
            .map(|release| &release.version)
            .max()
            .cloned(),
        rejected: Vec::new(),
        reason: None,
    };
    if unsupported_reason(requirement).is_some() {
        decision.reason = Some("unsupported_requirement");
        return decision;
    }
    let spec = baseline(requirement);
    let before = spec.map(|spec| spec.version());
    let mut versions: BTreeMap<&Version, Vec<&Release>> = BTreeMap::new();
    for release in releases {
        if before.is_none_or(|before| &release.version > before) {
            versions.entry(&release.version).or_default().push(release);
        }
    }
    for (version, artifacts) in versions.into_iter().rev() {
        let mut shared = BTreeSet::new();
        for spec in specifiers(requirement) {
            let reason = match spec.operator() {
                Operator::LessThan | Operator::LessThanEqual => Rejection::UpperBound,
                Operator::NotEqual | Operator::NotEqualStar => Rejection::Excluded,
                _ => continue,
            };
            if !specifier_contains(spec, version) {
                shared.insert(reason);
            }
        }
        if policy.target != "latest" {
            let width = if policy.target == "patch" { 2 } else { 1 };
            if before.is_none_or(|before| {
                before.epoch() != version.epoch()
                    || (0..width).any(|index| {
                        before.release().get(index).copied().unwrap_or(0)
                            != version.release().get(index).copied().unwrap_or(0)
                    })
            }) {
                shared.insert(Rejection::Target);
            }
        }
        if version.is_pre() && !policy.allow_prerelease {
            shared.insert(Rejection::Prerelease);
        }
        if version.is_dev() {
            shared.insert(Rejection::Development);
        }
        if version.is_local() && spec.is_none_or(|spec| *spec.operator() != Operator::Equal) {
            shared.insert(Rejection::LocalVersion);
        }
        if policy.exact.as_ref().is_some_and(|exact| exact != version) {
            shared.insert(Rejection::ExactTarget);
        }
        let mut rejected = BTreeSet::new();
        let mut eligible = false;
        for artifact in artifacts {
            let mut reasons = shared.clone();
            if artifact.yanked {
                reasons.insert(Rejection::Yanked);
            }
            if let (Some(python), Some(metadata)) = (python, &artifact.requires_python)
                && !metadata.is_empty()
                && !metadata
                    .parse::<VersionSpecifiers>()
                    .is_ok_and(|specs| specifiers_contain(&specs, python))
            {
                reasons.insert(Rejection::Python);
            }
            if let Some(cutoff) = policy.cutoff {
                match artifact.uploaded {
                    Some(uploaded) if uploaded > cutoff => {
                        reasons.insert(Rejection::Cooldown);
                    }
                    None => {
                        reasons.insert(Rejection::UnknownUploadTime);
                    }
                    _ => {}
                }
            }
            eligible |= reasons.is_empty();
            rejected.extend(reasons);
        }
        if eligible {
            if decision.selected.is_none() {
                decision.selected = Some(version.clone());
            }
        } else {
            decision.rejected.push((
                version.clone(),
                rejected.into_iter().map(Rejection::code).collect(),
            ));
        }
    }
    if let Some(selected) = &decision.selected {
        if policy.minimal && requirement_contains(requirement, selected) {
            decision.selected = None;
            decision.reason = Some("minimal");
        }
    } else {
        decision.reason = Some(if decision.rejected.is_empty() {
            "no_newer_release"
        } else {
            "blocked"
        });
    }
    decision
}

/// Pick the highest newer stable release with the default candidate policy.
pub fn choose_version(
    requirement: &Requirement,
    releases: &[Release],
    target: &str,
    python: Option<&Version>,
) -> Option<Version> {
    evaluate(
        requirement,
        releases,
        &CandidatePolicy {
            target: target.into(),
            ..CandidatePolicy::default()
        },
        python,
    )
    .selected
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
                uploaded: None,
                yanked: false,
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
                uploaded: None,
                yanked: false,
            },
            Release {
                version: version("2"),
                requires_python: Some(">=3.12".into()),
                uploaded: None,
                yanked: false,
            },
            Release {
                version: version("3"),
                requires_python: Some("nonsense".into()),
                uploaded: None,
                yanked: false,
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
            uploaded: None,
            yanked: false,
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
            uploaded: None,
            yanked: false,
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

    fn decision(requirement: &str, values: &[&str], policy: CandidatePolicy) -> Decision {
        evaluate(
            &requirement.parse().unwrap(),
            &releases(values),
            &policy,
            None,
        )
    }

    fn artifact(value: &str, python: Option<&str>, uploaded: Option<i64>, yanked: bool) -> Release {
        Release {
            version: version(value),
            requires_python: python.map(str::to_owned),
            uploaded,
            yanked,
        }
    }

    #[test]
    fn observations_are_independent_of_policy_and_include_current_versions() {
        let values = vec![
            artifact("1", None, None, false),
            artifact("2", Some(">=3.12"), Some(101), false),
            artifact("3", None, None, true),
            artifact("4rc1", None, None, false),
            artifact("5a1", None, None, true),
            artifact("6.dev1", None, None, false),
        ];
        let result = evaluate(
            &"demo>=2,<3".parse().unwrap(),
            &values,
            &CandidatePolicy::default(),
            Some(&version("3.10")),
        );
        assert_eq!(result.latest, Some(version("2")));
        assert_eq!(result.prerelease, Some(version("4rc1")));
        assert_eq!(result.selected, None);
        assert_eq!(result.reason, Some("blocked"));
        assert_eq!(result.rejected.len(), 4);
        assert!(
            result
                .rejected
                .iter()
                .all(|(value, _)| value > &version("2"))
        );
        assert_eq!(result.rejected[0].0, version("6.dev1"));
    }

    #[test]
    fn decisions_distinguish_empty_current_and_unsupported_requirements() {
        for (req, values) in [("demo>=1", vec![]), ("demo>=2", vec!["1", "2", "2.0"])] {
            let result = decision(req, &values, CandidatePolicy::default());
            assert_eq!(result.selected, None);
            assert_eq!(result.reason, Some("no_newer_release"));
            assert!(result.rejected.is_empty());
        }
        for req in [
            "demo @ https://example.org/demo.whl",
            "demo>1",
            "demo==1.*",
            "demo>=1,>=2",
        ] {
            let result = decision(req, &["3"], CandidatePolicy::default());
            assert_eq!(result.selected, None, "{req}");
            assert_eq!(result.reason, Some("unsupported_requirement"));
            assert_eq!(result.latest, Some(version("3")));
        }
    }

    #[test]
    fn every_applicable_candidate_reason_is_retained() {
        let values = [artifact("3rc1+local", Some(">=3.12"), Some(101), true)];
        let result = evaluate(
            &"demo>=1.2,<2,!=3rc1".parse().unwrap(),
            &values,
            &CandidatePolicy {
                target: "patch".into(),
                cutoff: Some(100),
                exact: Some(version("4")),
                ..CandidatePolicy::default()
            },
            Some(&version("3.10")),
        );
        assert_eq!(
            result.rejected,
            vec![(
                version("3rc1+local"),
                vec![
                    "upper_bound",
                    "excluded",
                    "target",
                    "python",
                    "prerelease",
                    "yanked",
                    "cooldown",
                    "local_version",
                    "exact_target",
                ],
            )]
        );
        assert_eq!(result.latest, None);
        assert_eq!(result.prerelease, None);
        assert_eq!(result.reason, Some("blocked"));
    }

    #[test]
    fn python_and_cooldown_must_pass_on_the_same_artifact() {
        let req = "demo>=1".parse().unwrap();
        let policy = CandidatePolicy {
            cutoff: Some(100),
            ..CandidatePolicy::default()
        };
        let mut values = vec![
            artifact("1.5", None, Some(99), false),
            artifact("2", Some(">=3.12"), Some(99), false),
            artifact("2", Some(">=3.10"), Some(101), false),
        ];
        for _ in 0..2 {
            let result = evaluate(&req, &values, &policy, Some(&version("3.10")));
            assert_eq!(result.selected, Some(version("1.5")));
            assert_eq!(result.reason, None);
            assert_eq!(
                result.rejected,
                vec![(version("2"), vec!["python", "cooldown"])]
            );
            values.reverse();
        }
        // One qualifying artifact admits the version, regardless of other files.
        values.push(artifact("2", Some(">=3.10"), Some(100), false));
        let result = evaluate(&req, &values, &policy, Some(&version("3.10")));
        assert_eq!(result.selected, Some(version("2")));
        assert!(result.rejected.is_empty());
    }

    #[test]
    fn cooldown_is_inclusive_and_unknown_times_fail_only_when_active() {
        let req = "demo>=1".parse().unwrap();
        let values = vec![
            artifact("2", None, Some(100), false),
            artifact("3", None, Some(101), false),
            artifact("4", None, None, false),
        ];
        let result = evaluate(
            &req,
            &values,
            &CandidatePolicy {
                cutoff: Some(100),
                ..CandidatePolicy::default()
            },
            None,
        );
        assert_eq!(result.selected, Some(version("2")));
        assert_eq!(result.latest, Some(version("4")));
        assert_eq!(
            result.rejected,
            vec![
                (version("4"), vec!["unknown_upload_time"]),
                (version("3"), vec!["cooldown"]),
            ]
        );
        assert_eq!(
            evaluate(&req, &values, &CandidatePolicy::default(), None).selected,
            Some(version("4"))
        );
        let all_recent = evaluate(
            &req,
            &values[1..],
            &CandidatePolicy {
                cutoff: Some(100),
                ..CandidatePolicy::default()
            },
            None,
        );
        assert_eq!(all_recent.selected, None);
        assert_eq!(all_recent.reason, Some("blocked"));
    }

    #[test]
    fn yanked_old_artifact_cannot_supply_cooldown_eligibility() {
        let values = vec![
            artifact("2", None, Some(99), true),
            artifact("2", None, Some(101), false),
            artifact("3", None, None, true),
        ];
        let result = evaluate(
            &"demo>=1".parse().unwrap(),
            &values,
            &CandidatePolicy {
                cutoff: Some(100),
                ..CandidatePolicy::default()
            },
            None,
        );
        assert_eq!(result.selected, None);
        assert_eq!(result.latest, Some(version("2")));
        assert_eq!(
            result.rejected,
            vec![
                (version("3"), vec!["yanked", "unknown_upload_time"]),
                (version("2"), vec!["yanked", "cooldown"]),
            ]
        );
    }

    #[test]
    fn prerelease_policy_uses_pep440_order_and_never_allows_development() {
        let values = ["2a1", "2b1", "2rc1", "2.dev9", "2rc2.dev1", "1.9"];
        let result = decision("demo>=1", &values, CandidatePolicy::default());
        assert_eq!(result.selected, Some(version("1.9")));
        assert_eq!(result.prerelease, Some(version("2rc1")));
        assert!(
            result
                .rejected
                .iter()
                .any(|(value, reasons)| value == &version("2.dev9") && reasons == &["development"])
        );
        let policy = CandidatePolicy {
            allow_prerelease: true,
            ..CandidatePolicy::default()
        };
        let result = decision("demo>=1", &values, policy.clone());
        assert_eq!(result.selected, Some(version("2rc1")));
        assert_eq!(result.rejected.len(), 2);
        assert!(
            result
                .rejected
                .iter()
                .all(|(_, reasons)| reasons == &["development"])
        );
        for (values, expected) in [
            (vec!["2rc1", "2"], "2"),
            (vec!["2", "3rc1"], "3rc1"),
            (vec!["2a1", "2b1"], "2b1"),
            (vec!["2b1", "2rc1"], "2rc1"),
        ] {
            assert_eq!(
                decision("demo>=1", &values, policy.clone()).selected,
                Some(version(expected))
            );
        }
        assert_eq!(
            choose("demo>=1rc1", &["1rc2", "1", "2.dev1"], "latest"),
            Some(version("1"))
        );
        assert_eq!(choose("demo>=1rc1", &["1rc2"], "latest"), None);
    }

    #[test]
    fn prereleases_remain_subject_to_python_age_bounds_and_epochs() {
        let values = vec![
            artifact("1!2a1", Some(">=3.10"), Some(99), false),
            artifact("1!2b1", Some(">=3.12"), Some(99), false),
            artifact("1!2rc1", Some(">=3.10"), Some(101), false),
            artifact("2!1rc1", None, Some(99), false),
        ];
        let result = evaluate(
            &"demo>=1!1,<1!3".parse().unwrap(),
            &values,
            &CandidatePolicy {
                cutoff: Some(100),
                allow_prerelease: true,
                target: "minor".into(),
                ..CandidatePolicy::default()
            },
            Some(&version("3.10")),
        );
        // A minor target retains the first release component and the epoch.
        assert_eq!(result.selected, None);
        assert_eq!(result.rejected[0].1, vec!["upper_bound", "target"]);
        let result = evaluate(
            &"demo>=1!1,<1!3".parse().unwrap(),
            &values,
            &CandidatePolicy {
                cutoff: Some(100),
                allow_prerelease: true,
                ..CandidatePolicy::default()
            },
            Some(&version("3.10")),
        );
        assert_eq!(result.selected, Some(version("1!2a1")));
        assert_eq!(result.latest, None);
    }

    #[test]
    fn minimal_suppresses_only_when_the_full_original_range_admits_the_candidate() {
        let policy = CandidatePolicy {
            minimal: true,
            ..CandidatePolicy::default()
        };
        for (req, values, expected, reason) in [
            ("demo>=1", vec!["2"], None, Some("minimal")),
            ("demo", vec!["2"], None, Some("minimal")),
            ("demo==1", vec!["2"], Some("2"), None),
            ("demo~=1.2", vec!["1.9"], None, Some("minimal")),
            ("demo~=1.2", vec!["2"], Some("2"), None),
            ("demo~=1.2.3", vec!["1.3"], Some("1.3"), None),
            ("demo>=1,<2", vec!["1.9", "2"], None, Some("minimal")),
            (
                "demo>=1,!=2.0.1.*",
                vec!["2.0", "2.0.1"],
                None,
                Some("minimal"),
            ),
            ("demo>=1,!=2.0.0.*", vec!["2.0"], None, Some("blocked")),
        ] {
            let result = decision(req, &values, policy.clone());
            assert_eq!(result.selected, expected.map(version), "{req}");
            assert_eq!(result.reason, reason, "{req}");
        }
        let req = "demo>=1,!=2.0.0.*".parse().unwrap();
        assert!(!requirement_contains(&req, &version("2")));
        assert!(requirement_contains(&req, &version("2.1")));
        assert!(!requirement_contains(
            &"demo @ https://example.org/demo.whl".parse().unwrap(),
            &version("2")
        ));
    }

    #[test]
    fn exact_targets_are_not_implicitly_downgraded_or_substituted() {
        for (req, exact, values, expected) in [
            ("demo>=1", "2", vec!["2", "3"], Some("2")),
            ("demo>=1", "2", vec!["3"], None),
            ("demo>=2", "1", vec!["1", "3"], None),
            ("demo>=2", "2", vec!["2", "3"], None),
            ("demo>=1,<2", "2", vec!["1.9", "2"], None),
            ("demo>=1,!=2", "2", vec!["2", "3"], None),
            ("demo>=1.2", "1.3", vec!["1.2.1", "1.3"], Some("1.3")),
            ("demo>=1", "2rc1", vec!["2rc1"], None),
            ("demo>=1", "2+local", vec!["2+local"], None),
            ("demo==1+local", "2+local", vec!["2+local"], Some("2+local")),
        ] {
            let result = decision(
                req,
                &values,
                CandidatePolicy {
                    exact: Some(version(exact)),
                    ..CandidatePolicy::default()
                },
            );
            assert_eq!(result.selected, expected.map(version), "{req} -> {exact}");
        }
        let result = decision(
            "demo>=1.2",
            &["1.3"],
            CandidatePolicy {
                exact: Some(version("1.3")),
                target: "patch".into(),
                ..CandidatePolicy::default()
            },
        );
        assert_eq!(result.rejected, vec![(version("1.3"), vec!["target"])]);
        let result = decision(
            "demo>=1",
            &["2"],
            CandidatePolicy {
                exact: Some(version("2")),
                minimal: true,
                ..CandidatePolicy::default()
            },
        );
        assert_eq!(result.selected, None);
        assert_eq!(result.reason, Some("minimal"));
    }

    #[test]
    fn exact_target_artifacts_must_pass_active_safety_rules() {
        let req = "demo>=1".parse().unwrap();
        let policy = CandidatePolicy {
            exact: Some(version("2")),
            cutoff: Some(100),
            ..CandidatePolicy::default()
        };
        for (artifact, expected) in [
            (artifact("2", None, Some(99), true), vec!["yanked"]),
            (
                artifact("2", Some(">=3.12"), Some(99), false),
                vec!["python"],
            ),
            (artifact("2", None, Some(101), false), vec!["cooldown"]),
            (
                artifact("2", None, None, false),
                vec!["unknown_upload_time"],
            ),
        ] {
            let result = evaluate(&req, &[artifact], &policy, Some(&version("3.10")));
            assert_eq!(result.selected, None);
            assert_eq!(result.rejected, vec![(version("2"), expected)]);
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
