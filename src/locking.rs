use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::changes::safe_text;
use crate::workspace::read_project;

fn parent_workspace(directory: &Path) -> Result<Option<PathBuf>> {
    for candidate in directory.ancestors() {
        let path = candidate.join("pyproject.toml");
        if !path.is_file() {
            continue;
        }
        let project = read_project(&path)?;
        if project
            .document
            .get("tool")
            .and_then(|v| v.get("uv"))
            .and_then(|v| v.get("workspace"))
            .is_some()
        {
            return Ok((candidate != directory).then(|| candidate.to_owned()));
        }
    }
    Ok(None)
}

pub fn lock_project(directory: &Path) -> Result<()> {
    let directory = directory.canonicalize()?;
    let recovery = format!(
        "Requirement files were updated. uv.lock may be stale or changed. Resolve the error and run uv lock in {}, or review and restore the affected files.",
        safe_text(&directory.to_string_lossy(), false)
    );
    lock_inner(&directory).with_context(|| recovery)
}

fn lock_inner(directory: &Path) -> Result<()> {
    match fs::metadata(directory.join("uv.lock")) {
        Ok(metadata) if metadata.is_file() => (),
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    if let Some(parent) = parent_workspace(directory)? {
        eprintln!(
            "Skipping uv lock: parent workspace found at {}. Requirement files were updated; lockfiles were not changed. Use -w to update and lock the workspace, or review and lock manually.",
            safe_text(&parent.to_string_lossy(), false)
        );
        return Ok(());
    }
    println!(
        "Running uv lock in {}...",
        safe_text(&directory.to_string_lossy(), false)
    );
    let mut child = Command::new("uv")
        .arg("lock")
        .current_dir(directory)
        .env_remove("UV_PROJECT")
        .env_remove("UV_FROZEN")
        .stdin(Stdio::null())
        .spawn()
        .context("uv lock could not start")?;
    loop {
        if let Err(error) = crate::check_interrupt() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(
                error.context("Interrupted during uv lock; lock completion is not confirmed")
            );
        }
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("uv lock failed ({status})");
            }
            println!("uv lock completed.");
            return Ok(());
        }
        sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_workspace_guard_stops_at_local_workspace() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("pyproject.toml"),
            "[tool.uv.workspace]\nmembers=[]",
        )
        .unwrap();
        let member = temp.path().join("member");
        fs::create_dir(&member).unwrap();
        fs::write(member.join("pyproject.toml"), "[project]\ndependencies=[]").unwrap();
        assert_eq!(
            parent_workspace(&member).unwrap(),
            Some(temp.path().to_owned())
        );
        fs::write(
            member.join("pyproject.toml"),
            "[tool.uv.workspace]\nmembers=[]",
        )
        .unwrap();
        assert!(parent_workspace(&member).unwrap().is_none());
    }

    #[test]
    fn missing_or_directory_lockfile_does_not_start_uv() {
        let temp = tempfile::tempdir().unwrap();
        lock_project(temp.path()).unwrap();
        fs::create_dir(temp.path().join("uv.lock")).unwrap();
        lock_project(temp.path()).unwrap();
    }
}
