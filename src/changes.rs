use std::fs;

use anyhow::{Context, Result, anyhow, ensure};
use similar::TextDiff;
use toml_edit::{Document, Item, Value};

use crate::dependencies::{Dependency, Update, apply_update};
use crate::workspace::Project;

#[derive(Debug)]
pub struct Change<'a> {
    pub project: &'a Project,
    pub content: Vec<u8>,
    pub count: usize,
}

fn value_at<'a>(document: &'a Item, dependency: &Dependency) -> Result<&'a Value> {
    let mut item = document;
    for key in &dependency.section {
        item = item
            .get(key)
            .ok_or_else(|| anyhow!("Dependency section no longer exists"))?;
    }
    item.as_array()
        .and_then(|array| array.get(dependency.index))
        .context("Dependency occurrence no longer exists")
}

pub fn prepare_change<'a>(project: &'a Project, updates: &[Update]) -> Result<Change<'a>> {
    let original = std::str::from_utf8(&project.original)?;
    let parsed = Document::parse(original)?;
    let mut document = project.document.clone();
    let mut replacements = Vec::new();
    for update in updates {
        let span = value_at(parsed.as_item(), &update.dependency)?
            .span()
            .context("Missing dependency source span")?;
        apply_update(&mut document, update)?;
        let mut value = value_at(document.as_item(), &update.dependency)?.clone();
        value.decor_mut().clear();
        replacements.push((span, value.to_string()));
    }
    replacements.sort_by_key(|(span, _)| span.start);
    ensure!(
        replacements
            .windows(2)
            .all(|pair| pair[0].0.end <= pair[1].0.start),
        "Duplicate dependency update"
    );
    let mut content = original.to_owned();
    for (span, replacement) in replacements.into_iter().rev() {
        content.replace_range(span, &replacement);
    }
    Ok(Change {
        project,
        content: content.into_bytes(),
        count: updates.len(),
    })
}

/// Escape terminal controls without changing bytes written to the project.
pub fn safe_text(value: &str, multiline: bool) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if !character.is_control() || (multiline && matches!(character, '\n' | '\t')) {
            output.push(character);
        } else if (character as u32) < 256 {
            output.push_str(&format!("\\x{:02x}", character as u32));
        } else {
            output.extend(character.escape_default());
        }
    }
    output
}

pub fn preview_diff(change: &Change<'_>) -> String {
    let before = String::from_utf8_lossy(&change.project.original).replace("\r\n", "\n");
    let after = String::from_utf8_lossy(&change.content).replace("\r\n", "\n");
    let path = change.project.path.to_string_lossy();
    safe_text(
        &TextDiff::from_lines(&before, &after)
            .unified_diff()
            .header(&path, &path)
            .to_string(),
        true,
    )
}

pub fn check_snapshots(changes: &[Change<'_>]) -> Result<()> {
    for change in changes {
        let path = &change.project.path;
        let current = fs::read(path)
            .with_context(|| format!("Cannot read {} before writing", path.display()))?;
        ensure!(
            current == change.project.original,
            "{} changed during review. No files were written; run the command again.",
            path.display()
        );
    }
    Ok(())
}

pub fn write_changes(changes: &[Change<'_>]) -> Result<()> {
    check_snapshots(changes)?;
    for change in changes {
        crate::check_interrupt()?;
        fs::write(&change.project.path, &change.content).with_context(|| {
            format!(
                "Cannot write {}. Earlier successful updates remain; review affected files.",
                change.project.path.display()
            )
        })?;
        println!(
            "Updated {} dependencies in {}.",
            change.count,
            safe_text(&change.project.path.to_string_lossy(), false)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependencies::dependencies;
    use crate::workspace::read_project;

    #[test]
    fn edits_only_selected_occurrence_and_preserves_source_bytes() {
        for newline in ["\n", "\r\n"] {
            for quote in ["\"", "'", "\"\"\"", "'''"] {
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("pyproject.toml");
                let original = format!("[project]\ndependencies = [\"demo>=1.0\", {quote}demo>=1.0{quote}] # keep\n[dependency-groups]\ndev = [{{include-group = 'other'}}, 'demo>=1.0']").replace('\n', newline);
                fs::write(&path, &original).unwrap();
                let project = read_project(&path).unwrap();
                let update = Update {
                    dependency: dependencies(&project.document)[1].clone(),
                    replacement: "demo>=2.0".into(),
                    version: "2.0".parse().unwrap(),
                    release_link: None,
                    project_link: None,
                };
                let change = prepare_change(&project, &[update]).unwrap();
                let expected = original.replacen(
                    &format!("{quote}demo>=1.0{quote}]"),
                    &format!("{quote}demo>=2.0{quote}]"),
                    1,
                );
                assert_eq!(change.content, expected.as_bytes());
                assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
                assert!(preview_diff(&change).contains("+dependencies ="));
                check_snapshots(&[change]).unwrap();
            }
        }
    }

    #[test]
    fn snapshot_failure_prevents_all_writes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pyproject.toml");
        fs::write(&path, "[project]\ndependencies = []").unwrap();
        let project = read_project(&path).unwrap();
        let change = Change {
            project: &project,
            content: b"new".to_vec(),
            count: 1,
        };
        fs::write(&path, "user edit").unwrap();
        assert!(
            write_changes(&[change])
                .unwrap_err()
                .to_string()
                .contains("changed during review")
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "user edit");
    }

    #[test]
    fn diff_marks_missing_newlines_and_escapes_controls() {
        let project = Project {
            path: "\x1b[2Jpyproject.toml".into(),
            document: Default::default(),
            original: b"old\x07".to_vec(),
        };
        let change = Change {
            project: &project,
            content: b"new\x07".to_vec(),
            count: 1,
        };
        let diff = preview_diff(&change);
        assert!(!diff.contains('\x1b'));
        assert!(diff.contains("\\x07"));
        assert_eq!(diff.matches("\\ No newline at end of file").count(), 2);
    }
}
