//! Read dependency occurrences and update their declared version floors.

use anyhow::{Context, Result, ensure};
use pep440_rs::{Operator, Version, VersionSpecifier};
use pep508_rs::{Requirement, VersionOrUrl};
use regex::Regex;
use std::sync::LazyLock;
use toml_edit::{DocumentMut, Item, Value};

#[derive(Clone, Debug)]
pub struct Dependency {
    pub requirement: String,
    pub section: Vec<String>,
    pub index: usize,
}

#[derive(Clone, Debug)]
pub struct Update {
    pub dependency: Dependency,
    pub replacement: String,
    pub version: Version,
    pub release_link: Option<String>,
    pub project_link: Option<String>,
}

/// Visit runtime requirements, extras, and groups, but not group include tables.
pub fn dependencies(document: &DocumentMut) -> Vec<Dependency> {
    let mut arrays = Vec::new();
    if let Some(project) = document.get("project") {
        if let Some(array) = project.get("dependencies").and_then(Item::as_array) {
            arrays.push((vec!["project".into(), "dependencies".into()], array));
        }
        if let Some(extras) = project
            .get("optional-dependencies")
            .and_then(Item::as_table_like)
        {
            for (name, item) in extras.iter() {
                if let Some(array) = item.as_array() {
                    arrays.push((
                        vec![
                            "project".into(),
                            "optional-dependencies".into(),
                            name.into(),
                        ],
                        array,
                    ));
                }
            }
        }
    }
    if let Some(groups) = document
        .get("dependency-groups")
        .and_then(Item::as_table_like)
    {
        for (name, item) in groups.iter() {
            if let Some(array) = item.as_array() {
                arrays.push((vec!["dependency-groups".into(), name.into()], array));
            }
        }
    }
    arrays
        .into_iter()
        .flat_map(|(section, array)| {
            array.iter().enumerate().filter_map(move |(index, item)| {
                item.as_str().map(|requirement| Dependency {
                    requirement: requirement.into(),
                    section: section.clone(),
                    index,
                })
            })
        })
        .collect()
}

/// Apply PEP 503 name normalization, also used for group and extra selectors.
pub fn normalize(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut separator = false;
    for ch in value.chars() {
        if matches!(ch, '-' | '_' | '.') {
            if !separator {
                normalized.push('-');
            }
            separator = true;
        } else {
            normalized.push(ch.to_ascii_lowercase());
            separator = false;
        }
    }
    normalized
}

pub(crate) fn specifiers(requirement: &Requirement) -> &[VersionSpecifier] {
    match &requirement.version_or_url {
        Some(VersionOrUrl::VersionSpecifier(specs)) => specs,
        _ => &[],
    }
}

/// Accept arbitrary equality pins as unsupported, even when their token is not
/// a PEP 440 version. Strictly validate the name, extras, and markers as usual.
pub fn parse_requirement(value: &str) -> Result<Requirement> {
    let original_error = match value.parse::<Requirement>() {
        Ok(requirement) => return Ok(requirement),
        Err(error) => error,
    };
    static ARBITRARY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"===\s*[^\s,;)]+").unwrap());
    let (before, marker) = value
        .split_once(';')
        .map_or((value, ""), |(before, marker)| (before, marker));
    let sanitized = ARBITRARY.replace_all(before, "===0");
    if sanitized != before {
        let sanitized = if value.contains(';') {
            format!("{sanitized};{marker}")
        } else {
            sanitized.into_owned()
        };
        if let Ok(requirement) = sanitized.parse::<Requirement>()
            && specifiers(&requirement)
                .iter()
                .any(|spec| *spec.operator() == Operator::ExactEqual)
        {
            return Ok(requirement);
        }
    }
    Err(original_error.into())
}

pub fn baseline(requirement: &Requirement) -> Option<&VersionSpecifier> {
    specifiers(requirement).iter().find(|spec| {
        matches!(
            spec.operator(),
            Operator::GreaterThanEqual | Operator::Equal | Operator::TildeEqual
        )
    })
}

pub fn unsupported_reason(requirement: &Requirement) -> Option<&'static str> {
    if matches!(requirement.version_or_url, Some(VersionOrUrl::Url(_))) {
        return Some("URL dependency");
    }
    let specs = specifiers(requirement);
    if specs.len() > 1 {
        if specs
            .iter()
            .filter(|spec| *spec.operator() == Operator::GreaterThanEqual)
            .count()
            == 1
            && specs.iter().all(|spec| {
                matches!(
                    spec.operator(),
                    Operator::GreaterThanEqual
                        | Operator::LessThan
                        | Operator::LessThanEqual
                        | Operator::NotEqual
                        | Operator::NotEqualStar
                )
            })
        {
            return None;
        }
        return Some("unsupported composite version constraints");
    }
    if specs.first().is_some_and(|spec| {
        !matches!(
            spec.operator(),
            Operator::Equal | Operator::GreaterThanEqual | Operator::TildeEqual
        )
    }) {
        return Some("unsupported version constraint");
    }
    None
}

/// Change only the floor token. Keep extras, bounds, markers, and spacing.
pub fn replacement(requirement: &Requirement, original: &str, latest: &Version) -> Option<String> {
    let Some(spec) = baseline(requirement) else {
        let (before, marker) = original
            .split_once(';')
            .map_or((original, None), |(before, marker)| (before, Some(marker)));
        return Some(format!(
            "{}>={latest}{}",
            before.trim_end(),
            marker.map_or_else(String::new, |marker| format!(" ;{marker}"))
        ));
    };
    if latest <= spec.version() {
        return None;
    }
    static FLOOR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(>=|==|~=)\s*([^\s,;)]+)").unwrap());
    let before_marker = original.split(';').next().unwrap_or(original);
    for captures in FLOOR.captures_iter(before_marker) {
        let token = captures.get(2).unwrap();
        if captures[1] == spec.operator().to_string()
            && token.as_str().parse::<Version>().ok().as_ref() == Some(spec.version())
        {
            let mut result = original.to_owned();
            result.replace_range(token.range(), &latest.to_string());
            return Some(result);
        }
    }
    None
}

/// Describe the declared baseline, not an installed version or compatibility.
pub fn version_change(update: &Update) -> &'static str {
    let Ok(requirement) = update.dependency.requirement.parse::<Requirement>() else {
        return "other";
    };
    let Some(spec) = baseline(&requirement) else {
        return "unbounded";
    };
    let before = spec.version();
    let after = &update.version;
    if before.epoch() != after.epoch()
        || [before, after].iter().any(|version| {
            version.any_prerelease()
                || version.is_post()
                || version.is_local()
                || version.release().len() > 3
        })
    {
        return "other";
    }
    for (index, label) in ["major", "minor", "patch"].into_iter().enumerate() {
        if before.release().get(index).copied().unwrap_or(0)
            != after.release().get(index).copied().unwrap_or(0)
        {
            return label;
        }
    }
    "other"
}

/// Replace one occurrence and keep its string delimiters and TOML decoration.
pub fn apply_update(document: &mut DocumentMut, update: &Update) -> Result<()> {
    let mut item = document.as_item_mut();
    for key in &update.dependency.section {
        item = item.get_mut(key).with_context(|| {
            format!(
                "Missing dependency section: {}",
                update.dependency.section.join(".")
            )
        })?;
    }
    let value = item
        .as_array_mut()
        .and_then(|array| array.get_mut(update.dependency.index))
        .context("Missing dependency occurrence")?;
    ensure!(
        value.as_str() == Some(update.dependency.requirement.as_str()),
        "Dependency occurrence changed before update"
    );
    let Value::String(string) = value else {
        anyhow::bail!("Dependency occurrence is not a string");
    };
    let repr = string.display_repr();
    // Try a source-level token edit first. This keeps multiline whitespace and
    // escape sequences outside the changed version exactly as written.
    let original = &update.dependency.requirement;
    let prefix: usize = original
        .chars()
        .zip(update.replacement.chars())
        .take_while(|(before, after)| before == after)
        .map(|(character, _)| character.len_utf8())
        .sum();
    let suffix: usize = original[prefix..]
        .chars()
        .rev()
        .zip(update.replacement[prefix..].chars().rev())
        .take_while(|(before, after)| before == after)
        .map(|(character, _)| character.len_utf8())
        .sum();
    let before = &original[prefix..original.len() - suffix];
    let after = &update.replacement[prefix..update.replacement.len() - suffix];
    for (start, _) in repr.match_indices(before) {
        let mut candidate = repr.to_string();
        candidate.replace_range(start..start + before.len(), after);
        if let Ok(mut updated) = candidate.parse::<Value>()
            && updated.as_str() == Some(update.replacement.as_str())
        {
            *updated.decor_mut() = value.decor().clone();
            *value = updated;
            return Ok(());
        }
    }
    let delimiter = if repr.starts_with("\"\"\"") {
        "\"\"\""
    } else if repr.starts_with("'''") {
        "'''"
    } else if repr.starts_with('\'') {
        "'"
    } else {
        "\""
    };
    let content = if delimiter.starts_with('"') {
        let encoded = serde_json::to_string(&update.replacement)?;
        encoded[1..encoded.len() - 1].to_owned()
    } else {
        update.replacement.clone()
    };
    let quoted = format!("{delimiter}{content}{delimiter}");
    let mut updated = quoted
        .parse::<Value>()
        .ok()
        .filter(|value| value.as_str() == Some(update.replacement.as_str()))
        .unwrap_or_else(|| Value::from(update.replacement.clone()));
    *updated.decor_mut() = value.decor().clone();
    *value = updated;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requirement(value: &str) -> Requirement {
        value.parse().unwrap()
    }

    fn version(value: &str) -> Version {
        value.parse().unwrap()
    }

    fn update(original: &str, latest: &str) -> Update {
        Update {
            dependency: Dependency {
                requirement: original.into(),
                section: vec!["project".into(), "dependencies".into()],
                index: 0,
            },
            replacement: replacement(&requirement(original), original, &version(latest)).unwrap(),
            version: version(latest),
            release_link: None,
            project_link: None,
        }
    }

    #[test]
    fn reads_all_occurrences_without_include_tables() {
        let document = r#"[project]
dependencies = ["Demo>=1", 42, "bare"]
[project.optional-dependencies]
cli = ["Demo==1"]
broken = "not an array"
[dependency-groups]
dev = [{include-group = "other"}, "Demo~=1.0"]
"#
        .parse::<DocumentMut>()
        .unwrap();
        let entries = dependencies(&document);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[1].index, 2);
        assert_eq!(
            entries[2].section,
            ["project", "optional-dependencies", "cli"]
        );
        assert_eq!(entries[3].section, ["dependency-groups", "dev"]);
        assert_eq!(entries[3].index, 1);
        assert!(dependencies(&DocumentMut::new()).is_empty());
    }

    #[test]
    fn canonicalizes_names() {
        for name in ["demo-pkg", "Demo_Pkg", "DEMO.pkg", "demo.._-PKG"] {
            assert_eq!(normalize(name), "demo-pkg");
        }
    }

    #[test]
    fn arbitrary_pins_are_valid_but_unsupported() {
        for value in [
            "demo===nightly",
            "demo[foo]=== nightly; python_version > '3.10'",
        ] {
            let requirement = parse_requirement(value).unwrap();
            assert_eq!(requirement.name.as_ref(), "demo");
            assert_eq!(
                unsupported_reason(&requirement),
                Some("unsupported version constraint")
            );
        }
        for value in [
            "bad!===nightly",
            "demo===nightly; invalid marker",
            "demo>nightly",
        ] {
            assert!(parse_requirement(value).is_err(), "{value}");
        }
    }

    #[test]
    fn supported_constraints() {
        for value in [
            "demo",
            "demo[extra]",
            "demo==1",
            "demo>=1",
            "demo~=1.0",
            "demo>=1,<2",
            "demo>=1,<=2",
            "demo>=1,<2,!=1.5",
            "demo>=1,!=1.5.*",
            "demo==1+internal",
        ] {
            assert_eq!(unsupported_reason(&requirement(value)), None, "{value}");
        }
    }

    #[test]
    fn unsupported_constraints() {
        for value in ["demo==1.*", "demo!=1", "demo<2", "demo>1", "demo===1"] {
            assert_eq!(
                unsupported_reason(&requirement(value)),
                Some("unsupported version constraint"),
                "{value}"
            );
        }
        for value in [
            "demo>=1,>=1.2",
            "demo==1,<2",
            "demo~=1.0,<2",
            "demo>1,<2",
            "demo<2,!=1",
        ] {
            assert_eq!(
                unsupported_reason(&requirement(value)),
                Some("unsupported composite version constraints"),
                "{value}"
            );
        }
        assert_eq!(
            unsupported_reason(&requirement("demo @ https://example.org/demo.whl")),
            Some("URL dependency")
        );
    }

    #[test]
    fn replacement_preserves_constraints_and_spacing() {
        for original in [
            "Demo[extra] >= 1.0 , < 2 ; python_version >= '3.10'",
            "Demo (>=1.0,<2,!=1.5)",
            "Demo<2,>=1.0",
            "Demo<=2, >=1.0",
            "Demo==1.0",
            "Demo~=1.0",
        ] {
            assert_eq!(
                replacement(&requirement(original), original, &version("1.8")),
                Some(original.replace("1.0", "1.8"))
            );
        }
        let original = "demo[all]; python_version > '3.10'";
        assert_eq!(
            replacement(&requirement(original), original, &version("2")),
            Some("demo[all]>=2 ; python_version > '3.10'".into())
        );
        assert_eq!(
            replacement(&requirement("demo"), "demo", &version("2")),
            Some("demo>=2".into())
        );
        assert_eq!(
            replacement(&requirement("demo>=2"), "demo>=2", &version("2")),
            None
        );
        assert_eq!(
            replacement(&requirement("demo>=3"), "demo>=3", &version("2")),
            None
        );
        assert_eq!(
            replacement(
                &requirement("demo==1.2.3+old"),
                "demo==1.2.3+old",
                &version("1.2.5+internal")
            ),
            Some("demo==1.2.5+internal".into())
        );
        assert_eq!(
            replacement(&requirement("demo>=v1.0"), "demo>=v1.0", &version("2")),
            Some("demo>=2".into())
        );
    }

    #[test]
    fn classifies_baseline_changes() {
        for (original, latest, expected) in [
            ("demo", "2", "unbounded"),
            ("demo>=1", "2", "major"),
            ("demo>=1.2.3", "1.3", "minor"),
            ("demo>=1.2", "1.2.1", "patch"),
            ("demo>=1.2", "1.2.0", "other"),
            ("demo>=1", "1!2", "other"),
            ("demo>=1", "2rc1", "other"),
            ("demo>=1.dev1", "2", "other"),
            ("demo>=1", "2.post1", "other"),
            ("demo==1+old", "2", "other"),
            ("demo>=1", "2.0.0.1", "other"),
        ] {
            let mut change = update(original, "9");
            change.version = version(latest);
            assert_eq!(version_change(&change), expected, "{original} -> {latest}");
        }
    }

    #[test]
    fn keeps_quote_styles_and_comments() {
        for quote in ["\"", "'", "\"\"\"", "'''"] {
            let original = format!(
                "[project]\ndependencies = [\n  {quote}demo>=1.0{quote}, # Keep comment.\n]\n"
            );
            let mut document = original.parse::<DocumentMut>().unwrap();
            apply_update(&mut document, &update("demo>=1.0", "2.0")).unwrap();
            assert_eq!(document.to_string(), original.replace("1.0", "2.0"));
        }
    }

    #[test]
    fn updates_only_selected_occurrence() {
        let original = "[project]\ndependencies = [\"demo>=1.0\", 'demo[all]>=1.0; python_version < \"3.14\"'] # Keep\n[project.optional-dependencies]\ncli = [\"demo>=1.0\"]\n[dependency-groups]\ndev = [{include-group = \"other\"}, \"demo>=1.0\"]\n";
        let mut document = original.parse::<DocumentMut>().unwrap();
        let selected = dependencies(&document)[1].clone();
        let mut change = update(&selected.requirement, "2.0");
        change.dependency = selected;
        apply_update(&mut document, &change).unwrap();
        assert_eq!(
            document.to_string(),
            original.replace("demo[all]>=1.0", "demo[all]>=2.0")
        );
        assert!(apply_update(&mut document, &change).is_err());
    }

    #[test]
    fn keeps_multiline_whitespace_and_unrelated_escape_sequences() {
        for original in [
            "[project]\ndependencies = [\"\"\"demo>=1.0;\\n python_version < \\\"3.14\\\"\"\"\"]\n",
            "[project]\ndependencies = [\"\"\"demo>=1.0;\n python_version < '3.14'\"\"\"]\n",
            "[project]\ndependencies = [\"demo>=1.0; python_version < '\\u0033.14'\"]\n",
        ] {
            let mut document = original.parse::<DocumentMut>().unwrap();
            let entry = dependencies(&document).remove(0);
            let mut change = update(&entry.requirement, "2.0");
            change.dependency = entry;
            apply_update(&mut document, &change).unwrap();
            assert_eq!(
                document.to_string(),
                original.replace("demo>=1.0", "demo>=2.0")
            );
        }
    }

    #[test]
    fn keeps_basic_quotes_with_escaped_markers() {
        let original = r#"[project]
dependencies = ["demo>=1; python_version < \"3.14\""]
"#;
        let mut document = original.parse::<DocumentMut>().unwrap();
        let entry = dependencies(&document).remove(0);
        let mut change = update(&entry.requirement, "2");
        change.dependency = entry;
        apply_update(&mut document, &change).unwrap();
        assert_eq!(document.to_string(), original.replace("demo>=1", "demo>=2"));
    }
}
