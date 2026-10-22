#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use check_updates::dependencies::{Update, dependencies};
use check_updates::output::Pending;
use check_updates::verification::ResolvePolicy;
use check_updates::{doctor, workspace};

struct Fixture {
    temp: tempfile::TempDir,
    root: PathBuf,
    audit: PathBuf,
    manifest: Vec<u8>,
    lock: Vec<u8>,
    pending: Pending,
}

impl Fixture {
    fn new(action: &str) -> Self {
        assert!(
            Command::new("uv")
                .arg("--version")
                .output()
                .expect("Doctor tests require uv")
                .status
                .success()
        );
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let wheels = temp.path().join("wheels");
        let audit = temp.path().join("audit");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&wheels).unwrap();
        // macOS temporary paths can use the /var -> /private/var alias.
        let root = root.canonicalize().unwrap();
        let wheels = wheels.canonicalize().unwrap();
        // Generate only inert local fixture wheels with the Python standard library.
        // No public package is downloaded or installed by these tests.
        let make_wheels = r#"
import pathlib, sys, zipfile
for version in ('1.0', '2.0'):
    stem = f'doctor_demo-{version}.dist-info'
    with zipfile.ZipFile(pathlib.Path(sys.argv[1]) / f'doctor_demo-{version}-py3-none-any.whl', 'w') as wheel:
        wheel.writestr(stem + '/METADATA', f'Metadata-Version: 2.1\nName: doctor-demo\nVersion: {version}\n')
        wheel.writestr(stem + '/WHEEL', 'Wheel-Version: 1.0\nGenerator: doctor-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n')
        wheel.writestr(stem + '/RECORD', '')
"#;
        let status = Command::new("uv")
            .current_dir(temp.path())
            .args([
                "run",
                "--no-project",
                "--offline",
                "--no-python-downloads",
                "python",
                "-c",
                make_wheels,
            ])
            .arg(&wheels)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "Doctor tests require an offline Python interpreter"
        );
        let manifest = format!(
            "[project]\nname='doctor-fixture'\nversion='0.0.0'\nrequires-python='>=3.8'\ndependencies=['doctor-demo==1.0']\n[tool.uv]\npackage=false\nno-index=true\nfind-links=[{}]\n",
            serde_json::to_string(&wheels.to_string_lossy()).unwrap()
        ).into_bytes();
        fs::write(root.join("pyproject.toml"), &manifest).unwrap();
        let script = format!(
            "import os, sys\nfrom pathlib import Path\nfrom importlib.metadata import version\naudit=Path({audit:?})\nrows=audit.read_text().splitlines() if audit.exists() else []\nwith audit.open('a') as stream: stream.write(str(Path.cwd())+'\\n')\nassert Path.cwd() != Path({root:?})\nassert Path(os.environ['UV_PROJECT_ENVIRONMENT']) == Path.cwd()/'.venv'\nassert Path(sys.prefix) == Path.cwd()/'.venv'\nassert 'UV_PROJECT' not in os.environ\nassert 'PYTHONPATH' not in os.environ\n{action}\n",
            audit = audit.to_string_lossy(),
            root = root.to_string_lossy(),
        );
        fs::write(root.join("doctor_check.py"), script).unwrap();
        let status = Command::new("uv")
            .current_dir(&root)
            .args(["lock", "--offline", "--no-python-downloads"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "offline fixture lock failed");
        let lock = fs::read(root.join("uv.lock")).unwrap();
        let project = workspace::read_project(root.join("pyproject.toml")).unwrap();
        let update = Update {
            dependency: dependencies(&project.document)[0].clone(),
            replacement: "doctor-demo==2.0".into(),
            version: "2.0".parse().unwrap(),
            release_link: None,
            project_link: None,
        };
        let pending = vec![(project, vec![update])];
        Self {
            temp,
            root,
            audit,
            manifest,
            lock,
            pending,
        }
    }

    fn run(&self) -> anyhow::Result<()> {
        self.run_command(&[
            "uv".into(),
            "run".into(),
            "--no-sync".into(),
            "python".into(),
            "doctor_check.py".into(),
        ])
    }

    fn run_command(&self, command: &[String]) -> anyhow::Result<()> {
        let projects = vec![self.pending[0].0.clone()];
        doctor::run(
            &self.root,
            &projects,
            &self.pending,
            &ResolvePolicy::default(),
            command,
        )
    }

    fn unchanged(&self) {
        assert_eq!(
            fs::read(self.root.join("pyproject.toml")).unwrap(),
            self.manifest
        );
        assert_eq!(fs::read(self.root.join("uv.lock")).unwrap(), self.lock);
        assert!(!self.root.join(".venv").exists());
        if self.audit.exists() {
            for path in fs::read_to_string(&self.audit).unwrap().lines() {
                assert!(
                    !PathBuf::from(path).exists(),
                    "temporary environment was not removed"
                );
            }
        }
    }
}

#[test]
fn requires_a_nonempty_explicit_test_command() {
    let temp = tempfile::tempdir().unwrap();
    for command in [vec![], vec![String::new()]] {
        let error = doctor::run(
            temp.path(),
            &[],
            &vec![],
            &ResolvePolicy::default(),
            &command,
        )
        .unwrap_err();
        assert!(error.to_string().contains("nonempty test command"));
    }
}

#[test]
fn baseline_failure_stops_before_candidate_trials() {
    let fixture = Fixture::new("sys.exit(1)");
    let error = fixture.run().unwrap_err();
    assert!(error.to_string().contains("baseline"), "{error:#}");
    assert_eq!(
        fs::read_to_string(&fixture.audit).unwrap().lines().count(),
        1
    );
    fixture.unchanged();
}

#[test]
fn passing_batch_is_retested_and_applies_exact_manifest_and_lock() {
    let fixture = Fixture::new("assert version('doctor-demo') in ('1.0', '2.0')");
    fixture.run().unwrap();
    let manifest = fs::read_to_string(fixture.root.join("pyproject.toml")).unwrap();
    let lock = fs::read_to_string(fixture.root.join("uv.lock")).unwrap();
    assert!(manifest.contains("doctor-demo==2.0"));
    assert!(lock.contains("version = \"2.0\""));
    assert!(!lock.contains("version = \"1.0\""));
    assert_eq!(
        fs::read_to_string(&fixture.audit).unwrap().lines().count(),
        3
    );
    assert!(!fixture.root.join(".venv").exists());
    for path in fs::read_to_string(&fixture.audit).unwrap().lines() {
        assert!(!PathBuf::from(path).exists());
        assert!(!lock.contains(path));
    }
}

#[test]
fn no_passing_candidate_leaves_original_inputs_unchanged() {
    let fixture = Fixture::new("sys.exit(0 if version('doctor-demo') == '1.0' else 1)");
    let error = fixture.run().unwrap_err();
    assert!(
        error.to_string().contains("no passing nonempty subset"),
        "{error:#}"
    );
    assert_eq!(
        fs::read_to_string(&fixture.audit).unwrap().lines().count(),
        2
    );
    fixture.unchanged();
}

#[test]
fn final_combined_failure_is_never_applied() {
    let fixture = Fixture::new("sys.exit(1 if len(rows) == 2 else 0)");
    let error = fixture.run().unwrap_err();
    assert!(
        error.to_string().contains("final combined tests failed"),
        "{error:#}"
    );
    assert_eq!(
        fs::read_to_string(&fixture.audit).unwrap().lines().count(),
        3
    );
    fixture.unchanged();
}

#[test]
fn changed_temporary_lock_and_missing_command_are_not_applied() {
    let fixture = Fixture::new(
        "with open('uv.lock', 'a') as stream: stream.write('\\n# changed by test\\n')",
    );
    let error = fixture.run().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("changed temporary resolver inputs"),
        "{error:#}"
    );
    fixture.unchanged();
    let error = fixture
        .run_command(&["doctor-command-that-does-not-exist".into()])
        .unwrap_err();
    assert!(error.to_string().contains("could not start"), "{error:#}");
    fixture.unchanged();
}

#[test]
fn concurrent_original_source_edit_is_not_overwritten() {
    let fixture = Fixture::new("pass");
    let script = fixture.root.join("doctor_check.py");
    let original = fs::read_to_string(&script).unwrap();
    fs::write(
        &script,
        format!(
            "{original}\nif version('doctor-demo') == '2.0': Path({:?}).write_text('user edit')\n",
            fixture.root.join("tracked.txt").to_string_lossy()
        ),
    )
    .unwrap();
    fs::write(fixture.root.join("tracked.txt"), "original source").unwrap();
    let error = fixture.run().unwrap_err();
    assert!(
        error.to_string().contains("changed during review"),
        "{error:#}"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("tracked.txt")).unwrap(),
        "user edit"
    );
    fixture.unchanged();
    assert!(fixture.temp.path().is_dir());
}

fn change_scope(fixture: &mut Fixture, manifest: String) {
    fs::write(fixture.root.join("pyproject.toml"), &manifest).unwrap();
    let status = Command::new("uv")
        .current_dir(&fixture.root)
        .args(["lock", "--offline", "--no-python-downloads"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    fixture.manifest = manifest.into_bytes();
    fixture.lock = fs::read(fixture.root.join("uv.lock")).unwrap();
    let project = workspace::read_project(fixture.root.join("pyproject.toml")).unwrap();
    let mut update = fixture.pending[0].1[0].clone();
    update.dependency = dependencies(&project.document)[0].clone();
    fixture.pending = vec![(project, vec![update])];
}

#[test]
fn optional_extras_and_nondefault_groups_are_actually_installed_and_tested() {
    for section in [
        "[project.optional-dependencies]\nfeature",
        "[dependency-groups]\nqa",
    ] {
        let mut fixture = Fixture::new("assert version('doctor-demo') in ('1.0', '2.0')");
        let manifest = String::from_utf8(fixture.manifest.clone())
            .unwrap()
            .replace(
                "dependencies=['doctor-demo==1.0']",
                &format!("dependencies=[]\n{section}=['doctor-demo==1.0']"),
            );
        change_scope(&mut fixture, manifest);
        fixture.run().unwrap();
        assert!(
            fs::read_to_string(fixture.root.join("pyproject.toml"))
                .unwrap()
                .contains("doctor-demo==2.0")
        );
        assert_eq!(
            fs::read_to_string(&fixture.audit).unwrap().lines().count(),
            3
        );
        assert!(!fixture.root.join(".venv").exists());
    }
}

#[test]
fn inactive_marker_distribution_is_not_described_as_tested() {
    let mut fixture = Fixture::new("pass");
    let manifest = String::from_utf8(fixture.manifest.clone())
        .unwrap()
        .replace(
            "dependencies=['doctor-demo==1.0']",
            "dependencies=[\"doctor-demo==1.0; python_version < '2'\"]",
        );
    change_scope(&mut fixture, manifest);
    fixture.pending[0].1[0].replacement = "doctor-demo==2.0; python_version < '2'".into();
    let error = fixture.run().unwrap_err();
    assert!(error.to_string().contains("baseline"), "{error:#}");
    assert!(
        !fixture.audit.exists(),
        "test command must not run for an absent selected distribution"
    );
    fixture.unchanged();
}

#[test]
fn cli_minimal_bound_change_still_resolves_the_exact_requested_candidate() {
    use sha2::{Digest, Sha256};
    let mut fixture = Fixture::new("pass");
    let manifest = String::from_utf8(fixture.manifest.clone())
        .unwrap()
        .replace("doctor-demo==1.0", "doctor-demo>=1.0,<2");
    change_scope(&mut fixture, manifest);
    // Public lookup metadata is synthetic and invocation-local; uv uses only
    // inert local wheels. No public registry request is needed or permitted.
    let cache = fixture.temp.path().join("metadata/check-updates");
    fs::create_dir_all(&cache).unwrap();
    let digest: String = Sha256::digest(b"doctor-demo")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    fs::write(cache.join(format!("v2-{digest}.json")), serde_json::json!({"version":2,"time":now,"releases":[["1.0",null,null,false],["2.0",null,null,false]]}).to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_check-updates"))
        .current_dir(&fixture.root)
        .env("XDG_CACHE_HOME", cache.parent().unwrap())
        .env("UV_OFFLINE", "1")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .args([
            "--bound",
            "doctor-demo<3",
            "--to",
            "doctor-demo==2.0",
            "--minimal",
            "-u",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(fixture.root.join("pyproject.toml"))
            .unwrap()
            .contains("doctor-demo>=1.0,<3")
    );
    let lock = fs::read_to_string(fixture.root.join("uv.lock")).unwrap();
    assert!(lock.contains("version = \"2.0\""));
    assert!(!lock.contains("version = \"1.0\""));
}
