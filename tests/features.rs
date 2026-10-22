//! CLI policy contracts. All metadata comes from a local, metadata-only index.
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

struct IndexServer {
    url: String,
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl IndexServer {
    fn new(packages: &[(&str, &[&str])]) -> Self {
        Self::metadata(
            &packages
                .iter()
                .map(|(name, versions)| {
                    (*name, versions.iter().map(|v| artifact(name, v)).collect())
                })
                .collect::<Vec<_>>(),
        )
    }

    fn metadata(packages: &[(&str, Vec<Value>)]) -> Self {
        Self::routes(
            packages
                .iter()
                .map(|(name, files)| {
                    (
                        format!("/simple/{name}/"),
                        json!({"files": files}).to_string(),
                    )
                })
                .collect(),
        )
    }

    fn routes(routes: HashMap<String, String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let thread = thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
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
                        if path.is_empty() {
                            continue;
                        }
                        recorded.lock().unwrap().push(path.clone());
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
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url: format!("http://{address}/simple/"),
            address,
            stop,
            requests,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for IndexServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.address);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn artifact(name: &str, version: &str) -> Value {
    json!({"filename": format!("{}-{version}-py3-none-any.whl", name.replace('-', "_")), "url": "unused", "yanked": false})
}

fn project(content: &str) -> TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("pyproject.toml"), content).unwrap();
    directory
}

fn command(directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_check-updates"));
    command
        .current_dir(directory)
        .stdin(Stdio::null())
        .env("NO_COLOR", "1")
        .env("UV_OFFLINE", "1")
        .env("UV_PYTHON_DOWNLOADS", "never")
        .env_remove("PYTHONPATH")
        .env_remove("PYTHONHOME");
    command
}

fn run(directory: &TempDir, index: &IndexServer, args: &[&str]) -> Output {
    command(directory.path())
        .args(["--index", &index.url, "--no-cache"])
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

fn report(directory: &TempDir, index: &IndexServer, args: &[&str], code: i32) -> Value {
    let mut flags = vec!["--explain", "--format", "json"];
    flags.extend_from_slice(args);
    let output = run(directory, index, &flags);
    assert_code(&output, code);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 2);
    assert!(value["updates"].is_array());
    assert!(value["decisions"].is_array());
    value
}

fn unchanged(directory: &TempDir, original: &str) {
    assert_eq!(
        fs::read_to_string(directory.path().join("pyproject.toml")).unwrap(),
        original
    );
    assert!(!directory.path().join(".venv").exists());
    assert!(!directory.path().join("uv.lock").exists());
}

fn configured(index: &IndexServer) -> String {
    format!(
        "\n[[tool.uv.index]]\nname='fixture'\nurl='{}'\ndefault=true\n",
        index.url
    )
}

#[test]
fn explain_schema_two_keeps_occurrence_identity_and_selected_and_blocked_versions() {
    let original = "[project]\nrequires-python='>=3.10'\ndependencies=['demo>=1.0,<2', 'demo>=1.0,<2', 'current>=2.0', 'blocked>=1.0,<2']\n";
    let directory = project(original);
    let index = IndexServer::new(&[
        ("demo", &["1.5", "2.0"]),
        ("current", &["2.0"]),
        ("blocked", &["2.0"]),
    ]);
    let value = report(&directory, &index, &["--check"], 3);
    assert_eq!(value.as_object().unwrap().len(), 3);
    assert_eq!(value["updates"].as_array().unwrap().len(), 2);
    let rows = value["decisions"].as_array().unwrap();
    assert_eq!(rows.len(), 4);
    for (position, row) in rows[..2].iter().enumerate() {
        assert_eq!(row["file"], "pyproject.toml");
        assert_eq!(row["section"], json!(["project", "dependencies"]));
        assert_eq!(row["index"], position);
        assert_eq!(row["requirement"], "demo>=1.0,<2");
        assert_eq!(row["latest"], "2.0");
        assert_eq!(row["selected"], "1.5");
        assert!(row["reason"].is_null());
        assert!(row["locked"].is_null());
        assert_eq!(
            row["rejected"],
            json!([{"version":"2.0","reasons":["upper_bound"]}])
        );
    }
    assert_eq!(rows[2]["reason"], "no_newer_release");
    assert_eq!(rows[2]["latest"], "2.0");
    assert!(rows[2]["selected"].is_null());
    assert_eq!(rows[3]["reason"], "blocked");
    assert!(rows[3]["selected"].is_null());
    assert_eq!(
        index.requests().len(),
        3,
        "duplicate occurrences share metadata"
    );
    unchanged(&directory, original);
}

#[test]
fn explain_reports_all_rejection_reasons_and_check_ignores_blocked_releases() {
    let directory =
        project("[project]\nrequires-python='>=3.10'\ndependencies=['demo>=1.0,<2,!=2.0']\n");
    let mut file = artifact("demo", "2.0");
    file["requires-python"] = json!(">=3.12");
    file["upload-time"] = json!("2999-01-01T00:00:00Z");
    file["yanked"] = json!("withdrawn");
    let index = IndexServer::metadata(&[("demo", vec![artifact("demo", "1.0"), file])]);
    let value = report(
        &directory,
        &index,
        &["--check", "--target", "patch", "--cooldown", "7d"],
        0,
    );
    assert_eq!(value["updates"], json!([]));
    assert_eq!(value["decisions"][0]["reason"], "blocked");
    assert_eq!(
        value["decisions"][0]["rejected"][0]["reasons"],
        json!([
            "upper_bound",
            "excluded",
            "target",
            "python",
            "yanked",
            "cooldown"
        ])
    );
}

#[test]
fn explain_distinguishes_prerelease_only_yanked_only_and_empty_indexes_from_default_errors() {
    for (files, prerelease, reason, rejection) in [
        (
            vec![artifact("demo", "2.0rc1")],
            json!("2.0rc1"),
            "blocked",
            Some("prerelease"),
        ),
        (
            {
                let mut file = artifact("demo", "2.0");
                file["yanked"] = json!("");
                vec![file]
            },
            Value::Null,
            "blocked",
            Some("yanked"),
        ),
        (vec![], Value::Null, "no_newer_release", None),
    ] {
        let directory = project("[project]\ndependencies=['demo>=1']\n");
        let index = IndexServer::metadata(&[("demo", files)]);
        let value = report(&directory, &index, &["--check"], 0);
        let row = &value["decisions"][0];
        assert!(row["latest"].is_null());
        assert!(row["selected"].is_null());
        assert_eq!(row["prerelease"], prerelease);
        assert_eq!(row["reason"], reason);
        assert_eq!(value["updates"], json!([]));
        if let Some(rejection) = rejection {
            assert_eq!(row["rejected"][0]["reasons"], json!([rejection]));
        } else {
            assert_eq!(row["rejected"], json!([]));
        }
        let output = run(&directory, &index, &["--format", "json"]);
        assert_code(&output, 1);
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("No stable, non-yanked release"));
    }
}

#[test]
fn malformed_or_failed_lookup_never_emits_partial_extended_json_or_writes() {
    for body in [
        None,
        Some("not json"),
        Some("{}"),
        Some("{\"files\":{}}"),
        Some("{\"files\":[1]}"),
    ] {
        let original = "[project]\ndependencies=['good>=1', 'bad>=1']\n";
        let directory = project(original);
        let mut routes = HashMap::from([(
            "/simple/good/".into(),
            json!({"files":[artifact("good", "2.0")]}).to_string(),
        )]);
        if let Some(body) = body {
            routes.insert("/simple/bad/".into(), body.into());
        }
        let index = IndexServer::routes(routes);
        for args in [
            vec!["--explain", "--format", "json"],
            vec!["--format", "json"],
            vec!["-u", "--no-lock"],
        ] {
            let output = run(&directory, &index, &args);
            assert_code(&output, 1);
            assert!(output.stdout.is_empty());
            unchanged(&directory, original);
        }
    }
}

#[test]
fn unknown_metadata_and_skips_are_explicit_without_looking_up_unsupported_requirements() {
    let original = "[project]\nrequires-python='>=3.10'\ndependencies=['demo>=1', 'special===nightly', 'direct @ https://invalid.example/direct.whl']\n";
    let directory = project(original);
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    let value = report(&directory, &index, &[], 0);
    assert_eq!(
        value["decisions"][0]["selected"], "2.0",
        "unknown Python metadata preserves default eligibility"
    );
    for row in &value["decisions"].as_array().unwrap()[1..] {
        assert_eq!(row["reason"], "unsupported_requirement");
        for field in ["latest", "selected", "prerelease", "locked"] {
            assert!(row[field].is_null());
        }
        assert!(row["detail"].is_string());
        assert_eq!(row["rejected"], json!([]));
    }
    assert_eq!(index.requests(), vec!["/simple/demo/"]);
    let value = report(&directory, &index, &["--cooldown", "7d"], 0);
    assert_eq!(value["updates"], json!([]));
    assert_eq!(
        value["decisions"][0]["rejected"][0]["reasons"],
        json!(["unknown_upload_time"])
    );
    unchanged(&directory, original);
}

#[test]
fn default_json_remains_exact_schema_one_even_with_selection_flags() {
    let directory = project("[project]\ndependencies=['demo>=1']\n");
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    let output = run(&directory, &index, &["--format", "json"]);
    assert_code(&output, 0);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value,
        json!({"schema_version":1,"updates":[{
            "file":"pyproject.toml", "section":["project","dependencies"], "index":0,
            "requirement":"demo>=1", "replacement":"demo>=2.0", "version":"2.0", "change":"major",
            "release_url":format!("{}demo/",index.url), "project_release_url":null
        }]})
    );
    let output = run(
        &directory,
        &index,
        &["--format", "json", "--minimal", "--check"],
    );
    assert_code(&output, 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"schema_version":1,"updates":[]})
    );
}

#[test]
fn minimal_suppresses_edits_check_exit_and_automatic_lock() {
    let original = "[project]\ndependencies=['demo>=1']\n";
    let directory = project(original);
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    let value = report(&directory, &index, &["--minimal", "--check"], 0);
    assert_eq!(value["decisions"][0]["reason"], "minimal");
    assert_eq!(value["updates"], json!([]));
    let lock = b"intentionally invalid lock: uv must not run";
    fs::write(directory.path().join("uv.lock"), lock).unwrap();
    let empty_path = tempfile::tempdir().unwrap();
    let output = command(directory.path())
        .env("PATH", empty_path.path())
        .args(["--index", &index.url, "--minimal", "-u"])
        .output()
        .unwrap();
    assert_code(&output, 0);
    assert_eq!(fs::read(directory.path().join("uv.lock")).unwrap(), lock);
    assert_eq!(
        fs::read_to_string(directory.path().join("pyproject.toml")).unwrap(),
        original
    );
}

#[test]
fn minimal_honors_full_ranges_pins_and_compatible_bounds() {
    for (requirement, target, expected) in [
        ("demo==1.0", "latest", Some("2.0")),
        ("demo~=1.0", "latest", Some("2.0")),
        ("demo~=1.0.0", "latest", Some("2.0")),
        ("demo~=1.0", "minor", None),
        ("demo~=1.0.0", "patch", None),
        ("demo>=1.0,<2,!=1.9", "latest", None),
    ] {
        let directory = project(&format!("[project]\ndependencies=['{requirement}']\n"));
        let index = IndexServer::new(&[("demo", &["1.0", "1.0.5", "1.8", "1.9", "2.0"])]);
        let value = report(&directory, &index, &["--minimal", "--target", target], 0);
        assert_eq!(
            value["decisions"][0]["selected"],
            expected.map_or(Value::Null, |v| json!(v))
        );
        if expected.is_none() {
            assert_eq!(value["decisions"][0]["reason"], "minimal");
            assert_eq!(value["updates"], json!([]));
        } else {
            assert_eq!(value["updates"].as_array().unwrap().len(), 1);
        }
    }
}

#[test]
fn cooldown_falls_back_to_old_artifact_and_rejects_future_and_unknown_times() {
    let directory = project("[project]\ndependencies=['demo>=1']\n");
    let files = [
        ("1.5", Some("2000-01-01T00:00:00Z")),
        ("2.0", Some("2999-01-01T00:00:00Z")),
        ("3.0", None),
        ("4.0", Some("bad time")),
    ]
    .into_iter()
    .map(|(version, timestamp)| {
        let mut file = artifact("demo", version);
        if let Some(timestamp) = timestamp {
            file["upload-time"] = json!(timestamp);
        }
        file
    })
    .collect();
    let index = IndexServer::metadata(&[("demo", files)]);
    let value = report(&directory, &index, &["--cooldown", "7d", "--check"], 3);
    assert_eq!(value["decisions"][0]["latest"], "4.0");
    assert_eq!(value["decisions"][0]["selected"], "1.5");
    assert_eq!(
        value["decisions"][0]["rejected"],
        json!([
            {"version":"4.0","reasons":["unknown_upload_time"]},
            {"version":"3.0","reasons":["unknown_upload_time"]},
            {"version":"2.0","reasons":["cooldown"]}
        ])
    );
    assert_eq!(
        report(&directory, &index, &[], 0)["decisions"][0]["selected"],
        "4.0"
    );
}

#[test]
fn cooldown_and_python_must_pass_on_the_same_artifact() {
    let directory = project("[project]\nrequires-python='>=3.10'\ndependencies=['demo>=1']\n");
    let mut old = artifact("demo", "2.0");
    old["upload-time"] = json!("2000-01-01T00:00:00Z");
    old["requires-python"] = json!(">=3.12");
    let mut future = artifact("demo", "2.0");
    future["upload-time"] = json!("2999-01-01T00:00:00Z");
    future["requires-python"] = json!(">=3.8");
    for files in [vec![old.clone(), future.clone()], vec![future, old]] {
        let index = IndexServer::metadata(&[("demo", files)]);
        let value = report(&directory, &index, &["--cooldown", "1d", "--check"], 0);
        assert!(value["decisions"][0]["selected"].is_null());
        assert_eq!(
            value["decisions"][0]["rejected"][0]["reasons"],
            json!(["python", "cooldown"])
        );
    }
}

#[test]
fn prerelease_cli_flags_override_config_and_development_releases_remain_blocked() {
    let index = IndexServer::new(&[("demo", &["1.5", "2.0rc1", "3.0.dev1"])]);
    for (config, args, selected) in [
        (false, vec![], "1.5"),
        (false, vec!["--pre"], "2.0rc1"),
        (true, vec![], "2.0rc1"),
        (true, vec!["--no-pre"], "1.5"),
    ] {
        let directory = project(&format!(
            "[project]\ndependencies=['demo>=1']\n[tool.check-updates]\npre={config}\n"
        ));
        let value = report(&directory, &index, &args, 0);
        assert_eq!(value["decisions"][0]["latest"], "1.5");
        assert_eq!(value["decisions"][0]["prerelease"], "2.0rc1");
        assert_eq!(value["decisions"][0]["selected"], selected);
        assert_eq!(
            value["decisions"][0]["rejected"][0]["reasons"],
            json!(["development"])
        );
    }
}

#[test]
fn exact_target_preserves_operator_bounds_markers_and_duplicate_occurrences() {
    let original = "[project]\ndependencies=[\"Demo[extra]>=1.0, <3, !=2.1; python_version < '3.14'\", 'demo>=1.0,<3'] # keep\n";
    let directory = project(original);
    let index = IndexServer::new(&[("demo", &["2.0", "2.1", "2.5"])]);
    let output = run(
        &directory,
        &index,
        &["--to", "DEMO==2.0", "--format", "json"],
    );
    assert_code(&output, 0);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["updates"].as_array().unwrap().len(), 2);
    assert_eq!(
        value["updates"][0]["replacement"],
        "Demo[extra]>=2.0, <3, !=2.1; python_version < '3.14'"
    );
    assert_eq!(value["updates"][1]["replacement"], "demo>=2.0,<3");
    unchanged(&directory, original);
    let output = run(
        &directory,
        &index,
        &["--to", "demo==2.0", "-u", "--no-lock"],
    );
    assert_code(&output, 0);
    assert_eq!(
        fs::read_to_string(directory.path().join("pyproject.toml")).unwrap(),
        original.replace(">=1.0", ">=2.0")
    );
}

#[test]
fn exact_targets_fail_closed_for_missing_yanked_excluded_out_of_bounds_and_downgrades() {
    let mut yanked = artifact("demo", "1.6");
    yanked["yanked"] = json!(true);
    let mut python = artifact("demo", "1.7");
    python["requires-python"] = json!(">=3.12");
    let mut recent = artifact("demo", "1.8");
    recent["upload-time"] = json!("2999-01-01T00:00:00Z");
    let index = IndexServer::metadata(&[(
        "demo",
        vec![
            artifact("demo", "0.9"),
            artifact("demo", "1.0"),
            artifact("demo", "1.5"),
            yanked,
            python,
            recent,
            artifact("demo", "2.0"),
        ],
    )]);
    for (target, extra) in [
        ("9.0", vec![]),
        ("1.6", vec![]),
        ("1.5", vec![]),
        ("2.0", vec![]),
        ("0.9", vec![]),
        ("1.0", vec![]),
        ("1.7", vec![]),
        ("1.8", vec!["--cooldown", "1d"]),
    ] {
        let original = "[project]\nrequires-python='>=3.10'\ndependencies=['demo>=1.0,<2,!=1.5']\n";
        let directory = project(original);
        let target = format!("demo=={target}");
        let mut args = vec!["-u", "--no-lock", "--to", &target];
        args.extend(extra);
        let output = run(&directory, &index, &args);
        assert_code(&output, 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("Explicit target"));
        unchanged(&directory, original);
    }
}

#[test]
fn explicit_targets_require_selected_dependencies_and_reject_duplicate_normalized_names() {
    let original = "[project]\ndependencies=['demo-pkg>=1']\n";
    let directory = project(original);
    let index = IndexServer::new(&[("demo-pkg", &["2.0"])]);
    for (args, code) in [
        (vec!["--to", "other==2.0"], 1),
        (vec!["--to", "demo-pkg==2.0", "--exclude", "demo-pkg"], 1),
        (vec!["--to", "Demo_Pkg==2.0", "--to", "demo.pkg==2.0"], 2),
        (vec!["--to", "demo-pkg==2.0", "--to", "demo-pkg==3.0"], 2),
    ] {
        let mut flags = vec!["-u", "--no-lock"];
        flags.extend(args);
        let output = run(&directory, &index, &flags);
        assert_code(&output, code);
        unchanged(&directory, original);
    }
}

#[test]
fn bound_preview_with_exact_target_keeps_comments_markers_and_exclusions() {
    let original = "[project]\ndependencies=[\"demo[extra]>=1.0, <2, !=2.1; python_version < '3.14'\", 'demo>=1.0,<=2'] # keep\n";
    let directory = project(original);
    let index = IndexServer::new(&[("demo", &["2.1", "2.5", "3.0"])]);
    let output = run(
        &directory,
        &index,
        &[
            "--bound",
            "demo<3",
            "--to",
            "demo==2.5",
            "--diff",
            "--check",
        ],
    );
    assert_code(&output, 3);
    let diff = String::from_utf8(output.stdout).unwrap();
    assert!(diff.starts_with("--- pyproject.toml\n+++ pyproject.toml\n"));
    assert!(diff.contains("demo[extra]>=2.5, <3, !=2.1; python_version < '3.14'"));
    assert!(diff.contains("'demo>=2.5,<3'] # keep"));
    assert!(!diff.contains("Dependency decisions"));
    unchanged(&directory, original);
}

#[test]
fn bound_rejects_unsupported_shapes_and_noop_does_not_trigger_verification() {
    let index = IndexServer::new(&[("demo", &["1.0"])]);
    for requirement in ["demo>=1.0", "demo~=1.0", "demo>=1.0,<2,<=3"] {
        let original = format!("[project]\ndependencies=['{requirement}']\n");
        let directory = project(&original);
        let output = run(&directory, &index, &["--bound", "demo<3", "--diff"]);
        assert_code(&output, 1);
        assert!(output.stdout.is_empty());
        unchanged(&directory, &original);
    }
    let original = "[project]\ndependencies=['demo>=1.0,<2']\n";
    let directory = project(original);
    for args in [
        vec!["--bound", "demo<2", "--diff", "--check"],
        vec!["--bound", "demo<2", "-u"],
    ] {
        let output = run(&directory, &index, &args);
        assert_code(&output, 0);
        unchanged(&directory, original);
    }
}

#[test]
fn bound_writes_require_successful_verification_and_leave_files_unchanged_on_failure() {
    let index = IndexServer::new(&[("demo", &["2.5"])]);
    let original = format!(
        "[project]\nname='fixture'\nversion='0.0.0'\ndependencies=['demo>=1,<2']\n{}",
        configured(&index)
    );
    let directory = project(&original);
    let output = command(directory.path())
        .args(["--bound", "demo<3", "-u"])
        .output()
        .unwrap();
    assert_code(&output, 1);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("existing regular uv.lock"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    unchanged(&directory, &original);
}

#[test]
fn policy_rejects_unknown_keys_types_side_effects_and_normalized_package_collisions_before_lookup()
{
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    for policy in [
        "upgrade=true",
        "verify=true",
        "doctor=true",
        "test-command=['true']",
        "unknown=1",
        "target='bogus'",
        "cooldown=7",
        "pre=1",
        "minimal='yes'",
        "include='demo'",
        "exclude=[1]",
        "include-glob=['[']",
        "packages.demo={upgrade=true}",
        "packages.DEMO_PKG={target='patch'}\npackages.demo-pkg={target='minor'}",
    ] {
        let original =
            format!("[project]\ndependencies=['demo>=1']\n[tool.check-updates]\n{policy}\n");
        let directory = project(&original);
        let output = run(&directory, &index, &["-u", "--no-lock"]);
        assert_code(&output, 1);
        assert!(output.stdout.is_empty());
        unchanged(&directory, &original);
    }
    assert!(index.requests().is_empty());
}

#[test]
fn declarative_policy_never_authorizes_writes_or_uv_and_cli_beats_package_policy() {
    let original = "[project]\ndependencies=['demo-pkg>=1.0.0', 'other>=1.0.0']\n[tool.check-updates]\ntarget='minor'\nminimal=true\n[tool.check-updates.packages.DEMO_PKG]\ntarget='patch'\n";
    let directory = project(original);
    let index = IndexServer::new(&[
        ("demo-pkg", &["1.0.1", "1.2", "2.0"]),
        ("other", &["1.0.1", "1.2", "2.0"]),
    ]);
    let value = report(&directory, &index, &[], 0);
    assert_eq!(value["updates"], json!([]));
    let value = report(&directory, &index, &["--no-minimal"], 0);
    assert_eq!(value["decisions"][0]["selected"], "1.0.1");
    assert_eq!(value["decisions"][1]["selected"], "1.2");
    let value = report(
        &directory,
        &index,
        &["--no-minimal", "--target", "latest"],
        0,
    );
    assert_eq!(value["decisions"][0]["selected"], "2.0");
    assert_eq!(value["decisions"][1]["selected"], "2.0");
    unchanged(&directory, original);
}

fn member(root: &TempDir, directory: &str, content: &str) {
    let path = root.path().join(directory);
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("pyproject.toml"), content).unwrap();
}

#[test]
fn workspace_policy_inherits_scalars_replaces_lists_and_keeps_occurrences_distinct() {
    let index = IndexServer::new(&[
        ("demo", &["1.0.1", "1.5", "2.0"]),
        ("other", &["1.5", "2.0"]),
    ]);
    let root = project(
        "[project]\nname='root'\ndependencies=['demo>=1.0']\n[tool.uv.workspace]\nmembers=['packages/*']\n[tool.check-updates]\ntarget='minor'\ninclude=['demo']\nexclude=['other']\n",
    );
    member(
        &root,
        "packages/a",
        "[project]\nname='a'\ndependencies=['demo>=1.0','other>=1.0']\n[tool.check-updates]\ninclude=['other']\nexclude=[]\n",
    );
    member(
        &root,
        "packages/b",
        "[project]\nname='b'\ndependencies=['demo>=1.0']\n[tool.check-updates]\ntarget='patch'\n",
    );
    let value = report(&root, &index, &["-w"], 0);
    let rows = value["decisions"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["requirement"], "demo>=1.0");
    assert_eq!(rows[0]["selected"], "1.5");
    let a = rows
        .iter()
        .find(|r| {
            r["file"]
                .as_str()
                .unwrap()
                .replace('\\', "/")
                .contains("packages/a/")
        })
        .unwrap();
    assert_eq!(a["requirement"], "other>=1.0");
    assert_eq!(a["selected"], "1.5");
    let b = rows
        .iter()
        .find(|r| {
            r["file"]
                .as_str()
                .unwrap()
                .replace('\\', "/")
                .contains("packages/b/")
        })
        .unwrap();
    assert_eq!(b["selected"], "1.0.1");
    let output = command(&root.path().join("packages/a"))
        .args(["--index", &index.url, "--format", "json"])
        .output()
        .unwrap();
    assert_code(&output, 0);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["updates"][0]["version"], "2.0",
        "without -w only local policy applies"
    );
}

#[test]
fn member_selection_retains_root_policy_indexes_and_workspace_sources_from_subdirectory() {
    let index = IndexServer::new(&[("demo", &["1.5", "2.0"])]);
    let root_text = format!(
        "[project]\nname='root'\ndependencies=['never-query>=1']\n[tool.uv.workspace]\nmembers=['packages/*']\n[tool.uv.sources]\nsibling={{workspace=true}}\n[tool.check-updates]\ntarget='minor'\n{}",
        configured(&index)
    );
    let root = project(&root_text);
    member(
        &root,
        "packages/selected",
        "[project]\nname='Selected_Member'\ndependencies=['demo>=1.0', 'sibling>=1']\n",
    );
    member(
        &root,
        "packages/sibling",
        "[project]\nname='sibling'\ndependencies=['never-query>=1']\n",
    );
    let subdir = root.path().join("packages/selected/subdir");
    fs::create_dir(&subdir).unwrap();
    let output = command(&subdir)
        .args([
            "-w",
            "--member",
            "selected.member",
            "--explain",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_code(&output, 0);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["decisions"].as_array().unwrap().len(), 2);
    assert_eq!(value["decisions"][0]["selected"], "1.5");
    assert_eq!(value["decisions"][1]["reason"], "workspace");
    assert_eq!(index.requests(), vec!["/simple/demo/"]);
    let output = command(root.path())
        .args(["-w", "--member", "selected-member", "-u", "--no-lock"])
        .output()
        .unwrap();
    assert_code(&output, 0);
    assert_eq!(
        fs::read_to_string(root.path().join("pyproject.toml")).unwrap(),
        root_text
    );
    assert!(
        fs::read_to_string(root.path().join("packages/selected/pyproject.toml"))
            .unwrap()
            .contains("demo>=1.5")
    );
    assert!(!root.path().join("uv.lock").exists());
}

#[test]
fn member_selection_rejects_unknown_ambiguous_excluded_and_nonworkspace_names_before_lookup() {
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    let root = project(
        "[project]\nname='root'\ndependencies=['demo>=1']\n[tool.uv.workspace]\nmembers=['packages/*']\nexclude=['packages/excluded']\n",
    );
    member(&root, "packages/a", "[project]\nname='same_name'\n");
    member(&root, "packages/b", "[project]\nname='same-name'\n");
    member(&root, "packages/excluded", "[project]\nname='excluded'\n");
    member(
        &root,
        "packages/unnamed",
        "[project]\ndependencies=['demo>=1']\n",
    );
    for name in ["unknown", "same.name", "excluded", "unnamed"] {
        let output = run(&root, &index, &["-w", "--member", name, "--format", "json"]);
        assert_code(&output, 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("unknown or ambiguous"));
        assert!(output.stdout.is_empty());
    }
    let single = project("[project]\nname='single'\ndependencies=['demo>=1']\n");
    let output = run(&single, &index, &["--member", "single"]);
    assert_code(&output, 2);
    let output = run(&single, &index, &["-w", "--member", "unknown"]);
    assert_code(&output, 1);
    assert!(index.requests().is_empty());
    let value = report(&root, &index, &["-w", "--member", "root"], 0);
    assert_eq!(value["decisions"].as_array().unwrap().len(), 1);
}

#[test]
fn exact_and_glob_includes_form_union_and_exclusions_and_sections_filter_before_lookup() {
    let original = "[project]\ndependencies=['pytest-good>=1', 'pytest-bad>=1', 'other>=1', 'unused>=1']\n[dependency-groups]\ndev=['pytest-dev>=1']\n";
    let directory = project(original);
    let index = IndexServer::new(&[("pytest-good", &["2.0"]), ("other", &["2.0"])]);
    let value = report(
        &directory,
        &index,
        &[
            "--include",
            "OTHER",
            "--include-glob",
            "PYTEST_*",
            "--exclude-glob",
            "*-bad",
            "--runtime",
        ],
        0,
    );
    assert_eq!(value["updates"].as_array().unwrap().len(), 2);
    assert_eq!(
        index.requests(),
        vec!["/simple/pytest-good/", "/simple/other/"]
    );
    let output = run(
        &directory,
        &index,
        &[
            "--include-glob",
            "pytest-*",
            "--exclude",
            "pytest-good",
            "--exclude-glob",
            "*-bad",
            "--runtime",
            "--format",
            "json",
        ],
    );
    assert_code(&output, 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["updates"],
        json!([])
    );
    assert_eq!(index.requests().len(), 2);
    unchanged(&directory, original);
}

#[test]
fn new_option_conflicts_and_invalid_durations_fail_before_project_io() {
    let empty = tempfile::tempdir().unwrap();
    for args in [
        vec!["--explain", "--diff"],
        vec!["--explain", "-u"],
        vec!["--show-locked", "-u"],
        vec!["--show-locked", "--diff"],
        vec!["--verify"],
        vec!["--verify", "-u", "--no-lock"],
        vec!["--bound", "demo<3", "-u", "--no-lock"],
        vec!["--pre", "--no-pre"],
        vec!["--minimal", "--no-minimal"],
        vec!["--member", "demo"],
        vec!["--doctor", "-u"],
        vec!["--doctor", "-u", "--verify", "--test-command", "unused"],
        vec!["--format", "json", "-u"],
        vec!["--include-glob", "["],
        vec!["--to", "demo>=2"],
        vec!["--to", "demo==2", "--to", "DEMO==3"],
        vec!["--bound", "demo>=3"],
    ] {
        let output = command(empty.path()).args(&args).output().unwrap();
        assert_code(&output, 2);
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("Cannot read"),
            "{args:?}"
        );
    }
    for duration in ["0d", "-1d", "1.5h", "1w", "99999999999999999999d"] {
        let output = command(empty.path())
            .arg(format!("--cooldown={duration}"))
            .output()
            .unwrap();
        assert_code(&output, 2);
    }
    assert_eq!(fs::read_dir(empty.path()).unwrap().count(), 0);
}

#[test]
fn verify_refuses_cli_index_mismatch_before_any_writes() {
    let original = "[project]\nname='fixture'\nversion='0.0.0'\ndependencies=['demo>=1']\n";
    let directory = project(original);
    let lock = b"original lock";
    fs::write(directory.path().join("uv.lock"), lock).unwrap();
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    let output = run(&directory, &index, &["--verify", "-u"]);
    assert_code(&output, 1);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--index only controls metadata lookup")
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("pyproject.toml")).unwrap(),
        original
    );
    assert_eq!(fs::read(directory.path().join("uv.lock")).unwrap(), lock);
    assert!(!directory.path().join(".venv").exists());
}

#[test]
fn verify_missing_lock_and_unavailable_uv_fail_without_mutation() {
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    let original = format!(
        "[project]\nname='fixture'\nversion='0.0.0'\nrequires-python='>=3.10'\ndependencies=['demo>=1']\n{}",
        configured(&index)
    );
    let directory = project(&original);
    let empty_path = tempfile::tempdir().unwrap();
    let output = command(directory.path())
        .env("PATH", empty_path.path())
        .args(["--verify", "-u"])
        .output()
        .unwrap();
    assert_code(&output, 1);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("existing regular uv.lock"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    unchanged(&directory, &original);
    let lock = "version=1\nrevision=3\n[[package]]\nname='fixture'\nversion='0.0.0'\nsource={virtual='.'}\n";
    fs::write(directory.path().join("uv.lock"), lock).unwrap();
    let output = command(directory.path())
        .env("PATH", empty_path.path())
        .args(["--verify", "-u"])
        .output()
        .unwrap();
    assert_code(&output, 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Cannot start uv for verification"));
    assert_eq!(
        fs::read_to_string(directory.path().join("pyproject.toml")).unwrap(),
        original
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("uv.lock")).unwrap(),
        lock
    );
    assert!(!directory.path().join(".venv").exists());
}

#[test]
fn unsupported_url_requirements_redact_credentials_and_query_tokens_in_all_reports() {
    let original = "[project]\ndependencies=['direct @ https://fixture-user:fixture-password@invalid.example/direct.whl?token=fixture-token#fixture-fragment', 'demo>=1']\n";
    let directory = project(original);
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    for args in [
        vec!["--explain", "--format", "json"],
        vec!["--explain"],
        vec!["--format", "json"],
    ] {
        let output = run(&directory, &index, &args);
        assert_code(&output, 0);
        for stream in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(stream);
            for secret in [
                "fixture-user",
                "fixture-password",
                "fixture-token",
                "fixture-fragment",
            ] {
                assert!(!text.contains(secret), "secret {secret} in output: {text}");
            }
        }
        if args.contains(&"json") {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["updates"].as_array().unwrap().len(), 1);
            if args.contains(&"--explain") {
                assert_eq!(value["decisions"][0]["reason"], "unsupported_requirement");
            }
        }
    }
    assert_eq!(index.requests(), vec!["/simple/demo/"; 3]);
    unchanged(&directory, original);
}

#[test]
fn package_cooldown_overrides_global_and_cli_overrides_package() {
    let original = "[project]\ndependencies=['demo>=1','other>=1']\n[tool.check-updates]\ncooldown='1d'\n[tool.check-updates.packages.DEMO]\ncooldown='999999d'\n";
    let directory = project(original);
    let mut demo = artifact("demo", "2.0");
    demo["upload-time"] = json!("2000-01-01T00:00:00Z");
    let mut other = artifact("other", "2.0");
    other["upload-time"] = json!("2000-01-01T00:00:00Z");
    let index = IndexServer::metadata(&[("demo", vec![demo]), ("other", vec![other])]);
    let value = report(&directory, &index, &[], 0);
    assert!(value["decisions"][0]["selected"].is_null());
    assert_eq!(
        value["decisions"][0]["rejected"][0]["reasons"],
        json!(["cooldown"])
    );
    assert_eq!(value["decisions"][1]["selected"], "2.0");
    let value = report(&directory, &index, &["--cooldown", "1d"], 0);
    assert_eq!(value["decisions"][0]["selected"], "2.0");
    unchanged(&directory, original);
}

#[test]
fn show_locked_and_minimal_keep_all_marker_versions_without_running_uv() {
    let original = "[project]\ndependencies=['demo>=1']\n";
    let directory = project(original);
    let index = IndexServer::new(&[("demo", &["3.0"])]);
    let lock = format!(
        "version=1\nrevision=3\n[[package]]\nname='demo'\nversion='1.5'\nsource={{registry='{}'}}\nresolution-markers=[\"python_version < '3.12'\"]\n[[package]]\nname='demo'\nversion='2.0'\nsource={{registry='{}'}}\nresolution-markers=[\"python_version >= '3.12'\"]\n",
        index.url, index.url
    );
    fs::write(directory.path().join("uv.lock"), &lock).unwrap();
    let empty_path = tempfile::tempdir().unwrap();
    let output = command(directory.path())
        .env("PATH", empty_path.path())
        .args([
            "--index",
            &index.url,
            "--show-locked",
            "--minimal",
            "--check",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_code(&output, 0);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 2);
    assert_eq!(value["updates"], json!([]));
    assert_eq!(value["decisions"][0]["reason"], "minimal");
    assert_eq!(value["decisions"][0]["latest"], "3.0");
    assert_eq!(
        value["decisions"][0]["locked"],
        json!({
            "state":"unverified", "freshness":"not_checked", "versions":[
                {"version":"1.5","source":"registry","markers":["python_version < '3.12'"]},
                {"version":"2.0","source":"registry","markers":["python_version >= '3.12'"]}
            ]
        })
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("uv.lock")).unwrap(),
        lock
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("pyproject.toml")).unwrap(),
        original
    );
    assert!(!directory.path().join(".venv").exists());
}

#[test]
fn show_locked_reports_missing_unsupported_and_source_mismatch_without_hiding_updates() {
    let directory = project("[project]\ndependencies=['demo>=1']\n");
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    for (lock, state) in [
        (None, "missing"),
        (Some("version=1\nrevision=999\n"), "unsupported"),
        (
            Some(
                "version=1\n[[package]]\nname='demo'\nversion='1.0'\nsource={registry='https://invalid.example/simple/'}\n",
            ),
            "source_mismatch",
        ),
    ] {
        if let Some(lock) = lock {
            fs::write(directory.path().join("uv.lock"), lock).unwrap();
        }
        let value = report(&directory, &index, &["--show-locked"], 0);
        assert_eq!(
            value["decisions"][0]["locked"],
            json!({"state":state,"freshness":"not_checked","versions":[]})
        );
        assert_eq!(value["decisions"][0]["selected"], "2.0");
        assert_eq!(value["updates"].as_array().unwrap().len(), 1);
        if let Some(lock) = lock {
            assert_eq!(
                fs::read_to_string(directory.path().join("uv.lock")).unwrap(),
                lock
            );
        }
    }
}

#[test]
fn unnamed_workspace_root_is_context_not_a_selectable_directory_name() {
    let index = IndexServer::new(&[("demo", &["2.0"])]);
    let root = project(&format!(
        "[tool.uv.workspace]\nmembers=['packages/*']\n[tool.check-updates]\ninclude=['demo']\n{}",
        configured(&index)
    ));
    member(
        &root,
        "packages/a",
        "[project]\nname='a'\ndependencies=['demo>=1','never-query>=1']\n",
    );
    let output = command(root.path())
        .args(["-w", "--member", "a", "--format", "json"])
        .output()
        .unwrap();
    assert_code(&output, 0);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["updates"].as_array().unwrap().len(), 1);
    assert_eq!(value["updates"][0]["requirement"], "demo>=1");
    let output = command(root.path())
        .args(["-w", "--member", "root", "--format", "json"])
        .output()
        .unwrap();
    assert_code(&output, 1);
    assert!(output.stdout.is_empty());
    assert_eq!(index.requests(), vec!["/simple/demo/"]);
}

#[test]
fn mixed_workspace_prerelease_policies_refuse_lock_writes_before_uv() {
    let index = IndexServer::new(&[("demo", &["2.0", "3.0rc1"])]);
    let root = project(&format!(
        "[project]\nname='root'\ndependencies=[]\n[tool.uv.workspace]\nmembers=['member']\n[tool.check-updates]\npre=true\n{}",
        configured(&index)
    ));
    member(
        &root,
        "member",
        "[project]\nname='stable-member'\ndependencies=['demo>=1']\n[tool.check-updates]\npre=false\n",
    );
    fs::write(root.path().join("uv.lock"), "unchanged lock").unwrap();
    let before = fs::read(root.path().join("member/pyproject.toml")).unwrap();
    let output = command(root.path())
        .args(["-w", "--member", "stable-member", "-u"])
        .output()
        .unwrap();
    assert_code(&output, 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Different prerelease policies"));
    assert_eq!(
        fs::read(root.path().join("member/pyproject.toml")).unwrap(),
        before
    );
    assert_eq!(
        fs::read_to_string(root.path().join("uv.lock")).unwrap(),
        "unchanged lock"
    );
    let output = command(root.path())
        .args(["-w", "--member", "stable-member", "-u", "--no-lock"])
        .output()
        .unwrap();
    assert_code(&output, 0);
    let after = fs::read_to_string(root.path().join("member/pyproject.toml")).unwrap();
    assert!(after.contains("demo>=2.0"));
    assert!(!after.contains("3.0rc1"));
}

#[test]
fn minimal_bound_preview_preserves_floor_but_keeps_the_exact_candidate() {
    let original = "[project]\ndependencies=['demo>=1.0,<2']\n";
    let root = project(original);
    let index = IndexServer::new(&[("demo", &["1.0", "2.0"])]);
    let output = run(
        &root,
        &index,
        &[
            "--minimal",
            "--bound",
            "demo<3",
            "--to",
            "demo==2.0",
            "--format",
            "json",
        ],
    );
    assert_code(&output, 0);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["updates"][0]["replacement"], "demo>=1.0,<3");
    assert_eq!(value["updates"][0]["version"], "2.0");
    unchanged(&root, original);
}
