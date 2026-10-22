use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use check_updates::changes::prepare_change;
use check_updates::dependencies::{Update, dependencies};
use check_updates::verification::{ResolvePolicy, preflight, prepare};
use check_updates::workspace::{Project, read_project};
use tempfile::TempDir;

fn uv(directory: &Path, args: &[&str]) -> Output {
    let mut command = Command::new("uv");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("UV_") {
            command.env_remove(key);
        }
    }
    let output = command
        .current_dir(directory)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("CRLF tests require uv 0.13 and an offline Python interpreter");
    assert!(
        output.status.success(),
        "uv {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

struct Fixture {
    directory: TempDir,
    project: Project,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let version = uv(directory.path(), &["--version"]);
        assert!(
            String::from_utf8_lossy(&version.stdout).starts_with("uv 0.13."),
            "CRLF tests require uv 0.13"
        );
        let wheels = directory.path().join("wheels");
        fs::create_dir(&wheels).unwrap();
        // Create inert wheels with the Python standard library. Do not sync a project environment.
        let script = "import pathlib,sys,zipfile\np=pathlib.Path(sys.argv[1])\nfor v in ['1.0','2.0']:\n with zipfile.ZipFile(p/('demo-'+v+'-py3-none-any.whl'),'w') as z:\n  d='demo-'+v+'.dist-info/'\n  z.writestr(d+'METADATA','Metadata-Version: 2.3\\nName: demo\\nVersion: '+v+'\\n')\n  z.writestr(d+'WHEEL','Wheel-Version: 1.0\\nRoot-Is-Purelib: true\\nTag: py3-none-any\\n')\n  z.writestr(d+'RECORD','')\n";
        uv(
            directory.path(),
            &[
                "run",
                "--no-project",
                "--offline",
                "--no-python-downloads",
                "python",
                "-c",
                script,
                wheels.to_str().unwrap(),
            ],
        );
        let text = format!(
            "# Preserve CRLF and comments\r\n[project]\r\nname = 'crlf-fixture'\r\nversion = '1'\r\nrequires-python = '>=3.11'\r\ndependencies = [\r\n    \"demo>=1.0,<3\", # Keep this bound and comment\r\n]\r\n\r\n[tool.uv]\r\npackage = false\r\nno-index = true\r\nfind-links = [{}]\r\n",
            toml_edit::Value::from(wheels.to_str().unwrap())
        );
        let path = directory.path().join("pyproject.toml");
        fs::write(&path, text).unwrap();
        uv(
            directory.path(),
            &[
                "lock",
                "--offline",
                "--no-build",
                "--no-python-downloads",
                "--upgrade-package",
                "demo==1.0",
            ],
        );
        Self {
            directory,
            project: read_project(path).unwrap(),
        }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn updates(&self) -> Vec<Update> {
        vec![Update {
            dependency: dependencies(&self.project.document)[0].clone(),
            replacement: "demo>=2.0,<3".into(),
            version: "2.0".parse().unwrap(),
            release_link: None,
            project_link: None,
        }]
    }
}

#[test]
fn verified_update_preserves_exact_crlf_source() {
    let fixture = Fixture::new();
    let updates = fixture.updates();
    let pending = vec![(fixture.project.clone(), updates.clone())];
    let verified = preflight(
        fixture.root(),
        std::slice::from_ref(&fixture.project),
        &pending,
        &ResolvePolicy::default(),
    )
    .unwrap();
    let change = prepare_change(&fixture.project, &updates).unwrap();
    verified.apply(&[change]).unwrap();

    let expected = String::from_utf8(fixture.project.original.clone())
        .unwrap()
        .replace("demo>=1.0,<3", "demo>=2.0,<3");
    assert_eq!(
        fs::read(&fixture.project.path).unwrap(),
        expected.as_bytes()
    );
    let lock = fs::read_to_string(fixture.root().join("uv.lock")).unwrap();
    assert!(lock.contains("name = \"demo\"\nversion = \"2.0\""));
    assert!(!fixture.root().join(".venv").exists());
}

#[test]
fn inconsistent_in_memory_crlf_document_is_rejected() {
    let mut fixture = Fixture::new();
    let lock = fs::read(fixture.root().join("uv.lock")).unwrap();
    fixture.project.document["project"]["name"] = toml_edit::value("changed-in-memory");
    let error = prepare(
        fixture.root(),
        std::slice::from_ref(&fixture.project),
        &ResolvePolicy::default(),
        false,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Project document differs from its original source snapshot"),
        "{error:#}"
    );
    assert_eq!(
        fs::read(&fixture.project.path).unwrap(),
        fixture.project.original
    );
    assert_eq!(fs::read(fixture.root().join("uv.lock")).unwrap(), lock);
}

#[test]
fn disk_line_ending_change_is_rejected_during_preparation() {
    let fixture = Fixture::new();
    let lock = fs::read(fixture.root().join("uv.lock")).unwrap();
    let changed = String::from_utf8(fixture.project.original.clone())
        .unwrap()
        .replace("\r\n", "\n");
    fs::write(&fixture.project.path, &changed).unwrap();
    let error = prepare(
        fixture.root(),
        std::slice::from_ref(&fixture.project),
        &ResolvePolicy::default(),
        false,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("Project changed during review"),
        "{error:#}"
    );
    assert_eq!(fs::read(&fixture.project.path).unwrap(), changed.as_bytes());
    assert_eq!(fs::read(fixture.root().join("uv.lock")).unwrap(), lock);
}

#[test]
fn disk_line_ending_change_blocks_verified_application() {
    let fixture = Fixture::new();
    let lock = fs::read(fixture.root().join("uv.lock")).unwrap();
    let updates = fixture.updates();
    let pending = vec![(fixture.project.clone(), updates.clone())];
    let verified = preflight(
        fixture.root(),
        std::slice::from_ref(&fixture.project),
        &pending,
        &ResolvePolicy::default(),
    )
    .unwrap();
    let changed = String::from_utf8(fixture.project.original.clone())
        .unwrap()
        .replace("\r\n", "\n");
    fs::write(&fixture.project.path, &changed).unwrap();
    let change = prepare_change(&fixture.project, &updates).unwrap();
    let error = verified.apply(&[change]).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Verification input changed during review"),
        "{error:#}"
    );
    assert_eq!(fs::read(&fixture.project.path).unwrap(), changed.as_bytes());
    assert_eq!(fs::read(fixture.root().join("uv.lock")).unwrap(), lock);
}
