//! Read locked registry versions without checking freshness or invoking uv.

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::Path;

use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item};
use url::Url;

use crate::dependencies::normalize;
use crate::indexes::Index;

fn report(state: &str, versions: Vec<Value>) -> Value {
    json!({"state": state, "freshness": "not_checked", "versions": versions})
}

// Reject links and special files. Nonblocking, no-follow opens also protect
// against a path being replaced with a FIFO or symlink before open on Unix.
fn read_lock(path: &Path) -> Result<String, &'static str> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => (),
        Ok(_) => return Err("unknown"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err("missing"),
        Err(_) => return Err("unknown"),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT opens a link itself, not its target.
        options.custom_flags(0x00200000);
    }
    let mut file = options.open(path).map_err(|_| "unknown")?;
    if !file.metadata().map_err(|_| "unknown")?.is_file() {
        return Err("unknown");
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidData {
            "malformed"
        } else {
            "unknown"
        }
    })?;
    Ok(text)
}

// Only compare index identity. Never return or print a source URL.
fn registry_identity(value: &str) -> Option<String> {
    if value.contains('\\') || value.chars().any(char::is_whitespace) {
        return None;
    }
    let mut url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
    {
        return None;
    }
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    Some(url.as_str().trim_end_matches('/').to_owned())
}

fn markers(package: &toml_edit::Table) -> Option<Vec<String>> {
    match package.get("resolution-markers") {
        None => Some(Vec::new()),
        Some(item) => item
            .as_array()?
            .iter()
            .map(|marker| marker.as_str().map(str::to_owned))
            .collect(),
    }
}

/// Inspect `root/uv.lock` for all normalized-name registry matches.
///
/// The caller supplies the allowed indexes, including public PyPI when allowed.
/// Entries remain separate, even when their versions are equal. A mismatch or
/// unknown source prevents an unqualified `unverified` state. Freshness is never
/// inferred from the lockfile or from a matching manifest requirement.
/// Unreadable files, links, special files, and absent package matches are unknown.
pub fn inspect(root: &Path, name: &str, indexes: &[Index]) -> Value {
    let text = match read_lock(&root.join("uv.lock")) {
        Ok(text) => text,
        Err(state) => return report(state, Vec::new()),
    };
    let document = match text.parse::<DocumentMut>() {
        Ok(document) => document,
        Err(_) => return report("malformed", Vec::new()),
    };
    let Some(version) = document.get("version").and_then(Item::as_integer) else {
        return report("malformed", Vec::new());
    };
    let revision = match document.get("revision") {
        None => 0,
        Some(item) => match item.as_integer() {
            Some(revision) if revision >= 0 => revision,
            _ => return report("malformed", Vec::new()),
        },
    };
    if version < 0 {
        return report("malformed", Vec::new());
    }
    if version != 1 || revision > 5 {
        return report("unsupported", Vec::new());
    }
    let packages = match document.get("package") {
        None => return report("unknown", Vec::new()),
        Some(item) => match item.as_array_of_tables() {
            Some(packages) => packages,
            None if item.as_array().is_some_and(|array| array.is_empty()) => {
                return report("unknown", Vec::new());
            }
            None => return report("malformed", Vec::new()),
        },
    };
    let allowed: Vec<_> = indexes
        .iter()
        .filter_map(|index| registry_identity(&index.url))
        .collect();
    let name = normalize(name);
    let mut versions = Vec::new();
    let mut unknown = false;
    let mut mismatch = false;
    for package in packages {
        let Some(package_name) = package.get("name").and_then(Item::as_str) else {
            return report("malformed", Vec::new());
        };
        if normalize(package_name) != name {
            continue;
        }
        let Some(markers) = markers(package) else {
            return report("malformed", Vec::new());
        };
        let Some(source) = package.get("source") else {
            unknown = true;
            continue;
        };
        let Some(source) = source.as_table_like() else {
            return report("malformed", Vec::new());
        };
        let Some(registry) = source.get("registry") else {
            unknown = true;
            continue;
        };
        if source.len() != 1 {
            unknown = true;
            continue;
        }
        let Some(registry) = registry.as_str() else {
            return report("malformed", Vec::new());
        };
        if Path::new(registry).is_absolute() {
            // uv also uses registry for absolute find-links directories. They
            // are valid lock records, but not HTTP index identities.
            unknown = true;
            continue;
        }
        let Some(registry) = registry_identity(registry) else {
            return report("malformed", Vec::new());
        };
        let Some(version) = package.get("version").and_then(Item::as_str) else {
            return report("malformed", Vec::new());
        };
        if version.parse::<pep440_rs::Version>().is_err() {
            return report("malformed", Vec::new());
        }
        if !allowed.contains(&registry) {
            mismatch = true;
            continue;
        }
        versions.push(json!({"version": version, "markers": markers, "source": "registry"}));
    }
    let state = if mismatch {
        "source_mismatch"
    } else if unknown || versions.is_empty() {
        "unknown"
    } else {
        "unverified"
    };
    report(state, versions)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PYPI: &str = "https://pypi.org/simple";

    fn indexes(urls: &[&str]) -> Vec<Index> {
        urls.iter()
            .map(|url| Index {
                url: (*url).to_owned(),
                ..Index::default()
            })
            .collect()
    }

    fn package(version: &str, registry: &str, markers: &[&str]) -> String {
        format!(
            "\n[[package]]\nname = 'Demo_Pkg'\nversion = '{version}'\nsource = {{registry = '{registry}'}}\nresolution-markers = {}\n",
            serde_json::to_string(markers).unwrap()
        )
    }

    fn fixture(text: &str, allowed: &[&str]) -> Value {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("uv.lock"), text).unwrap();
        let before = fs::read(temp.path().join("uv.lock")).unwrap();
        let result = inspect(temp.path(), "demo-pkg", &indexes(allowed));
        assert_eq!(result["freshness"], "not_checked");
        assert_eq!(fs::read(temp.path().join("uv.lock")).unwrap(), before);
        result
    }

    #[test]
    fn local_wheel_registry_is_unknown_not_malformed() {
        let temp = tempfile::tempdir().unwrap();
        let text = format!(
            "version=1\nrevision=5\n{}",
            package("2", temp.path().to_str().unwrap(), &[])
        );
        assert_eq!(fixture(&text, &[PYPI]), report("unknown", vec![]));
    }

    #[test]
    fn missing_lock_and_non_regular_lock_are_not_read() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(inspect(temp.path(), "demo", &[]), report("missing", vec![]));
        fs::create_dir(temp.path().join("uv.lock")).unwrap();
        assert_eq!(inspect(temp.path(), "demo", &[]), report("unknown", vec![]));
    }

    #[test]
    fn supports_version_one_with_absent_revision_and_revisions_zero_through_five() {
        for revision in [
            "",
            "revision = 0\n",
            "revision = 1\n",
            "revision = 2\n",
            "revision = 3\n",
            "revision = 4\n",
            "revision = 5\n",
        ] {
            let result = fixture(
                &format!("version = 1\n{revision}{}", package("1.2", PYPI, &[])),
                &[PYPI],
            );
            assert_eq!(
                result,
                report(
                    "unverified",
                    vec![json!({
                        "version": "1.2", "markers": [], "source": "registry"
                    })]
                )
            );
        }
    }

    #[test]
    fn unsupported_formats_are_not_interpreted_as_matches() {
        for header in [
            "version = 0",
            "version = 2",
            "version = 1\nrevision = 6",
            "version = 1\nrevision = 999",
        ] {
            assert_eq!(
                fixture(&format!("{header}\n{}", package("1", PYPI, &[])), &[PYPI]),
                report("unsupported", vec![])
            );
        }
    }

    #[test]
    fn malformed_headers_and_package_fields_do_not_return_partial_versions() {
        for text in [
            "[invalid",
            "",
            "version = '1'",
            "version = -1",
            "version = 1\nrevision = -1",
            "version = 1\nrevision = '3'",
            "version = 1\npackage = 'bad'",
            "version = 1\n[[package]]\nversion = '1'",
        ] {
            assert_eq!(
                fixture(text, &[PYPI]),
                report("malformed", vec![]),
                "{text}"
            );
        }
        for fields in [
            "version = 'not-a-version'\nsource = {registry = 'https://pypi.org/simple'}",
            "source = {registry = 'https://pypi.org/simple'}",
            "version = '1'\nsource = {registry = 42}",
            "version = '1'\nsource = {registry = 'not a URL'}",
            "version = '1'\nsource = false",
            "resolution-markers = ['valid', 42]",
            "resolution-markers = 'python_version < \"3.12\"'",
        ] {
            let text = format!(
                "version = 1\n{}\n[[package]]\nname = 'demo-pkg'\n{fields}",
                package("1", PYPI, &[])
            );
            assert_eq!(
                fixture(&text, &[PYPI]),
                report("malformed", vec![]),
                "{fields}"
            );
        }
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("uv.lock"), [0xff]).unwrap();
        assert_eq!(
            inspect(temp.path(), "demo", &[]),
            report("malformed", vec![])
        );
    }

    #[test]
    fn all_versions_and_equal_versions_with_different_markers_are_retained() {
        let first = "python_full_version < '3.12'";
        let second = "python_full_version >= '3.12'";
        let text = format!(
            "version = 1\nrevision = 3\nresolution-markers = [\"unrelated global marker\"]\n{}{}{}{}",
            package("1.0", PYPI, &[first]),
            package("2.0", PYPI, &[second]),
            package("2.0", "https://private.example/simple", &[first, second]),
            "\n[[package]]\nname = 'other'\nversion = '999'\nsource = {registry = 'https://elsewhere.example/simple'}"
        );
        let result = fixture(&text, &[PYPI, "https://private.example/simple"]);
        assert_eq!(result["state"], "unverified");
        assert_eq!(
            result["versions"],
            json!([
                {"version": "1.0", "markers": [first], "source": "registry"},
                {"version": "2.0", "markers": [second], "source": "registry"},
                {"version": "2.0", "markers": [first, second], "source": "registry"}
            ])
        );
    }

    #[test]
    fn equal_versions_from_one_registry_keep_each_marker_entry() {
        let first = "sys_platform == 'linux'";
        let second = "sys_platform == 'win32'";
        let text = format!(
            "version = 1\n{}{}",
            package("1", PYPI, &[first]),
            package("1", PYPI, &[second])
        );
        assert_eq!(
            fixture(&text, &[PYPI]),
            report(
                "unverified",
                vec![
                    json!({"version": "1", "markers": [first], "source": "registry"}),
                    json!({"version": "1", "markers": [second], "source": "registry"}),
                ]
            )
        );
    }

    #[test]
    fn global_resolution_markers_are_not_assigned_to_every_package() {
        let text = "version = 1\nresolution-markers = [\"sys_platform == 'linux'\"]\n\
            [[package]]\nname = 'demo-pkg'\nversion = '1'\n\
            source = {registry = 'https://pypi.org/simple'}";
        let result = fixture(text, &[PYPI]);
        assert_eq!(result["state"], "unverified");
        assert_eq!(result["versions"][0]["markers"], json!([]));
    }

    #[test]
    fn source_mismatch_is_explicit_even_with_an_allowed_match() {
        let rejected = package("99", "https://wrong.example/simple", &[]);
        for entries in [
            rejected.clone(),
            format!("{}{rejected}", package("1", PYPI, &[])),
        ] {
            let result = fixture(&format!("version = 1\n{entries}"), &[PYPI]);
            assert_eq!(result["state"], "source_mismatch");
            assert!(!result.to_string().contains("99"));
            assert!(!result.to_string().contains("wrong.example"));
        }
        assert_eq!(
            fixture(&format!("version = 1\n{}", package("1", PYPI, &[])), &[]),
            report("source_mismatch", vec![])
        );
    }

    #[test]
    fn registry_identity_removes_credentials_and_normalizes_urls_without_exposing_them() {
        let text = format!(
            "version = 1\n{}",
            package(
                "1",
                "https://lock-user:lock-secret@PRIVATE.example:443/simple///",
                &[]
            )
        );
        let result = fixture(
            &text,
            &["https://config-user:config-secret@private.example/simple/"],
        );
        assert_eq!(result["state"], "unverified");
        assert_eq!(result["versions"][0]["source"], "registry");
        for secret in [
            "lock-user",
            "lock-secret",
            "config-user",
            "config-secret",
            "private.example",
            "https://",
        ] {
            assert!(!result.to_string().contains(secret));
        }
        for different in [
            "http://private.example/simple",
            "https://private.example/other",
            "https://private.example:444/simple",
        ] {
            assert_eq!(fixture(&text, &[different])["state"], "source_mismatch");
        }
    }

    #[test]
    fn unknown_sources_and_absent_matches_do_not_select_an_arbitrary_version() {
        for source in [
            "",
            "source = {git = 'https://user:secret@example.test/repo'}",
            "source = {url = 'https://user:secret@example.test/demo.whl'}",
            "source = {editable = '.'}",
            "source = {directory = '../demo'}",
            "source = {future = 'secret'}",
            "source = {registry = 'https://pypi.org/simple', future = 'secret'}",
        ] {
            let text =
                format!("version = 1\n[[package]]\nname = 'Demo.Pkg'\nversion = '1'\n{source}");
            assert_eq!(fixture(&text, &[PYPI]), report("unknown", vec![]));
        }
        for text in [
            "version = 1",
            "version = 1\npackage = []",
            "version = 1\n[[package]]\nname = 'other'",
        ] {
            assert_eq!(fixture(text, &[PYPI]), report("unknown", vec![]));
        }
        let text = format!(
            "version = 1\n{}\n[[package]]\nname = 'demo-pkg'\nsource = {{future = 'secret'}}",
            package("1", PYPI, &[])
        );
        let result = fixture(&text, &[PYPI]);
        assert_eq!(result["state"], "unknown");
        assert_eq!(result["versions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn manifest_changes_do_not_imply_lock_freshness() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("uv.lock"),
            format!("version = 1\n{}", package("1", PYPI, &[])),
        )
        .unwrap();
        for requirement in ["demo-pkg==1", "demo-pkg>=99"] {
            fs::write(
                temp.path().join("pyproject.toml"),
                format!("[project]\ndependencies = ['{requirement}']"),
            )
            .unwrap();
            let result = inspect(temp.path(), "DEMO__..pkg", &indexes(&[PYPI]));
            assert_eq!(result["state"], "unverified");
            assert_eq!(result["freshness"], "not_checked");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_fifos_are_not_followed_or_opened() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::write(&target, format!("version = 1\n{}", package("1", PYPI, &[]))).unwrap();
        let lock = temp.path().join("uv.lock");
        symlink(&target, &lock).unwrap();
        assert_eq!(
            inspect(temp.path(), "demo-pkg", &indexes(&[PYPI])),
            report("unknown", vec![])
        );
        fs::remove_file(&lock).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(&lock)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            inspect(temp.path(), "demo-pkg", &indexes(&[PYPI])),
            report("unknown", vec![])
        );
    }
}
