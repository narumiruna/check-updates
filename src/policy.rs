//! Declarative selection policies. These settings never authorize side effects.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use pep508_rs::Requirement;
use toml_edit::{Item, TableLike};

use crate::dependencies::{Dependency, normalize};
use crate::selection::Selection;
use crate::workspace::Project;

#[derive(Clone, Debug, Default)]
pub struct PackagePolicy {
    pub target: Option<String>,
    pub cooldown: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub target: Option<String>,
    pub cooldown: Option<u64>,
    pub pre: Option<bool>,
    pub minimal: Option<bool>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub include_glob: Option<Vec<String>>,
    pub exclude_glob: Option<Vec<String>>,
    pub group: Option<Vec<String>>,
    pub extra: Option<Vec<String>>,
    pub runtime: Option<bool>,
    pub packages: BTreeMap<String, PackagePolicy>,
}

pub fn duration(value: &str) -> Result<u64> {
    let (number, multiplier) = match value.as_bytes().last() {
        Some(b'd') => (&value[..value.len() - 1], 86_400_u64),
        Some(b'h') => (&value[..value.len() - 1], 3_600),
        Some(b'm') => (&value[..value.len() - 1], 60),
        _ => bail!("Cooldown needs a positive integer with d, h, or m, for example 7d."),
    };
    ensure!(
        !number.is_empty() && number.bytes().all(|v| v.is_ascii_digit()),
        "Invalid cooldown duration."
    );
    let seconds = number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier));
    let seconds = seconds
        .filter(|n| *n > 0 && *n <= i64::MAX as u64)
        .context("Cooldown duration is zero or too large.")?;
    Ok(seconds)
}

fn target(value: &Item) -> Result<String> {
    match value.as_str() {
        Some(value @ ("latest" | "minor" | "patch")) => Ok(value.to_owned()),
        _ => bail!("Policy target must be latest, minor, or patch."),
    }
}

fn strings(value: &Item) -> Result<Vec<String>> {
    value
        .as_array()
        .context("Policy lists must be arrays of strings.")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .context("Policy lists must contain strings.")
        })
        .collect()
}

fn boolean(value: &Item) -> Result<bool> {
    value.as_bool().context("Policy flag must be a boolean.")
}

impl Settings {
    pub fn read(project: &Project) -> Result<Self> {
        let Some(item) = project
            .document
            .get("tool")
            .and_then(|v| v.get("check-updates"))
        else {
            return Ok(Self::default());
        };
        let table = item
            .as_table_like()
            .context("tool.check-updates must be a table.")?;
        Self::parse(table)
    }

    fn parse(table: &dyn TableLike) -> Result<Self> {
        let mut result = Self::default();
        for (key, value) in table.iter() {
            match key {
                "target" => result.target = Some(target(value)?),
                "cooldown" => {
                    result.cooldown = Some(duration(
                        value
                            .as_str()
                            .context("Policy cooldown must be a duration string.")?,
                    )?)
                }
                "pre" => result.pre = Some(boolean(value)?),
                "minimal" => result.minimal = Some(boolean(value)?),
                "runtime" => result.runtime = Some(boolean(value)?),
                "include" => result.include = Some(strings(value)?),
                "exclude" => result.exclude = Some(strings(value)?),
                "include-glob" => result.include_glob = Some(strings(value)?),
                "exclude-glob" => result.exclude_glob = Some(strings(value)?),
                "group" => result.group = Some(strings(value)?),
                "extra" => result.extra = Some(strings(value)?),
                "packages" => {
                    let packages = value
                        .as_table_like()
                        .context("Policy packages must be a table.")?;
                    for (name, value) in packages.iter() {
                        let name = normalize(name);
                        ensure!(
                            name.parse::<pep508_rs::PackageName>().is_ok(),
                            "Invalid policy package name."
                        );
                        let fields = value
                            .as_table_like()
                            .context("Each package policy must be a table.")?;
                        let mut package = PackagePolicy::default();
                        for (key, value) in fields.iter() {
                            match key {
                                "target" => package.target = Some(target(value)?),
                                "cooldown" => {
                                    package.cooldown =
                                        Some(duration(value.as_str().context(
                                            "Package cooldown must be a duration string.",
                                        )?)?)
                                }
                                _ => bail!("Package policy supports only target and cooldown."),
                            }
                        }
                        ensure!(
                            result.packages.insert(name, package).is_none(),
                            "Duplicate normalized policy package name."
                        );
                    }
                }
                _ => bail!(
                    "Unknown check-updates policy key. Only declarative selection settings are allowed."
                ),
            }
        }
        result.validate()?;
        Ok(result)
    }

    /// Higher-priority scalar and list values replace lower-priority values.
    /// Package tables merge by normalized name and then by field.
    pub fn overlay(&self, other: &Self) -> Self {
        let mut result = self.clone();
        macro_rules! replace {
            ($($field:ident),+) => { $(if other.$field.is_some() { result.$field = other.$field.clone(); })+ };
        }
        replace!(
            target,
            cooldown,
            pre,
            minimal,
            runtime,
            include,
            exclude,
            include_glob,
            exclude_glob,
            group,
            extra
        );
        for (name, higher) in &other.packages {
            let lower = result.packages.entry(name.clone()).or_default();
            if higher.target.is_some() {
                lower.target = higher.target.clone();
            }
            if higher.cooldown.is_some() {
                lower.cooldown = higher.cooldown;
            }
        }
        result
    }

    pub fn validate(&self) -> Result<()> {
        for patterns in [&self.include_glob, &self.exclude_glob]
            .into_iter()
            .flatten()
        {
            for pattern in patterns {
                glob::Pattern::new(&normalize(pattern)).context("Invalid package glob.")?;
            }
        }
        Ok(())
    }

    pub fn package_policy(&self, name: &str, cli: &Self) -> PackagePolicy {
        let package = self.packages.get(name);
        PackagePolicy {
            target: cli
                .target
                .clone()
                .or_else(|| package.and_then(|p| p.target.clone()))
                .or_else(|| self.target.clone()),
            cooldown: cli
                .cooldown
                .or_else(|| package.and_then(|p| p.cooldown))
                .or(self.cooldown),
        }
    }

    pub fn accepts(&self, dependency: &Dependency, requirement: &Requirement) -> bool {
        let name = normalize(requirement.name.as_ref());
        let exact = |list: &Option<Vec<String>>| {
            list.as_ref()
                .is_some_and(|list| list.iter().any(|n| normalize(n) == name))
        };
        let pattern = |list: &Option<Vec<String>>| {
            list.as_ref().is_some_and(|list| {
                list.iter()
                    .any(|p| glob::Pattern::new(&normalize(p)).is_ok_and(|p| p.matches(&name)))
            })
        };
        if exact(&self.exclude) || pattern(&self.exclude_glob) {
            return false;
        }
        let has_include = self.include.as_ref().is_some_and(|v| !v.is_empty())
            || self.include_glob.as_ref().is_some_and(|v| !v.is_empty());
        if has_include && !(exact(&self.include) || pattern(&self.include_glob)) {
            return false;
        }
        let names = |list: &Option<Vec<String>>| -> HashSet<String> {
            list.iter().flatten().map(|v| normalize(v)).collect()
        };
        Selection {
            groups: names(&self.group),
            extras: names(&self.extra),
            runtime: self.runtime.unwrap_or(false),
            ..Selection::default()
        }
        .accepts(dependency, requirement)
    }
}

/// Select editable members without removing the root needed for source inheritance.
pub fn selected_members(projects: &[Project], requested: &[String]) -> Result<HashSet<PathBuf>> {
    if requested.is_empty() {
        return Ok(projects.iter().map(|p| p.path.clone()).collect());
    }
    let mut result = HashSet::new();
    for name in requested {
        let name = normalize(name);
        let matches: Vec<_> = projects
            .iter()
            .filter(|p| {
                p.document
                    .get("project")
                    .and_then(|p| p.get("name"))
                    .and_then(Item::as_str)
                    .is_some_and(|v| normalize(v) == name)
            })
            .collect();
        ensure!(
            matches.len() == 1,
            "Workspace member name is unknown or ambiguous: {}",
            crate::changes::safe_text(&name, false)
        );
        result.insert(matches[0].path.clone());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings(text: &str) -> Result<Settings> {
        let document = format!("[tool.check-updates]\n{text}").parse()?;
        Settings::read(&Project {
            path: "pyproject.toml".into(),
            document,
            original: vec![],
        })
    }
    #[test]
    fn duration_is_checked_and_finite() {
        for (value, seconds) in [("7d", 604800), ("12h", 43200), ("30m", 1800)] {
            assert_eq!(duration(value).unwrap(), seconds);
        }
        for value in [
            "",
            "7",
            "0d",
            "-1d",
            "1.2h",
            "1w",
            "9999999999999999999d",
            "é",
        ] {
            assert!(duration(value).is_err());
        }
    }
    #[test]
    fn strict_types_and_no_side_effect_settings() {
        for value in [
            "upgrade=true",
            "verify=true",
            "doctor=true",
            "target='other'",
            "exclude=1",
            "pre=1",
            "cooldown=7",
            "packages.foo={upgrade=true}",
            "include-glob=['[']",
        ] {
            assert!(settings(value).is_err(), "{value}");
        }
        assert!(
            settings("packages.FOO_BAR={target='patch'}\npackages.foo-bar={target='minor'}")
                .is_err()
        );
    }
    #[test]
    fn inheritance_replaces_lists_and_cli_beats_package() {
        let root = settings(
            "target='minor'\nexclude=['foo']\npackages.foo={target='patch',cooldown='7d'}",
        )
        .unwrap();
        let member = settings("exclude=[]\npackages.foo={cooldown='1d'}").unwrap();
        let merged = root.overlay(&member);
        assert_eq!(merged.exclude, Some(vec![]));
        assert_eq!(
            merged
                .package_policy("foo", &Settings::default())
                .target
                .as_deref(),
            Some("patch")
        );
        assert_eq!(
            merged.package_policy("foo", &Settings::default()).cooldown,
            Some(86400)
        );
        let cli = Settings {
            target: Some("latest".into()),
            cooldown: Some(3600),
            ..Default::default()
        };
        assert_eq!(
            merged.package_policy("foo", &cli).target.as_deref(),
            Some("latest")
        );
        assert_eq!(merged.package_policy("foo", &cli).cooldown, Some(3600));
    }
    #[test]
    fn glob_and_exact_filters_form_union_and_exclusions_win() {
        let policy =
            settings("include=['other']\ninclude-glob=['PYTEST_*']\nexclude=['pytest_bad']")
                .unwrap();
        for (name, accepted) in [
            ("pytest-good", true),
            ("other", true),
            ("pytest-bad", false),
            ("unused", false),
        ] {
            let dependency = Dependency {
                requirement: format!("{name}>=1"),
                section: vec!["project".into(), "dependencies".into()],
                index: 0,
            };
            assert_eq!(
                policy.accepts(&dependency, &dependency.requirement.parse().unwrap()),
                accepted
            );
        }
    }
}
