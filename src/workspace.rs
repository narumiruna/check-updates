//! Discover uv workspaces and resolve dependency index sources without invoking uv.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, TableLike};

use crate::dependencies::normalize;
use crate::indexes::Index;

#[derive(Clone, Debug)]
pub struct Project {
    pub path: PathBuf,
    pub document: DocumentMut,
    pub original: Vec<u8>,
}

pub fn read_project(path: impl AsRef<Path>) -> Result<Project> {
    let path = path.as_ref();
    let read = || -> Result<Project> {
        let original = std::fs::read(path)?;
        let document = std::str::from_utf8(&original)?.parse::<DocumentMut>()?;
        Ok(Project {
            path: path.to_owned(),
            document,
            original,
        })
    };
    read().map_err(|error| anyhow::anyhow!("Cannot read {}: {error}", path.display()))
}

fn uv_config(project: &Project) -> Result<Option<&dyn TableLike>> {
    let Some(tool) = project.document.get("tool") else {
        return Ok(None);
    };
    let tool = tool
        .as_table_like()
        .context("tool and tool.uv must be tables.")?;
    tool.get("uv")
        .map(|uv| {
            uv.as_table_like()
                .context("tool and tool.uv must be tables.")
        })
        .transpose()
}

fn patterns(config: &dyn TableLike, key: &str, required: bool) -> Result<Vec<String>> {
    let error = || format!("tool.uv.workspace.{key} must be an array of glob strings.");
    let Some(value) = config.get(key) else {
        if required {
            bail!(error());
        }
        return Ok(Vec::new());
    };
    value
        .as_array()
        .with_context(error)?
        .iter()
        .map(|value| value.as_str().map(str::to_owned).with_context(error))
        .collect()
}

pub fn load_projects(workspace: bool) -> Result<Vec<Project>> {
    if !workspace {
        return Ok(vec![read_project("pyproject.toml")?]);
    }
    discover_projects(&std::env::current_dir()?)
}

fn expand_pattern(directory: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    if pattern.is_empty() || Path::new(pattern).is_absolute() {
        bail!("Workspace glob patterns must be nonempty and relative: {pattern}");
    }
    // Escape the directory, not the user pattern: workspace paths can contain glob characters.
    let directory = glob::Pattern::escape(
        directory
            .to_str()
            .context("Workspace directory is not valid UTF-8.")?,
    );
    let pattern = format!("{directory}/{pattern}");
    let options = glob::MatchOptions {
        require_literal_separator: true,
        ..Default::default()
    };
    glob::glob_with(&pattern, options)?
        .map(|entry| entry.map_err(Into::into))
        .collect()
}

// Accept an explicit start directory so tests do not change the process cwd.
fn discover_projects(current: &Path) -> Result<Vec<Project>> {
    let mut found = None;
    for directory in current.ancestors() {
        let path = directory.join("pyproject.toml");
        if !path.is_file() {
            continue;
        }
        let root = read_project(path)?;
        if uv_config(&root)?
            .and_then(|config| config.get("workspace"))
            .is_some()
        {
            found = Some((directory, root));
            break;
        }
    }
    let (directory, root) =
        found.context("No [tool.uv.workspace] found in the current directory or its parents.")?;
    let config = uv_config(&root)?
        .and_then(|config| config.get("workspace"))
        .and_then(Item::as_table_like)
        .context("tool.uv.workspace must be a table.")?;
    let members = patterns(config, "members", true)?;
    let exclusions = patterns(config, "exclude", false)?;
    let mut excluded = HashSet::new();
    for pattern in exclusions {
        for path in expand_pattern(directory, &pattern)? {
            excluded.insert(std::fs::canonicalize(path)?);
        }
    }
    let root_path = std::fs::canonicalize(&root.path)?;
    let mut paths = BTreeSet::new();
    for pattern in members {
        let matches = expand_pattern(directory, &pattern)?;
        if matches.is_empty() && !pattern.contains(['*', '?', '[']) {
            bail!(
                "Workspace member not found: {}",
                directory.join(&pattern).display()
            );
        }
        for member in matches {
            let member = std::fs::canonicalize(member)?;
            if !excluded.contains(&member) {
                let path = member.join("pyproject.toml");
                // Keep missing files for read_project to report. Resolve existing file symlinks.
                paths.insert(std::fs::canonicalize(&path).unwrap_or(path));
            }
        }
    }
    paths.remove(&root_path);
    let mut projects = vec![root];
    for path in paths {
        projects.push(read_project(path)?);
    }
    Ok(projects)
}

fn configured_indexes(config: &dyn TableLike) -> Result<Vec<Index>> {
    let Some(items) = config.get("index") else {
        return Ok(Vec::new());
    };
    let entries: Vec<&dyn TableLike> = if let Some(items) = items.as_array_of_tables() {
        items.iter().map(|item| item as &dyn TableLike).collect()
    } else if let Some(items) = items.as_array() {
        items
            .iter()
            .map(|item| {
                item.as_inline_table()
                    .map(|item| item as &dyn TableLike)
                    .context("Each tool.uv.index entry needs a URL string.")
            })
            .collect::<Result<_>>()?
    } else {
        bail!("tool.uv.index must be an array of tables.");
    };
    entries
        .into_iter()
        .map(|item| {
            let url = item
                .get("url")
                .and_then(Item::as_str)
                .context("Each tool.uv.index entry needs a URL string.")?;
            let name = item
                .get("name")
                .map(|name| name.as_str().context("Index names must be strings."))
                .transpose()?;
            let flag = |key| -> Result<bool> {
                item.get(key)
                    .map(|value| {
                        value
                            .as_bool()
                            .context("Index explicit and default values must be booleans.")
                    })
                    .transpose()
                    .map(|value| value.unwrap_or(false))
            };
            let explicit = flag("explicit")?;
            let default = flag("default")?;
            let authenticate = match item.get("authenticate") {
                None => "auto",
                Some(value) => match value.as_str() {
                    Some(value @ ("auto" | "always" | "never")) => value,
                    _ => bail!("Index authenticate must be auto, always, or never."),
                },
            };
            Ok(Index {
                url: url.to_owned(),
                name: name.map(str::to_owned),
                explicit,
                default,
                authenticate: authenticate.to_owned(),
            })
        })
        .collect()
}

fn sources<'a>(root: &'a Project, project: &'a Project) -> Result<HashMap<String, &'a Item>> {
    let mut sources = HashMap::new();
    for project in [root, project] {
        let Some(entries) = uv_config(project)?.and_then(|config| config.get("sources")) else {
            continue;
        };
        let entries = entries
            .as_table_like()
            .context("tool.uv.sources must be a table.")?;
        for (name, value) in entries.iter() {
            sources.insert(normalize(name), value);
        }
    }
    Ok(sources)
}

fn source_entries(source: &Item) -> Vec<&dyn TableLike> {
    if let Some(entries) = source.as_array() {
        entries
            .iter()
            .filter_map(|entry| entry.as_inline_table().map(|entry| entry as &dyn TableLike))
            .collect()
    } else if let Some(entries) = source.as_array_of_tables() {
        entries
            .iter()
            .map(|entry| entry as &dyn TableLike)
            .collect()
    } else {
        source.as_table_like().into_iter().collect()
    }
}

/// Respect root sources and member overrides, including external workspace strings.
pub fn workspace_dependencies(root: &Project, project: &Project) -> Result<HashSet<String>> {
    let mut names = HashSet::new();
    for (name, source) in sources(root, project)? {
        for entry in source_entries(source) {
            if let Some(value) = entry.get("workspace") {
                let enabled = if let Some(value) = value.as_bool() {
                    value
                } else if let Some(value) = value.as_str() {
                    !value.is_empty()
                } else {
                    bail!("Source workspace must be a boolean or a string.");
                };
                if enabled {
                    names.insert(name.clone());
                }
            }
        }
    }
    Ok(names)
}

/// Member definitions replace root names without changing first-match order.
/// A selected named source disables public fallback; defaults are searched last.
pub fn package_indexes(
    root: &Project,
    project: &Project,
    name: &str,
    urls: &[String],
    section: &[String],
) -> Result<Vec<Index>> {
    if !urls.is_empty() {
        return Ok(urls
            .iter()
            .map(|url| Index {
                url: url.clone(),
                name: None,
                explicit: false,
                default: true,
                authenticate: "auto".to_owned(),
            })
            .collect());
    }
    // A Vec retains Python dict insertion order when a member replaces a root definition.
    let mut definitions: Vec<(String, Index)> = Vec::new();
    for project in [root, project] {
        if let Some(config) = uv_config(project)? {
            for index in configured_indexes(config)? {
                let key = index
                    .name
                    .as_deref()
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&index.url)
                    .to_owned();
                if let Some(slot) = definitions.iter_mut().find(|(name, _)| name == &key) {
                    slot.1 = index;
                } else {
                    definitions.push((key, index));
                }
            }
        }
    }
    let extra =
        if section.len() == 3 && section[0] == "project" && section[1] == "optional-dependencies" {
            Some(normalize(&section[2]))
        } else {
            None
        };
    let sources = sources(root, project)?;
    let mut names = HashSet::new();
    if let Some(source) = sources.get(&normalize(name)) {
        for entry in source_entries(source) {
            let Some(index) = entry.get("index") else {
                continue;
            };
            if let Some(value) = entry.get("extra") {
                let value = value
                    .as_str()
                    .context("Index source extra must be a string.")?;
                if extra.as_deref() != Some(normalize(value).as_str()) {
                    continue;
                }
            }
            let index = index
                .as_str()
                .context("Index source index must be a string.")?;
            if entry.contains_key("marker") {
                bail!("Marker-specific index routing for {name} is not supported.");
            }
            names.insert(index);
        }
    }
    if names.len() > 1 {
        bail!("Multiple indexes for one dependency occurrence of {name} are not supported.");
    }
    if let Some(index_name) = names.into_iter().next() {
        let Some((_, mut index)) = definitions.into_iter().find(|(key, _)| key == index_name)
        else {
            bail!("Undefined named index for {name}.");
        };
        index.explicit = false;
        index.default = true;
        return Ok(vec![index]);
    }
    let mut indexes: Vec<_> = definitions
        .into_iter()
        .map(|(_, index)| index)
        .filter(|index| !index.explicit || index.default)
        .collect();
    indexes.sort_by_key(|index| index.default);
    Ok(indexes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn project(directory: &Path, name: &str, text: &str) -> Project {
        let path = directory.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        read_project(path).unwrap()
    }

    fn section(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    #[test]
    fn reads_bytes_and_preserves_format() {
        let temp = TempDir::new().unwrap();
        let text = "# Comment\n[project]\nname = 'demo'\n";
        let loaded = project(temp.path(), "pyproject.toml", text);
        assert_eq!(loaded.original, text.as_bytes());
        assert_eq!(loaded.document.to_string(), text);
        assert_eq!(loaded.clone().path, loaded.path);
        for (name, bytes) in [
            ("invalid.toml", b"[project".as_slice()),
            ("utf8.toml", &[0xff]),
        ] {
            let path = temp.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            assert!(
                read_project(path)
                    .unwrap_err()
                    .to_string()
                    .contains("Cannot read")
            );
        }
        assert!(read_project(temp.path().join("missing.toml")).is_err());
    }

    #[test]
    fn discovers_root_sorted_unique_members_and_excludes() {
        let temp = TempDir::new().unwrap();
        let root = project(
            temp.path(),
            "pyproject.toml",
            "[tool.uv.workspace]\nmembers = ['packages/*', 'packages/a', '.']\nexclude = ['packages/ex*']\n",
        );
        let b = project(
            temp.path(),
            "packages/b/pyproject.toml",
            "[project]\nname = 'b'\n",
        );
        let a = project(
            temp.path(),
            "packages/a/pyproject.toml",
            "[project]\nname = 'a'\n",
        );
        project(temp.path(), "packages/excluded/pyproject.toml", "");
        std::fs::write(
            temp.path().join("packages/excluded/pyproject.toml"),
            "invalid TOML",
        )
        .unwrap();
        project(temp.path(), "unrelated/pyproject.toml", "");
        let start = a.path.parent().unwrap().join("src");
        std::fs::create_dir(&start).unwrap();
        let found = discover_projects(&start).unwrap();
        let paths: Vec<_> = found.into_iter().map(|project| project.path).collect();
        assert_eq!(
            paths,
            [
                root.path,
                std::fs::canonicalize(a.path).unwrap(),
                std::fs::canonicalize(b.path).unwrap(),
            ]
        );
    }

    #[test]
    fn nearest_workspace_and_recursive_globs() {
        let temp = TempDir::new().unwrap();
        project(
            temp.path(),
            "pyproject.toml",
            "[tool.uv.workspace]\nmembers = ['absent']\n",
        );
        let root = project(
            temp.path(),
            "inner/pyproject.toml",
            "[tool.uv.workspace]\nmembers = ['packages/**/app', 'empty*']\n",
        );
        let a = project(temp.path(), "inner/packages/app/pyproject.toml", "");
        let b = project(temp.path(), "inner/packages/nested/app/pyproject.toml", "");
        let found = discover_projects(b.path.parent().unwrap()).unwrap();
        assert_eq!(
            found.into_iter().map(|p| p.path).collect::<Vec<_>>(),
            [
                root.path,
                std::fs::canonicalize(a.path).unwrap(),
                std::fs::canonicalize(b.path).unwrap(),
            ]
        );
    }

    #[test]
    fn invalid_workspace_configuration_and_members_fail_without_writes() {
        let temp = TempDir::new().unwrap();
        for text in [
            "[tool.uv.workspace]\n",
            "[tool.uv.workspace]\nmembers = 'packages/*'\n",
            "[tool.uv.workspace]\nmembers = [1]\n",
            "[tool.uv.workspace]\nmembers = []\nexclude = 'packages/*'\n",
            "[tool.uv]\nworkspace = true\n",
            "[tool.uv.workspace]\nmembers = ['missing']\n",
        ] {
            let root = project(temp.path(), "pyproject.toml", text);
            assert!(discover_projects(temp.path()).is_err(), "{text}");
            assert_eq!(std::fs::read(root.path).unwrap(), text.as_bytes());
        }
        project(
            temp.path(),
            "pyproject.toml",
            "[tool.uv.workspace]\nmembers = ['member']\n",
        );
        std::fs::create_dir(temp.path().join("member")).unwrap();
        assert!(
            discover_projects(temp.path())
                .unwrap_err()
                .to_string()
                .contains("Cannot read")
        );
        std::fs::write(temp.path().join("member/pyproject.toml"), "[invalid").unwrap();
        assert!(discover_projects(temp.path()).is_err());
    }

    #[test]
    fn absent_workspace_and_invalid_ancestor_are_errors() {
        let temp = TempDir::new().unwrap();
        project(
            temp.path(),
            "pyproject.toml",
            "[project]\nname = 'standalone'\n",
        );
        assert!(
            discover_projects(temp.path())
                .unwrap_err()
                .to_string()
                .contains("No [tool.uv.workspace]")
        );
        std::fs::write(temp.path().join("pyproject.toml"), "[invalid").unwrap();
        let member = project(temp.path(), "member/pyproject.toml", "");
        assert!(discover_projects(member.path.parent().unwrap()).is_err());
        let member = project(
            temp.path(),
            "member/pyproject.toml",
            "[tool.uv.workspace]\nmembers = []\n",
        );
        let found = discover_projects(member.path.parent().unwrap()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, member.path);
    }

    #[test]
    fn glob_directory_characters_and_hidden_members() {
        let temp = TempDir::new().unwrap();
        let root = project(
            temp.path(),
            "workspace[1]/pyproject.toml",
            "[tool.uv.workspace]\nmembers = ['packages/*']\n",
        );
        let member = project(
            temp.path(),
            "workspace[1]/packages/.hidden/pyproject.toml",
            "",
        );
        let found = discover_projects(root.path.parent().unwrap()).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].path, std::fs::canonicalize(member.path).unwrap());
    }

    #[test]
    fn workspace_sources_merge_by_normalized_name() {
        let temp = TempDir::new().unwrap();
        let root = project(
            temp.path(),
            "root.toml",
            "[tool.uv.sources]\nBird_Feeder = {workspace = true}\nother = {workspace = true}\n",
        );
        let member = project(
            temp.path(),
            "member.toml",
            "[tool.uv.sources]\nbird-feeder = {}\npytest = [{workspace = true, marker = 'ignored'}]\ntyper = {workspace = '../../external'}\nno = {workspace = false}\nempty = {workspace = ''}\n",
        );
        assert_eq!(
            workspace_dependencies(&root, &member).unwrap(),
            HashSet::from(["other".to_owned(), "pytest".to_owned(), "typer".to_owned()])
        );
    }

    #[test]
    fn ordered_indexes_member_overrides_defaults_and_explicit() {
        let temp = TempDir::new().unwrap();
        let root = project(
            temp.path(),
            "root.toml",
            "[[tool.uv.index]]\nname = 'fallback'\nurl = 'https://fallback/simple'\ndefault = true\n[[tool.uv.index]]\nname = 'first'\nurl = 'https://root/simple'\n[[tool.uv.index]]\nname = 'hidden'\nurl = 'https://hidden/simple'\nexplicit = true\n[[tool.uv.index]]\nname = 'blocked'\nurl = 'https://blocked/simple'\nexplicit = true\ndefault = true\n[[tool.uv.index]]\nurl = 'https://unnamed/simple'\n[tool.uv.sources]\ndemo = {index = 'hidden'}\n",
        );
        let member = project(
            temp.path(),
            "member.toml",
            "[[tool.uv.index]]\nname = 'first'\nurl = 'https://member/simple'\n[[tool.uv.index]]\nname = 'last'\nurl = 'https://last/simple'\n[[tool.uv.index]]\nname = 'hidden'\nurl = 'https://member-hidden/simple'\nexplicit = true\nauthenticate = 'never'\n",
        );
        let indexes = package_indexes(&root, &member, "other", &[], &[]).unwrap();
        assert_eq!(
            indexes.iter().map(|i| i.url.as_str()).collect::<Vec<_>>(),
            [
                "https://member/simple",
                "https://unnamed/simple",
                "https://last/simple",
                "https://fallback/simple",
                "https://blocked/simple"
            ]
        );
        assert!(indexes.last().unwrap().explicit);
        let named = package_indexes(&root, &member, "demo", &[], &[]).unwrap();
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].name.as_deref(), Some("hidden"));
        assert_eq!(named[0].url, "https://member-hidden/simple");
        assert_eq!(named[0].authenticate, "never");
        assert!(!named[0].explicit);
        assert!(named[0].default);
    }

    const EXTRA_INDEXES: &str = "[[tool.uv.index]]\nname = 'cpu'\nurl = 'https://cpu/simple'\nexplicit = true\nauthenticate = 'always'\n[[tool.uv.index]]\nname = 'gpu'\nurl = 'https://gpu/simple'\nexplicit = true\n[tool.uv.sources]\n";

    #[test]
    fn extra_routing_is_per_occurrence_and_normalized() {
        let temp = TempDir::new().unwrap();
        let root = project(
            temp.path(),
            "root.toml",
            &format!(
                "{EXTRA_INDEXES}Demo_Pkg = [{{index = 'cpu', extra = 'CPU_cache'}}, {{index = 'gpu', extra = 'gpu'}}]\n"
            ),
        );
        let member = project(temp.path(), "member.toml", "");
        for parts in [
            &["project", "dependencies"][..],
            &["dependency-groups", "cpu-cache"],
            &[],
        ] {
            assert!(
                package_indexes(&root, &member, "demo-pkg", &[], &section(parts))
                    .unwrap()
                    .is_empty()
            );
        }
        for (extra, name) in [("cpu-cache", "cpu"), ("gpu", "gpu")] {
            let indexes = package_indexes(
                &root,
                &member,
                "demo-pkg",
                &[],
                &section(&["project", "optional-dependencies", extra]),
            )
            .unwrap();
            assert_eq!(indexes.len(), 1);
            assert_eq!(indexes[0].name.as_deref(), Some(name));
            assert!(indexes[0].default);
            assert!(!indexes[0].explicit);
            if name == "cpu" {
                assert_eq!(indexes[0].authenticate, "always");
            }
        }
        let override_member = project(
            temp.path(),
            "member.toml",
            "[tool.uv.sources]\ndemo-pkg = {index = 'gpu'}\n",
        );
        let indexes = package_indexes(&root, &override_member, "demo-pkg", &[], &[]).unwrap();
        assert_eq!(indexes[0].name.as_deref(), Some("gpu"));
    }

    #[test]
    fn ambiguous_marked_undefined_and_invalid_sources_fail() {
        let temp = TempDir::new().unwrap();
        for source in [
            "demo = [{index = 'cpu', extra = 'cpu'}, {index = 'gpu', extra = 'cpu'}]",
            "demo = {index = 'cpu', extra = 'cpu', marker = 'python_version < \\\"3.12\\\"'}",
            "demo = {index = 'cpu', extra = ['cpu']}",
            "demo = {index = ['cpu']}",
            "demo = {index = 'undefined'}",
        ] {
            let root = project(
                temp.path(),
                "root.toml",
                &format!("{EXTRA_INDEXES}{source}\n"),
            );
            assert!(
                package_indexes(
                    &root,
                    &root,
                    "demo",
                    &[],
                    &section(&["project", "optional-dependencies", "cpu"])
                )
                .is_err(),
                "{source}"
            );
        }
        let root = project(
            temp.path(),
            "root.toml",
            &format!(
                "{EXTRA_INDEXES}demo = {{index = 'cpu', extra = 'cpu', marker = 'ignored'}}\n"
            ),
        );
        assert!(
            package_indexes(&root, &root, "demo", &[], &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn strict_config_types_and_cli_override() {
        let temp = TempDir::new().unwrap();
        for text in [
            "tool = 1",
            "[tool]\nuv = false",
            "[tool.uv]\nsources = []",
            "[tool.uv]\nindex = 'invalid'",
            "[tool.uv]\nindex = [1]",
            "[[tool.uv.index]]\nname = 'internal'",
            "[[tool.uv.index]]\nurl = 3",
            "[[tool.uv.index]]\nname = 3\nurl = 'https://example/simple'",
            "[[tool.uv.index]]\nurl = 'https://example/simple'\nexplicit = 'true'",
            "[[tool.uv.index]]\nurl = 'https://example/simple'\ndefault = 1",
            "[[tool.uv.index]]\nurl = 'https://example/simple'\nauthenticate = false",
            "[[tool.uv.index]]\nurl = 'https://example/simple'\nauthenticate = ['never']",
            "[[tool.uv.index]]\nurl = 'https://example/simple'\nauthenticate = 'sometimes'",
        ] {
            let root = project(temp.path(), "root.toml", text);
            assert!(
                package_indexes(&root, &root, "demo", &[], &[]).is_err(),
                "{text}"
            );
            let indexes = package_indexes(
                &root,
                &root,
                "demo",
                &["https://cli/simple".to_owned()],
                &[],
            )
            .unwrap();
            assert_eq!(indexes.len(), 1);
            assert_eq!(indexes[0].url, "https://cli/simple");
            assert!(indexes[0].default);
        }
    }

    #[test]
    fn inline_tables_and_duplicate_routes_work() {
        let temp = TempDir::new().unwrap();
        let root = project(
            temp.path(),
            "root.toml",
            "tool = {uv = {index = [{name = 'cpu', url = 'https://cpu/simple'}], sources = {demo = [{index = 'cpu'}, {index = 'cpu'}]}}}",
        );
        let indexes = package_indexes(&root, &root, "demo", &[], &[]).unwrap();
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].name.as_deref(), Some("cpu"));
    }
}
