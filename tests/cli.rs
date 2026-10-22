use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

struct IndexServer {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl IndexServer {
    fn new(packages: &[(&str, &[&str])]) -> Self {
        let routes: HashMap<String, String> = packages.iter().map(|(name, versions)| {
            (format!("/simple/{name}/"), json!({"files": versions.iter().map(|version| json!({"filename":format!("{name}-{version}-py3-none-any.whl"), "url":"unused", "yanked":false})).collect::<Vec<_>>()}).to_string())
        }).collect();
        Self::routes(routes)
    }

    fn routes(routes: HashMap<String, String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut request = [0; 16384];
                        let size = stream.read(&mut request).unwrap_or(0);
                        let path = String::from_utf8_lossy(&request[..size])
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("")
                            .to_owned();
                        let (status, body) = routes
                            .get(&path)
                            .map(|body| ("200 OK", body.as_str()))
                            .unwrap_or(("404 Not Found", "{}"));
                        let _ = write!(
                            stream,
                            "HTTP/1.1 {status}\r\nContent-Type: application/vnd.pypi.simple.v1+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url: format!("http://{address}/simple/"),
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for IndexServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(
            self.url
                .trim_start_matches("http://")
                .trim_end_matches("/simple/"),
        );
        self.thread.take().unwrap().join().unwrap();
    }
}

fn project(content: &str) -> TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("pyproject.toml"), content).unwrap();
    directory
}

fn command(directory: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_check-updates"));
    command.current_dir(directory).env("NO_COLOR", "1");
    command
}

fn run(directory: &TempDir, index: &IndexServer, args: &[&str]) -> Output {
    command(directory.path())
        .args(["--index", &index.url])
        .args(args)
        .output()
        .unwrap()
}

fn assert_code(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn json_contract_and_all_preview_modes_do_not_write() {
    let original =
        "[project]\nrequires-python = '>=3.10'\ndependencies = ['demo>=1.0', 'demo>=1.0']\n";
    let directory = project(original);
    let index = IndexServer::new(&[("demo", &["1.0", "1.2", "2.0rc1", "2.0"])]);
    for args in [
        vec!["--format", "json"],
        vec!["--check"],
        vec!["--diff"],
        vec!["--diff", "--check"],
        vec!["--format", "json", "--check"],
    ] {
        let output = run(&directory, &index, &args);
        assert_code(&output, if args.contains(&"--check") { 3 } else { 0 });
        if args.contains(&"json") {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["schema_version"], 1);
            assert_eq!(value["updates"].as_array().unwrap().len(), 2);
            for (index, update) in value["updates"].as_array().unwrap().iter().enumerate() {
                assert_eq!(update["file"], "pyproject.toml");
                assert_eq!(update["requirement"], "demo>=1.0");
                assert_eq!(update["replacement"], "demo>=2.0");
                assert_eq!(update["version"], "2.0");
                assert_eq!(update["section"], json!(["project", "dependencies"]));
                assert_eq!(update["index"], index);
                assert_eq!(update["change"], "major");
                assert!(
                    update["release_url"]
                        .as_str()
                        .unwrap()
                        .ends_with("/simple/demo/")
                );
                assert!(update["project_release_url"].is_null());
            }
        }
        if args.contains(&"--diff") {
            let diff = String::from_utf8_lossy(&output.stdout);
            assert!(diff.starts_with("--- pyproject.toml\n+++ pyproject.toml\n"));
            assert!(diff.contains("+'") || diff.contains("+dependencies"));
        }
        assert_eq!(
            fs::read_to_string(directory.path().join("pyproject.toml")).unwrap(),
            original
        );
    }
}

#[test]
fn update_filters_keep_comments_crlf_markers_and_upper_bounds() {
    let original = "[project]\r\nrequires-python='>=3.10'\r\ndependencies = [\"demo[foo]>=1.0, <2; python_version < '3.14'\", 'other>=1.0'] # comment\r\n[dependency-groups]\r\nDev_Group = ['demo>=1.0', {include-group='other'}]\r\n";
    let directory = project(original);
    let index = IndexServer::new(&[("demo", &["1.0", "1.0.2", "1.3", "2.0"])]);
    let output = run(
        &directory,
        &index,
        &[
            "-u",
            "--no-lock",
            "--include",
            "DEMO",
            "--runtime",
            "--target",
            "minor",
        ],
    );
    assert_code(&output, 0);
    assert_eq!(
        fs::read_to_string(directory.path().join("pyproject.toml")).unwrap(),
        original.replacen("demo[foo]>=1.0,", "demo[foo]>=1.3,", 1)
    );
    let output = run(
        &directory,
        &index,
        &[
            "-u",
            "--no-lock",
            "--group",
            "dev-group",
            "--target",
            "patch",
        ],
    );
    assert_code(&output, 0);
    assert!(
        fs::read_to_string(directory.path().join("pyproject.toml"))
            .unwrap()
            .contains("Dev_Group = ['demo>=1.0.2'")
    );
}

#[test]
fn lookup_failure_never_writes_partial_updates_or_partial_json() {
    let original = "[project]\ndependencies=['demo>=1.0','missing>=1']\n";
    let directory = project(original);
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    for args in [vec!["-u", "--no-lock"], vec!["--format", "json"]] {
        let output = run(&directory, &index, &args);
        assert_code(&output, 1);
        assert!(output.stdout.is_empty());
        assert_eq!(
            fs::read_to_string(directory.path().join("pyproject.toml")).unwrap(),
            original
        );
    }
}

#[test]
fn option_validation_runs_before_reading_project() {
    let directory = tempfile::tempdir().unwrap();
    for args in [
        vec!["-i"],
        vec!["-u", "-i"],
        vec!["--target", "bogus"],
        vec!["--format", "csv"],
        vec!["--no-lock"],
        vec!["--check", "-u"],
        vec!["--diff", "-u"],
        vec!["--format", "json", "-u"],
        vec!["--format", "json", "--diff"],
    ] {
        let output = command(directory.path()).args(&args).output().unwrap();
        assert_code(&output, 2);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("Cannot read"));
    }
    let output = command(directory.path()).arg("--version").output().unwrap();
    assert_code(&output, 0);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("check-updates {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn empty_scope_and_no_updates_have_stable_outputs() {
    let directory = project("[project]\ndependencies=['demo>=2.0']");
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    let output = run(&directory, &index, &["--check", "--format", "json"]);
    assert_code(&output, 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"schema_version":1,"updates":[]})
    );
    let output = run(&directory, &index, &["--diff"]);
    assert_code(&output, 0);
    assert!(output.stdout.is_empty());
    let output = run(&directory, &index, &["--include", "unknown"]);
    assert_code(&output, 0);
    assert!(String::from_utf8_lossy(&output.stdout).contains("No dependency updates available."));
}

#[test]
fn workspace_root_sources_exclusions_and_extra_routing() {
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    let root = project(&format!(
        "[project]\ndependencies=['demo>=1','member>=1']\n[tool.uv.workspace]\nmembers=['packages/*']\nexclude=['packages/excluded']\n[tool.uv.sources]\nmember={{workspace=true}}\n[[tool.uv.index]]\nname='private'\nurl='{}'\ndefault=true\n",
        index.url
    ));
    let member = root.path().join("packages/member");
    fs::create_dir_all(member.join("subdir")).unwrap();
    fs::create_dir_all(root.path().join("packages/excluded")).unwrap();
    fs::write(
        member.join("pyproject.toml"),
        "[project]\nname='member'\ndependencies=['demo>=1','member>=1']",
    )
    .unwrap();
    let output = command(&member.join("subdir"))
        .args(["-w", "--format", "json"])
        .output()
        .unwrap();
    assert_code(&output, 0);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["updates"].as_array().unwrap().len(), 2);
    assert!(String::from_utf8_lossy(&output.stderr).contains("workspace dependency"));
    let output = command(&member.join("subdir"))
        .args(["-w", "-u", "--no-lock"])
        .output()
        .unwrap();
    assert_code(&output, 0);
    assert!(
        fs::read_to_string(member.join("pyproject.toml"))
            .unwrap()
            .contains("demo>=2.0")
    );
}

#[test]
fn arbitrary_pins_are_skipped_without_blocking_other_updates() {
    let directory = project("[project]\ndependencies=['special===nightly','demo>=1']\n");
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    let output = run(&directory, &index, &["--format", "json"]);
    assert_code(&output, 0);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["updates"].as_array().unwrap().len(), 1);
    assert_eq!(value["updates"][0]["requirement"], "demo>=1");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Skipped special===nightly: unsupported version constraint")
    );
}

#[cfg(unix)]
#[test]
fn symlinked_manifest_locks_the_selected_directory_not_the_target_directory() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let target = project("[project]\ndependencies=['demo>=1']\n");
    let selected = tempfile::tempdir().unwrap();
    symlink(
        target.path().join("pyproject.toml"),
        selected.path().join("pyproject.toml"),
    )
    .unwrap();
    fs::write(selected.path().join("uv.lock"), "selected lock").unwrap();
    fs::write(target.path().join("uv.lock"), "target lock").unwrap();
    let bin = selected.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let uv = bin.join("uv");
    fs::write(&uv, "#!/bin/sh\nprintf 'locked' > uv.lock\n").unwrap();
    fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    let output = command(selected.path())
        .args(["-u", "--index", &index.url])
        .env("PATH", path)
        .output()
        .unwrap();
    assert_code(&output, 0);
    assert_eq!(
        fs::read_to_string(selected.path().join("uv.lock")).unwrap(),
        "locked"
    );
    assert_eq!(
        fs::read_to_string(target.path().join("uv.lock")).unwrap(),
        "target lock"
    );
}

#[test]
fn first_match_index_does_not_merge_versions() {
    let directory = project("[project]\ndependencies=['demo>=1']");
    let first = IndexServer::new(&[("demo", &["1.1"])]);
    let second = IndexServer::new(&[("demo", &["3.0"])]);
    let output = command(directory.path())
        .args([
            "--index",
            &first.url,
            "--index",
            &second.url,
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_code(&output, 0);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["updates"][0]["version"], "1.1");
}

#[cfg(unix)]
#[test]
fn automatic_lock_runs_once_after_writes_with_safe_environment() {
    use std::os::unix::fs::PermissionsExt;
    let directory = project("[project]\ndependencies=['demo>=1']\n");
    fs::write(directory.path().join("uv.lock"), "old lock").unwrap();
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let uv = bin.join("uv");
    fs::write(&uv, "#!/bin/sh\nset -eu\ntest \"$#\" = 1\ntest \"$1\" = lock\ntest -z \"${UV_PROJECT+x}\"\ntest -z \"${UV_FROZEN+x}\"\ntest \"$UV_INDEX_PRIVATE_PASSWORD\" = test-secret\nif read -r answer; then exit 90; fi\ngrep -q 'demo>=2.0' pyproject.toml\nprintf 'new lock' > uv.lock\nprintf 'called\\n' >> calls\nexit \"${LOCK_EXIT:-0}\"\n").unwrap();
    fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    for exit in [0, 7] {
        fs::write(
            directory.path().join("pyproject.toml"),
            "[project]\ndependencies=['demo>=1']\n",
        )
        .unwrap();
        let output = command(directory.path())
            .args(["--index", &index.url, "-u"])
            .env("PATH", &path)
            .env("UV_PROJECT", "/wrong")
            .env("UV_FROZEN", "1")
            .env("UV_INDEX_PRIVATE_PASSWORD", "test-secret")
            .env("LOCK_EXIT", exit.to_string())
            .output()
            .unwrap();
        assert_code(&output, if exit == 0 { 0 } else { 1 });
        assert_eq!(
            fs::read_to_string(directory.path().join("uv.lock")).unwrap(),
            "new lock"
        );
        assert!(
            fs::read_to_string(directory.path().join("pyproject.toml"))
                .unwrap()
                .contains("demo>=2.0")
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("test-secret"));
        if exit != 0 {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("uv.lock may be stale or changed")
            );
        }
    }
    assert_eq!(
        fs::read_to_string(directory.path().join("calls")).unwrap(),
        "called\ncalled\n"
    );
}
