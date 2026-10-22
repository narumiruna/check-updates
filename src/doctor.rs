//! Explicit installation and test execution in a temporary project.
//!
//! Doctor can use the network and executes project, dependency, and test code with
//! the user's permissions. The temporary copy is not an OS security sandbox.
//! Commands receive literal arguments, closed stdin, and a temporary environment.
//! Unix process groups are stopped after each command, including successful ones.
//! Other platforms are refused until equivalent process-tree cleanup is available.
//! Raw child output is bounded in memory and is not printed: it can contain secrets.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Child;
use std::process::{Command, ExitStatus, Stdio};
#[cfg(unix)]
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};

use crate::output::Pending;
use crate::verification::{self, Prepared, ResolvePolicy, Verified};
use crate::workspace::Project;
use crate::{changes, check_interrupt};

const MAX_ATTEMPTS: usize = 32; // Includes baseline and final confirmation.
const MAX_RUNTIME: Duration = Duration::from_secs(10 * 60);
#[cfg(unix)]
const OUTPUT_LIMIT: usize = 8192; // Per diagnostic stream.
const INSPECTION_BATCH_LIMIT: usize = 16 * 1024;
const INSPECTION_NAME_LIMIT: usize = 4096; // Serialized JSON bytes.
const INSPECTION_VERSION_LIMIT: usize = 1024; // Serialized JSON bytes.

/// Run only after explicit `--doctor -u` and a user-supplied test command.
/// Nothing is applied unless baseline and final combined tests pass.
pub fn run(
    root: &Path,
    projects: &[Project],
    pending: &Pending,
    policy: &ResolvePolicy,
    test_command: &[String],
) -> Result<()> {
    ensure!(
        !test_command.is_empty() && !test_command[0].is_empty(),
        "Doctor requires a nonempty test command"
    );
    ensure!(
        cfg!(unix),
        "Doctor is not supported on this platform: process-tree cleanup is required"
    );
    let count = pending.iter().map(|(_, updates)| updates.len()).sum();
    if count == 0 {
        return Ok(());
    }
    let deadline = Instant::now() + MAX_RUNTIME;
    check_budget(deadline)?;
    eprintln!(
        "Doctor executes code with your permissions; the temporary copy is not a security sandbox. Network access is possible."
    );
    eprintln!("Doctor: checking the baseline in an isolated environment.");
    let baseline = prepare_trial(root, projects, policy, deadline)?
        .resolve_with_deadline(&Vec::new(), deadline);
    check_budget(deadline)?;
    let baseline = baseline.map_err(|_| anyhow!("Doctor baseline resolution failed; no updates were applied. Child diagnostics suppressed to protect credentials"))?;
    ensure!(
        test_verified(&baseline, pending, false, test_command, deadline)?,
        "Doctor baseline installation or tests failed; no updates were applied"
    );
    let mut evaluate = |indices: &[usize]| -> Result<bool> {
        check_budget(deadline)?;
        baseline.check_snapshots_with_deadline(deadline)?;
        eprintln!(
            "Doctor: testing {} updates (one-based update rows: {}).",
            indices.len(),
            row_labels(indices)
        );
        let selected = subset(pending, indices);
        let prepared = prepare_trial(root, projects, policy, deadline)?;
        let verified = match prepared.resolve_with_deadline(&selected, deadline) {
            Ok(verified) => verified,
            Err(_) => {
                // Resolution conflicts are observations, not proof of a cause.
                // Do not expose resolver output, which can contain credentials.
                check_budget(deadline)?;
                baseline.check_snapshots_with_deadline(deadline)?;
                eprintln!(
                    "Doctor: subset resolution failed; child diagnostics suppressed to protect credentials."
                );
                return Ok(false);
            }
        };
        test_verified(&verified, &selected, true, test_command, deadline)
    };
    let selection = select_subset(count, MAX_ATTEMPTS, &mut evaluate)?;
    let Some(selection) = selection else {
        bail!(
            "Doctor found no passing nonempty subset within the attempt limit; combinations can remain untested. No updates were applied"
        );
    };
    eprintln!(
        "Doctor: {} of {count} updates passed an observed trial (one-based rows: {}); other combinations can remain untested. No unique failure cause was established.",
        selection.indices.len(),
        row_labels(&selection.indices)
    );
    // A fresh resolution and complete combined test are mandatory. Never apply
    // a union assembled from individually successful trials.
    check_budget(deadline)?;
    baseline.check_snapshots_with_deadline(deadline)?;
    let selected = subset(pending, &selection.indices);
    let verified =
        prepare_trial(root, projects, policy, deadline)?.resolve_with_deadline(&selected, deadline);
    check_budget(deadline)?;
    let verified = verified.map_err(|_| anyhow!("Doctor final resolution failed; no updates were applied. Child diagnostics suppressed to protect credentials"))?;
    ensure!(
        test_verified(&verified, &selected, true, test_command, deadline)?,
        "Doctor final combined tests failed; no updates were applied"
    );
    check_budget(deadline)?;
    baseline.check_snapshots_with_deadline(deadline)?;
    let changes = selected
        .iter()
        .map(|(project, updates)| changes::prepare_change(project, updates))
        .collect::<Result<Vec<_>>>()?;
    verified.apply(&changes)?;
    eprintln!(
        "Doctor applied {} tested updates after {} attempts. This is an observed test result, not a compatibility guarantee.",
        selection.indices.len(),
        selection.attempts + 1
    );
    Ok(())
}

fn prepare_trial(
    root: &Path,
    projects: &[Project],
    policy: &ResolvePolicy,
    deadline: Instant,
) -> Result<Prepared> {
    let prepared = verification::prepare_with_deadline(root, projects, policy, true, deadline);
    check_budget(deadline)?;
    prepared.map_err(|_| anyhow!("Doctor preparation failed; no updates were applied. Check supported static layouts, uv capabilities and resolver overrides. Child diagnostics suppressed to protect credentials"))
}

fn row_labels(indices: &[usize]) -> String {
    let mut labels = indices
        .iter()
        .take(MAX_ATTEMPTS)
        .map(|index| (index + 1).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if indices.len() > MAX_ATTEMPTS {
        labels.push_str(", ...");
    }
    labels
}

fn subset(pending: &Pending, indices: &[usize]) -> Pending {
    let mut index = 0;
    pending
        .iter()
        .filter_map(|(project, updates)| {
            let selected: Vec<_> = updates
                .iter()
                .filter_map(|update| {
                    let keep = indices.binary_search(&index).is_ok();
                    index += 1;
                    keep.then(|| update.clone())
                })
                .collect();
            (!selected.is_empty()).then(|| (project.clone(), selected))
        })
        .collect()
}

struct Selection {
    indices: Vec<usize>,
    attempts: usize,
}

/// Deterministic bounded observations, not diagnosis or exhaustive minimization.
/// Reserve one attempt for the final re-test; the baseline has already used one.
fn select_subset(
    count: usize,
    max_attempts: usize,
    evaluate: &mut impl FnMut(&[usize]) -> Result<bool>,
) -> Result<Option<Selection>> {
    ensure!(max_attempts >= 3, "Doctor attempt budget is too small");
    let mut attempts = 1;
    let mut seen = HashSet::new();
    let mut best: Option<Vec<usize>> = None;
    let mut trial = |indices: Vec<usize>| -> Result<Option<bool>> {
        if attempts >= max_attempts - 1 {
            return Ok(None);
        }
        if indices.is_empty() || !seen.insert(indices.clone()) {
            return Ok(Some(false));
        }
        attempts += 1;
        let passed = evaluate(&indices)?;
        if passed && best.as_ref().is_none_or(|best| indices.len() > best.len()) {
            best = Some(indices);
        }
        Ok(Some(passed))
    };
    let all: Vec<_> = (0..count).collect();
    // A passing batch already maximizes cardinality. On failure, try removals,
    // singles and pairs, including combinations that only pass together.
    if !trial(all)?.unwrap_or(false) {
        for removed in 0..count {
            if trial((0..count).filter(|index| *index != removed).collect())?.is_none() {
                break;
            }
        }
        for single in 0..count {
            if trial(vec![single])?.is_none() {
                break;
            }
        }
        'pairs: for first in 0..count {
            for second in first + 1..count {
                if trial(vec![first, second])?.is_none() {
                    break 'pairs;
                }
            }
        }
    }
    Ok(best.map(|indices| Selection { indices, attempts }))
}

fn check_budget(deadline: Instant) -> Result<()> {
    check_interrupt().context("Doctor evaluation incomplete; no updates were applied")?;
    ensure!(
        Instant::now() < deadline,
        "Doctor time limit reached; evaluation incomplete and no updates were applied"
    );
    Ok(())
}

// Tests must not change the exact resolver inputs that will be applied.
fn input_snapshots(directory: &Path, deadline: Instant) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    fn visit(
        directory: &Path,
        result: &mut Vec<(PathBuf, Vec<u8>)>,
        entries: &mut usize,
        bytes: &mut u64,
        deadline: Instant,
    ) -> Result<()> {
        for entry in fs::read_dir(directory)? {
            check_budget(deadline)?;
            let entry = entry?;
            let name = entry.file_name();
            if matches!(
                name.to_str(),
                Some(
                    ".venv"
                        | ".git"
                        | "target"
                        | "dist"
                        | "__pycache__"
                        | ".pytest_cache"
                        | ".mypy_cache"
                        | ".ruff_cache"
                        | ".cache"
                )
            ) {
                continue;
            }
            *entries += 1;
            ensure!(
                *entries <= 10_000,
                "Doctor temporary tree exceeds the input limit"
            );
            let kind = entry.file_type()?;
            if kind.is_dir() {
                visit(&entry.path(), result, entries, bytes, deadline)?;
            } else if matches!(
                name.to_str(),
                Some("pyproject.toml" | "uv.toml" | "uv.lock")
            ) {
                ensure!(
                    kind.is_file(),
                    "Doctor resolver inputs must remain regular files"
                );
                *bytes += entry.metadata()?.len();
                ensure!(
                    *bytes <= 64 * 1024 * 1024,
                    "Doctor resolver inputs exceed the size limit"
                );
                result.push((entry.path(), fs::read(entry.path())?));
            }
        }
        Ok(())
    }
    let mut result = Vec::new();
    visit(directory, &mut result, &mut 0, &mut 0, deadline)?;
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}

fn test_verified(
    verified: &Verified,
    selected: &Pending,
    exact: bool,
    test_command: &[String],
    deadline: Instant,
) -> Result<bool> {
    let directory = verified.directory();
    let inputs = input_snapshots(directory, deadline)?;
    let mut sync = isolated_command("uv", directory)?;
    sync.args([
        "sync",
        "--frozen",
        "--no-python-downloads",
        "--all-extras",
        "--all-groups",
        "--all-packages",
    ]);
    let sync_result = run_command(&mut sync, deadline, || check_budget(deadline))?;
    if !sync_result.status.success() {
        eprintln!(
            "Doctor: isolated installation failed ({}); child output suppressed to protect credentials.",
            sync_result.status
        );
        return Ok(false);
    }
    // A resolved marker branch is not necessarily installed on this interpreter.
    // Confirm selected distributions are present before describing them as tested.
    let names: Vec<_> = selected
        .iter()
        .flat_map(|(_, updates)| updates)
        .map(|update| {
            let requirement: pep508_rs::Requirement = update.dependency.requirement.parse()?;
            Ok((
                crate::dependencies::normalize(requirement.name.as_ref()),
                update.version.clone(),
            ))
        })
        .collect::<Result<_>>()?;
    if !inspect_distributions(directory, &names, exact, deadline)? {
        eprintln!(
            "Doctor: a selected distribution is absent or has a different version on this interpreter; this scope cannot be described as tested."
        );
        return Ok(false);
    }
    let mut test = isolated_command(&test_command[0], directory)?;
    test.args(&test_command[1..]);
    test.env("UV_NO_SYNC", "true").env("UV_FROZEN", "true");
    let result = run_command(&mut test, deadline, || check_budget(deadline))?;
    ensure!(
        input_snapshots(directory, deadline)? == inputs,
        "Doctor command changed temporary resolver inputs; no updates were applied"
    );
    if !result.status.success() {
        eprintln!(
            "Doctor: test command failed ({}); child output suppressed to protect credentials.",
            result.status
        );
    }
    Ok(result.status.success())
}

// Structured data has its own strict bound. Diagnostic retention must not cut JSON.
fn inspect_distributions(
    directory: &Path,
    names: &[(String, pep440_rs::Version)],
    exact: bool,
    deadline: Instant,
) -> Result<bool> {
    let script = r#"
import importlib.metadata as m, json, sys
records = []
size = 2
for name in json.loads(sys.argv[1]):
    try:
        version = m.version(name)
    except m.PackageNotFoundError:
        version = None
    encoded = json.dumps(version, ensure_ascii=False, separators=(',', ':'))
    if len(encoded.encode('utf-8')) > int(sys.argv[2]):
        sys.exit(80)
    record = '[' + json.dumps(name, ensure_ascii=False) + ',' + encoded + ']'
    size += len(record.encode('utf-8')) + bool(records)
    if size > int(sys.argv[3]):
        sys.exit(81)
    records.append(record)
sys.stdout.write('[' + ','.join(records) + ']')
"#;
    let mut start = 0;
    while start < names.len() {
        check_budget(deadline)?;
        let mut request = String::from("[");
        let mut response_bytes = 2; // Outer array brackets.
        let mut end = start;
        for (name, _) in &names[start..] {
            check_budget(deadline)?;
            // Check before serialization as well, so one large name is not copied.
            ensure!(
                name.len() <= INSPECTION_NAME_LIMIT,
                "Doctor distribution name exceeds the structured inspection size limit"
            );
            let encoded = serde_json::to_string(name)?;
            ensure!(
                encoded.len() <= INSPECTION_NAME_LIMIT,
                "Doctor distribution name exceeds the structured inspection size limit"
            );
            let comma = usize::from(end > start);
            // Each response record is [name, version]. Reserve the maximum
            // serialized version, but count the actual serialized name bytes.
            let record_bytes = encoded.len() + INSPECTION_VERSION_LIMIT + 3;
            if request.len() + comma + encoded.len() + 1 > INSPECTION_BATCH_LIMIT
                || response_bytes + comma + record_bytes > INSPECTION_BATCH_LIMIT
            {
                break;
            }
            if comma != 0 {
                request.push(',');
            }
            request.push_str(&encoded);
            response_bytes += comma + record_bytes;
            end += 1;
        }
        ensure!(
            end > start,
            "Doctor inspection record exceeds the batch size limit"
        );
        request.push(']');
        let mut command = isolated_command("python", directory)?;
        command
            .args(["-I", "-c", script])
            .arg(request)
            .arg(INSPECTION_VERSION_LIMIT.to_string())
            .arg(INSPECTION_BATCH_LIMIT.to_string());
        let result = run_command_with_stdout_limit(
            &mut command,
            deadline,
            || check_budget(deadline),
            Some(INSPECTION_BATCH_LIMIT),
        )?;
        match result.status.code() {
            Some(80) => {
                bail!("Doctor distribution version exceeds the structured inspection size limit")
            }
            Some(81) => bail!("Doctor structured inspection response exceeds the size limit"),
            _ => ensure!(
                result.status.success(),
                "Cannot inspect isolated Doctor distributions; no updates were applied"
            ),
        }
        // Do not include parse errors or child bytes in diagnostics.
        let installed: Vec<(String, Option<String>)> = serde_json::from_slice(&result._stdout)
            .map_err(|_| anyhow!("Invalid Doctor structured inspection response"))?;
        ensure!(
            installed.len() == end - start,
            "Incomplete Doctor structured inspection response"
        );
        for ((name, expected), (returned, version)) in names[start..end].iter().zip(installed) {
            ensure!(
                name == &returned,
                "Invalid Doctor structured inspection record"
            );
            let actual = version.and_then(|v| v.parse::<pep440_rs::Version>().ok());
            if actual.is_none() || exact && actual.as_ref() != Some(expected) {
                return Ok(false);
            }
        }
        start = end;
    }
    check_budget(deadline)?;
    Ok(true)
}

fn isolated_command(program: &str, directory: &Path) -> Result<Command> {
    let mut command = Command::new(program);
    command.current_dir(directory).stdin(Stdio::null());
    for variable in [
        "UV_PROJECT",
        "UV_WORKING_DIRECTORY",
        "UV_DIRECTORY",
        "UV_PROJECT_ENVIRONMENT",
        "UV_ACTIVE",
        "UV_FROZEN",
        "UV_LOCKED",
        "UV_NO_SYNC",
        "UV_NO_PROJECT",
        "UV_ENV_FILE",
        "VIRTUAL_ENV",
        "CONDA_PREFIX",
        "PYTHONPATH",
        "PYTHONHOME",
    ] {
        command.env_remove(variable);
    }
    let environment = directory.join(".venv");
    let bin = environment.join(if cfg!(windows) { "Scripts" } else { "bin" });
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))?;
    command
        .env("PATH", path)
        .env("UV_PROJECT_ENVIRONMENT", environment)
        .env("UV_PYTHON_DOWNLOADS", "never")
        .env("UV_NO_PROGRESS", "true")
        .env("PYTHONNOUSERSITE", "1");
    Ok(command)
}

struct CommandResult {
    status: ExitStatus,
    // Never render raw child output. Keep bounded bytes only while waiting.
    _stdout: Vec<u8>,
    _stderr: Vec<u8>,
}

#[cfg(unix)]
struct ChildGuard(Option<Child>);

#[cfg(unix)]
impl ChildGuard {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("Doctor child is still running")
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.0.take() {
            stop_child(&mut child);
        }
    }
}

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(unix)]
fn stop_child(child: &mut Child) {
    #[cfg(unix)]
    // SAFETY: the child created a new process group with its own PID. A negative
    // PID targets only that group, not the invoking process's group.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run_command(
    command: &mut Command,
    deadline: Instant,
    check: impl FnMut() -> Result<()>,
) -> Result<CommandResult> {
    run_command_with_stdout_limit(command, deadline, check, None)
}

#[cfg(unix)]
fn run_command_with_stdout_limit(
    command: &mut Command,
    deadline: Instant,
    mut check: impl FnMut() -> Result<()>,
    stdout_limit: Option<usize>,
) -> Result<CommandResult> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn reader(
        mut pipe: impl Read + AsRawFd + Send + 'static,
        done: Arc<AtomicBool>,
        limit: usize,
        strict: bool,
    ) -> Result<thread::JoinHandle<Result<Vec<u8>>>> {
        let fd = pipe.as_raw_fd();
        // SAFETY: fd is a live pipe owned by this reader; only status flags change.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        ensure!(
            flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
            "Cannot make Doctor diagnostics nonblocking"
        );
        Ok(thread::spawn(move || {
            let mut retained = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(size) => {
                        let keep = size.min(limit - retained.len());
                        ensure!(
                            !strict || keep == size,
                            "Doctor structured inspection response exceeds the size limit"
                        );
                        retained.extend_from_slice(&buffer[..keep]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if done.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) if strict => bail!("Cannot read Doctor structured inspection response"),
                    Err(_) => break,
                }
                // Drain buffered output after exit. Stopping immediately on
                // `done` can lose a short JSON response written during sleep.
                // The retention limit also bounds a daemon that keeps writing.
                if done.load(Ordering::Relaxed) && !strict && retained.len() == limit {
                    break;
                }
            }
            Ok(retained)
        }))
    }

    check()?;
    ensure!(
        Instant::now() < deadline,
        "Doctor command deadline reached; evaluation incomplete"
    );
    command
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child =
        ChildGuard(Some(command.spawn().context(
            "Doctor command could not start; no updates were applied",
        )?));
    let done = Arc::new(AtomicBool::new(false));
    let stdout = reader(
        child
            .child()
            .stdout
            .take()
            .context("Missing Doctor stdout")?,
        done.clone(),
        stdout_limit.unwrap_or(OUTPUT_LIMIT),
        stdout_limit.is_some(),
    )?;
    let stderr = match reader(
        child
            .child()
            .stderr
            .take()
            .context("Missing Doctor stderr")?,
        done.clone(),
        OUTPUT_LIMIT,
        false,
    ) {
        Ok(reader) => reader,
        Err(error) => {
            child.stop();
            done.store(true, Ordering::Relaxed);
            let _ = stdout.join();
            return Err(error);
        }
    };
    let result = (|| -> Result<ExitStatus> {
        loop {
            check()?;
            ensure!(
                Instant::now() < deadline,
                "Doctor time limit reached; evaluation incomplete and no updates were applied"
            );
            if let Some(status) = child.child().try_wait()? {
                return Ok(status);
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    // Kill descendants even when the direct child succeeded. Readers stop without
    // waiting for EOF: a daemon can retain a pipe after escaping the process group.
    child.stop();
    done.store(true, Ordering::Relaxed);
    let stdout = stdout.join();
    let stderr = stderr.join();
    Ok(CommandResult {
        status: result?,
        _stdout: stdout.map_err(|_| anyhow!("Doctor stdout reader failed"))??,
        _stderr: stderr.map_err(|_| anyhow!("Doctor stderr reader failed"))??,
    })
}

#[cfg(not(unix))]
fn run_command_with_stdout_limit(
    _command: &mut Command,
    _deadline: Instant,
    _check: impl FnMut() -> Result<()>,
    _stdout_limit: Option<usize>,
) -> Result<CommandResult> {
    bail!("Doctor requires supported process-tree cleanup")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn distribution_fixture(records: &[(String, String)]) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        // Use only the standard library and an offline interpreter. No package
        // install is needed: importlib.metadata reads these dist-info records.
        let script = r#"
import json, pathlib, subprocess, sys, venv
root = pathlib.Path(sys.argv[1]) / '.venv'
venv.EnvBuilder(with_pip=False).create(root)
python = root / 'bin' / 'python'
site = pathlib.Path(subprocess.check_output([str(python), '-I', '-c', "import sysconfig; print(sysconfig.get_path('purelib'))"], text=True).strip())
for name, version in json.loads(sys.argv[2]):
    info = site / (name.replace('-', '_') + '-1.0.dist-info')
    info.mkdir()
    (info / 'METADATA').write_text('Metadata-Version: 2.1\nName: ' + name + '\nVersion: ' + version + '\n', encoding='utf-8')
"#;
        let mut command = Command::new("uv");
        command
            .current_dir(temp.path())
            .args([
                "run",
                "--no-project",
                "--offline",
                "--no-python-downloads",
                "python",
                "-I",
                "-c",
                script,
            ])
            .arg(temp.path())
            .arg(serde_json::to_string(records).unwrap())
            .stdin(Stdio::null());
        let result = run_command(
            &mut command,
            Instant::now() + Duration::from_secs(30),
            || Ok(()),
        )
        .unwrap();
        assert!(result.status.success(), "offline Python fixture failed");
        temp
    }

    #[cfg(unix)]
    #[test]
    fn large_distribution_scope_exceeds_old_diagnostic_capture_but_is_inspected() {
        let records: Vec<_> = (0..400)
            .map(|index| {
                (
                    format!("doctor-inspection-package-{index:04}"),
                    "2.0".to_string(),
                )
            })
            .collect();
        let temp = distribution_fixture(&records);
        let names: Vec<_> = records
            .iter()
            .map(|(name, version)| (name.clone(), version.parse().unwrap()))
            .collect();
        let deadline = Instant::now() + Duration::from_secs(30);
        // Reproduce the old inspection command against real Python metadata.
        let script = "import importlib.metadata as m, json, sys; print(json.dumps({n:m.version(n) for n in json.loads(sys.argv[1])}))";
        let mut legacy = isolated_command("python", temp.path()).unwrap();
        legacy.args(["-I", "-c", script]).arg(
            serde_json::to_string(&records.iter().map(|(n, _)| n).collect::<Vec<_>>()).unwrap(),
        );
        let full = run_command_with_stdout_limit(&mut legacy, deadline, || Ok(()), Some(64 * 1024))
            .unwrap();
        assert!(full.status.success());
        assert!(full._stdout.len() > OUTPUT_LIMIT);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&full._stdout)
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            names.len()
        );
        let truncated = run_command(&mut legacy, deadline, || Ok(())).unwrap();
        assert!(truncated.status.success());
        assert_eq!(truncated._stdout.len(), OUTPUT_LIMIT);
        assert!(serde_json::from_slice::<serde_json::Value>(&truncated._stdout).is_err());
        assert!(inspect_distributions(temp.path(), &names, true, deadline).unwrap());
        // Check the last batch too, not only the first response.
        let mut mismatch = names.clone();
        mismatch.last_mut().unwrap().1 = "3.0".parse().unwrap();
        assert!(!inspect_distributions(temp.path(), &mismatch, true, deadline).unwrap());
        assert!(inspect_distributions(temp.path(), &mismatch, false, deadline).unwrap());
        mismatch.last_mut().unwrap().0 = "doctor-missing-distribution".into();
        for exact in [false, true] {
            assert!(!inspect_distributions(temp.path(), &mismatch, exact, deadline).unwrap());
        }
    }

    #[cfg(unix)]
    #[test]
    fn inspection_counts_long_names_and_versions_in_bytes() {
        // A full response batch exceeds the diagnostic limit. Names stay below
        // the filesystem component limit, but are much longer than normal names.
        let version = format!("2.0+{}", "a".repeat(900));
        let records: Vec<_> = (0..40)
            .map(|index| {
                (
                    format!("doctor-{index}-{}", "x".repeat(180)),
                    version.clone(),
                )
            })
            .collect();
        let temp = distribution_fixture(&records);
        let names: Vec<_> = records
            .iter()
            .map(|(name, version)| (name.clone(), version.parse().unwrap()))
            .collect();
        assert!(
            inspect_distributions(
                temp.path(),
                &names,
                true,
                Instant::now() + Duration::from_secs(30)
            )
            .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn oversized_inspection_names_and_versions_fail_clearly() {
        let deadline = Instant::now() + Duration::from_secs(30);
        // No command may start for an oversized first name.
        for name in [
            "x".repeat(INSPECTION_NAME_LIMIT + 1),
            "\n".repeat(INSPECTION_NAME_LIMIT / 2),
        ] {
            let error = inspect_distributions(
                Path::new("missing-doctor-root"),
                &[(name, "2.0".parse().unwrap())],
                true,
                deadline,
            )
            .unwrap_err();
            assert!(error.to_string().contains("name exceeds"), "{error:#}");
        }
        let temp = distribution_fixture(&[(
            "doctor-large-version".into(),
            format!("2.0+{}", "a".repeat(INSPECTION_VERSION_LIMIT)),
        )]);
        let error = inspect_distributions(
            temp.path(),
            &[("doctor-large-version".into(), "2.0".parse().unwrap())],
            true,
            deadline,
        )
        .unwrap_err();
        assert!(error.to_string().contains("version exceeds"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn structured_stdout_has_a_separate_fail_closed_bound() {
        for size in [
            OUTPUT_LIMIT + 1,
            INSPECTION_BATCH_LIMIT,
            INSPECTION_BATCH_LIMIT + 1,
        ] {
            let mut command = Command::new("head");
            command.args(["-c", &size.to_string(), "/dev/zero"]);
            let result = run_command_with_stdout_limit(
                &mut command,
                Instant::now() + Duration::from_secs(3),
                || Ok(()),
                Some(INSPECTION_BATCH_LIMIT),
            );
            if size <= INSPECTION_BATCH_LIMIT {
                assert_eq!(result.unwrap()._stdout.len(), size);
            } else {
                assert!(
                    result
                        .err()
                        .unwrap()
                        .to_string()
                        .contains("response exceeds")
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn inspection_uses_the_existing_deadline() {
        let error = inspect_distributions(
            Path::new("missing-doctor-root"),
            &[("doctor-demo".into(), "2.0".parse().unwrap())],
            true,
            Instant::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("time limit"));
    }

    #[cfg(unix)]
    #[test]
    fn short_output_written_just_before_exit_is_fully_drained() {
        for _ in 0..30 {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 0.009; printf '{\"version\":\"2.0\"}'"]);
            let result = run_command(
                &mut command,
                Instant::now() + Duration::from_secs(2),
                || Ok(()),
            )
            .unwrap();
            assert!(result.status.success());
            assert_eq!(result._stdout, b"{\"version\":\"2.0\"}");
        }
    }

    #[test]
    fn individually_passing_updates_are_not_combined_without_testing() {
        let mut trials = Vec::new();
        let selected = select_subset(2, 32, &mut |indices| {
            trials.push(indices.to_vec());
            Ok(indices.len() == 1)
        })
        .unwrap()
        .unwrap();
        assert_eq!(selected.indices, [1]);
        assert_eq!(trials, [vec![0, 1], vec![1], vec![0]]);
    }

    #[test]
    fn pair_that_only_passes_together_is_observed() {
        let selected = select_subset(4, 32, &mut |indices| Ok(indices == [0, 1]))
            .unwrap()
            .unwrap();
        assert_eq!(selected.indices, [0, 1]);
    }

    #[test]
    fn attempts_are_bounded_and_final_confirmation_is_reserved() {
        let mut calls = 0;
        assert!(
            select_subset(100, 7, &mut |_| {
                calls += 1;
                Ok(false)
            })
            .unwrap()
            .is_none()
        );
        assert_eq!(calls, 5); // baseline + 5 trials + final = 7
        assert!(select_subset(1, 2, &mut |_| Ok(true)).is_err());
    }

    #[test]
    fn passing_batch_is_the_best_observed_subset() {
        let mut calls = 0;
        let selected = select_subset(3, 32, &mut |_| {
            calls += 1;
            Ok(true)
        })
        .unwrap()
        .unwrap();
        assert_eq!(selected.indices, [0, 1, 2]);
        assert_eq!(calls, 1);
    }

    #[test]
    fn evaluation_errors_stop_the_search() {
        let mut calls = 0;
        assert!(
            select_subset(3, 32, &mut |_| {
                calls += 1;
                bail!("cancelled")
            })
            .is_err()
        );
        assert_eq!(calls, 1);
    }

    #[cfg(not(unix))]
    #[test]
    fn unsupported_platform_is_refused_before_launch() {
        let error = run(
            Path::new("missing-doctor-root"),
            &[],
            &Vec::new(),
            &ResolvePolicy::default(),
            &["missing-doctor-command".into()],
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("process-tree cleanup is required")
        );
    }

    #[cfg(unix)]
    #[test]
    fn empty_candidates_do_not_start_any_commands() {
        run(
            Path::new("missing-doctor-root"),
            &[],
            &Vec::new(),
            &ResolvePolicy::default(),
            &["missing-doctor-command".into()],
        )
        .unwrap();
    }

    #[test]
    fn environment_overrides_cannot_select_the_user_environment() {
        let temp = tempfile::tempdir().unwrap();
        let command = isolated_command("uv", temp.path()).unwrap();
        let environment = command
            .get_envs()
            .collect::<std::collections::BTreeMap<_, _>>();
        for key in [
            "UV_PROJECT",
            "UV_ACTIVE",
            "VIRTUAL_ENV",
            "PYTHONPATH",
            "PYTHONHOME",
            "CONDA_PREFIX",
        ] {
            assert_eq!(environment.get(std::ffi::OsStr::new(key)), Some(&None));
        }
        assert_eq!(
            environment.get(std::ffi::OsStr::new("UV_PROJECT_ENVIRONMENT")),
            Some(&Some(temp.path().join(".venv").as_os_str()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn successful_child_cannot_leave_background_workers() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("worker");
        let mut command = Command::new("sh");
        command
            .args(["-c", "(sleep 0.2; touch \"$1\") & exit 0", "test"])
            .arg(&marker);
        assert!(
            run_command(
                &mut command,
                Instant::now() + Duration::from_secs(3),
                || Ok(())
            )
            .unwrap()
            .status
            .success()
        );
        thread::sleep(Duration::from_millis(300));
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn commands_receive_literal_arguments_closed_stdin_and_bounded_output() {
        let mut command = Command::new("sh");
        command.args(["-c", "test \"$1\" = 'literal;not-a-command' || exit 2; read x && exit 3; head -c 100000 /dev/zero; head -c 100000 /dev/zero >&2", "test", "literal;not-a-command"]).stdin(Stdio::null());
        let result = run_command(
            &mut command,
            Instant::now() + Duration::from_secs(5),
            || Ok(()),
        )
        .unwrap();
        assert!(result.status.success());
        assert!(result._stdout.len() <= OUTPUT_LIMIT);
        assert!(result._stderr.len() <= OUTPUT_LIMIT);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_and_cancellation_stop_commands_and_descendants() {
        for (cancel, stdout_limit) in [
            (false, None),
            (true, None),
            (false, Some(INSPECTION_BATCH_LIMIT)),
            (true, Some(INSPECTION_BATCH_LIMIT)),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let marker = temp.path().join("escaped");
            let mut command = Command::new("sh");
            command
                .args(["-c", "(sleep 0.5; touch \"$1\") & wait", "test"])
                .arg(&marker);
            let start = Instant::now();
            let result = run_command_with_stdout_limit(
                &mut command,
                start + Duration::from_millis(150),
                || {
                    if cancel && start.elapsed() > Duration::from_millis(40) {
                        bail!("cancelled")
                    }
                    Ok(())
                },
                stdout_limit,
            );
            assert!(result.is_err());
            assert!(start.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(600));
            assert!(!marker.exists());
        }
    }
}
