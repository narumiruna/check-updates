use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use check_updates::changes::{Change, prepare_change};
use check_updates::dependencies::{Update, dependencies};
use check_updates::verification::{ResolvePolicy, preflight, prepare, prepare_with_deadline};
use check_updates::workspace::{Project, read_project};
use tempfile::TempDir;

fn uv_available() -> bool {
    let status = Command::new("uv")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .expect("Verification tests require uv 0.13.0 and an offline Python interpreter");
    assert!(
        status.success(),
        "Verification tests require a working uv executable"
    );
    true
}

fn uv(directory: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new("uv");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("UV_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(directory)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

struct Fixture {
    directory: TempDir,
    project: Project,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let wheel_dir = directory.path().join("wheels");
        fs::create_dir(&wheel_dir).unwrap();
        // The Python standard library creates real, offline wheel fixtures.
        let script = "import sys,zipfile,pathlib\np=pathlib.Path(sys.argv[1])\nfor v in ['1.0','2.0','3.0']:\n with zipfile.ZipFile(p/('demo-'+v+'-py3-none-any.whl'),'w') as z:\n  d='demo-'+v+'.dist-info/'\n  extra='Requires-Dist: absent==99\\n' if v=='3.0' else ''\n  z.writestr(d+'METADATA','Metadata-Version: 2.3\\nName: demo\\nVersion: '+v+'\\n'+extra)\n  z.writestr(d+'WHEEL','Wheel-Version: 1.0\\nRoot-Is-Purelib: true\\nTag: py3-none-any\\n')\n  z.writestr(d+'RECORD','')\n";
        let output = uv(
            directory.path(),
            &[
                "run",
                "--no-project",
                "--offline",
                "--no-python-downloads",
                "python",
                "-c",
                script,
                wheel_dir.to_str().unwrap(),
            ],
        );
        assert!(
            output.status.success(),
            "fixture interpreter: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = format!(
            "[project]\nname='fixture'\nversion='1'\nrequires-python='>=3.11'\ndependencies=['demo>=1.0']\n[tool.uv]\nno-index=true\nfind-links=[{}]\n",
            toml_edit::Value::from(wheel_dir.to_str().unwrap())
        );
        let path = directory.path().join("pyproject.toml");
        fs::write(&path, text).unwrap();
        let output = uv(
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
        assert!(
            output.status.success(),
            "fixture lock: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self {
            directory,
            project: read_project(path).unwrap(),
        }
    }

    fn pending(&self, version: &str) -> Vec<(Project, Vec<Update>)> {
        vec![(
            self.project.clone(),
            vec![Update {
                dependency: dependencies(&self.project.document)[0].clone(),
                replacement: format!(
                    "demo>=1.0,<{upper}",
                    upper = if version == "3.0" { "4" } else { "3" }
                ),
                version: version.parse().unwrap(),
                release_link: None,
                project_link: None,
            }],
        )]
    }

    fn lock(&self) -> Vec<u8> {
        fs::read(self.directory.path().join("uv.lock")).unwrap()
    }
    fn root(&self) -> &Path {
        self.directory.path()
    }
}

struct WheelIndex {
    url: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl WheelIndex {
    fn new(wheels: &Path, timestamps: bool) -> Self {
        use std::io::{Read, Write};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/simple/", listener.local_addr().unwrap());
        let mut routes = std::collections::HashMap::new();
        for name in ["demo", "child"] {
            let mut files = Vec::new();
            for (version, uploaded) in [
                ("1.0", "2010-01-01T00:00:00Z"),
                (
                    "2.0",
                    if name == "child" {
                        "2035-01-01T00:00:00Z"
                    } else {
                        "2025-01-01T00:00:00Z"
                    },
                ),
                ("3.0", "2035-01-01T00:00:00Z"),
            ] {
                let filename = format!("{name}-{version}-py3-none-any.whl");
                if !wheels.join(&filename).exists() {
                    continue;
                }
                let mut entry = serde_json::json!({"filename":filename,"url":format!("/wheels/{filename}"),"hashes":{},"yanked":false});
                if timestamps {
                    entry["upload-time"] = uploaded.into();
                }
                files.push(entry);
                routes.insert(
                    format!("/wheels/{filename}"),
                    (
                        "application/octet-stream",
                        fs::read(wheels.join(filename)).unwrap(),
                    ),
                );
            }
            if !files.is_empty() {
                routes.insert(format!("/simple/{name}/"), ("application/vnd.pypi.simple.v1+json", serde_json::to_vec(
                    &serde_json::json!({"meta":{"api-version":"1.1"},"name":name,"files":files})).unwrap()));
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let worker = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Accepted sockets can inherit O_NONBLOCK on macOS.
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut request = [0; 16384];
                        let size = stream.read(&mut request).unwrap_or(0);
                        let request = String::from_utf8_lossy(&request[..size]);
                        let route = request.split_whitespace().nth(1).unwrap_or("");
                        if let Some((kind, body)) = routes.get(route) {
                            let _ = write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(body);
                        } else {
                            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for WheelIndex {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn package_cutoff_overrides_global_and_unknown_registry_timestamps_fail_closed() {
    if !uv_available() {
        return;
    }
    for timestamps in [true, false] {
        let fixture = Fixture::new();
        let index = WheelIndex::new(&fixture.root().join("wheels"), timestamps);
        let text = format!(
            "[project]\nname='fixture'\nversion='1'\nrequires-python='>=3.11'\ndependencies=['demo>=1.0']\n[[tool.uv.index]]\nurl='{}'\ndefault=true\n",
            index.url
        );
        fs::write(&fixture.project.path, text).unwrap();
        let project = read_project(&fixture.project.path).unwrap();
        let mut pending = fixture.pending("2.0");
        pending[0].0 = project.clone();
        let policy = ResolvePolicy {
            cutoff: Some(1_577_836_800),
            package_cutoffs: vec![("demo".into(), 1_893_456_000)],
            prerelease: false,
        };
        let before = fixture.lock();
        let result = preflight(fixture.root(), &[project], &pending, &policy);
        assert_eq!(result.is_ok(), timestamps, "{result:?}");
        assert_eq!(fixture.lock(), before);
    }
}

#[test]
fn cooldown_refreshes_recent_locked_transitives_and_enforces_package_overrides() {
    assert!(uv_available());
    let fixture = Fixture::new();
    let wheels = fixture.root().join("wheels");
    let script = "import pathlib,sys,zipfile\np=pathlib.Path(sys.argv[1])\nfor name in ['demo','child']:\n for v in ['1.0','2.0']:\n  with zipfile.ZipFile(p/(name+'-'+v+'-py3-none-any.whl'),'w') as z:\n   d=name+'-'+v+'.dist-info/'\n   extra='Requires-Dist: child>=1\\n' if name=='demo' else ''\n   z.writestr(d+'METADATA','Metadata-Version: 2.3\\nName: '+name+'\\nVersion: '+v+'\\n'+extra)\n   z.writestr(d+'WHEEL','Wheel-Version: 1.0\\nRoot-Is-Purelib: true\\nTag: py3-none-any\\n')\n   z.writestr(d+'RECORD','')\n";
    let output = uv(
        fixture.root(),
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
    assert!(output.status.success());
    let index = WheelIndex::new(&wheels, true);
    fs::write(&fixture.project.path, format!("[project]\nname='fixture'\nversion='1'\nrequires-python='>=3.11'\ndependencies=['demo>=1']\n[[tool.uv.index]]\nurl='{}'\ndefault=true\n", index.url)).unwrap();
    let output = uv(
        fixture.root(),
        &[
            "lock",
            "--no-build",
            "--no-python-downloads",
            "--upgrade-package",
            "demo==1.0",
            "--upgrade-package",
            "child==2.0",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let before = fixture.lock();
    let project = read_project(&fixture.project.path).unwrap();
    let mut pending = fixture.pending("2.0");
    pending[0].0 = project.clone();
    pending[0].1[0].dependency = dependencies(&project.document)[0].clone();
    let policy = ResolvePolicy {
        cutoff: Some(1_577_836_800),
        package_cutoffs: vec![("demo".into(), 1_893_456_000)],
        prerelease: false,
    };
    let verified = preflight(
        fixture.root(),
        std::slice::from_ref(&project),
        &pending,
        &policy,
    )
    .unwrap();
    assert_eq!(fixture.lock(), before);
    let lock = fs::read_to_string(verified.directory().join("uv.lock"))
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    let child = lock["package"]
        .as_array_of_tables()
        .unwrap()
        .iter()
        .find(|p| p["name"].as_str() == Some("child"))
        .unwrap();
    assert_eq!(
        child["version"].as_str(),
        Some("1.0"),
        "recent locked child2 must not survive cutoff"
    );
    let changes = vec![prepare_change(&pending[0].0, &pending[0].1).unwrap()];
    verified.apply(&changes).unwrap();
    assert!(
        fs::read_to_string(&fixture.project.path)
            .unwrap()
            .contains("demo>=1.0,<3")
    );
}

#[test]
fn real_offline_resolution_enforces_exact_candidate_and_applies_relocatable_lock() {
    if !uv_available() {
        eprintln!("uv unavailable: real offline fixture skipped");
        return;
    }
    let fixture = Fixture::new();
    let before = fixture.lock();
    let pending = fixture.pending("2.0");
    let verified = preflight(
        fixture.root(),
        std::slice::from_ref(&fixture.project),
        &pending,
        &ResolvePolicy::default(),
    )
    .unwrap();
    let temporary = verified.directory().to_owned();
    assert_eq!(fixture.lock(), before);
    assert_eq!(
        fs::read(&fixture.project.path).unwrap(),
        fixture.project.original
    );
    let temporary_lock = fs::read_to_string(verified.directory().join("uv.lock")).unwrap();
    assert!(temporary_lock.contains("version = \"2.0\""));
    assert!(!temporary_lock.contains(&*temporary.to_string_lossy()));
    let changes: Vec<_> = pending
        .iter()
        .map(|(project, updates)| prepare_change(project, updates).unwrap())
        .collect();
    verified.apply(&changes).unwrap();
    assert_eq!(fixture.lock(), temporary_lock.as_bytes());
    assert_eq!(fs::read(&fixture.project.path).unwrap(), changes[0].content);
    drop(verified);
    assert!(!temporary.exists());
}

#[test]
fn proposed_url_sources_and_inconsistent_in_memory_documents_are_rejected() {
    if !uv_available() {
        return;
    }
    let fixture = Fixture::new();
    let mut pending = fixture.pending("2.0");
    pending[0].1[0].replacement = "demo @ file:///unsupported-backend".into();
    let error = preflight(
        fixture.root(),
        std::slice::from_ref(&fixture.project),
        &pending,
        &ResolvePolicy::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("direct URL"));
    assert_eq!(
        fs::read(&fixture.project.path).unwrap(),
        fixture.project.original
    );
    let mut project = fixture.project.clone();
    project.document.remove("tool");
    assert!(prepare(fixture.root(), &[project], &ResolvePolicy::default(), false).is_err());
}

#[test]
fn exact_prerelease_verification_uses_the_effective_prerelease_policy() {
    assert!(uv_available());
    let fixture = Fixture::new();
    let wheels = fixture.root().join("wheels");
    let script = "import pathlib,sys,zipfile\np=pathlib.Path(sys.argv[1]); v='2.0rc1'; d='demo-'+v+'.dist-info/'\nwith zipfile.ZipFile(p/('demo-'+v+'-py3-none-any.whl'),'w') as z:\n z.writestr(d+'METADATA','Metadata-Version: 2.3\\nName: demo\\nVersion: '+v+'\\n')\n z.writestr(d+'WHEEL','Wheel-Version: 1.0\\nRoot-Is-Purelib: true\\nTag: py3-none-any\\n')\n z.writestr(d+'RECORD','')\n";
    let output = uv(
        fixture.root(),
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
    assert!(output.status.success());
    for prerelease in [false, true] {
        let result = preflight(
            fixture.root(),
            std::slice::from_ref(&fixture.project),
            &fixture.pending("2.0rc1"),
            &ResolvePolicy {
                prerelease,
                ..Default::default()
            },
        );
        assert_eq!(result.is_ok(), prerelease, "{result:?}");
    }
    assert_eq!(
        fs::read(&fixture.project.path).unwrap(),
        fixture.project.original
    );
}

#[test]
fn offline_conflict_keeps_all_original_inputs_and_cleans_up() {
    if !uv_available() {
        return;
    }
    let fixture = Fixture::new();
    let before = fixture.lock();
    let prepared = prepare(
        fixture.root(),
        std::slice::from_ref(&fixture.project),
        &ResolvePolicy::default(),
        false,
    )
    .unwrap();
    let temporary = prepared.directory().to_owned();
    let error = prepared.resolve(&fixture.pending("3.0")).unwrap_err();
    assert!(format!("{error:#}").contains("uv verification failed"));
    assert_eq!(fixture.lock(), before);
    assert_eq!(
        fs::read(&fixture.project.path).unwrap(),
        fixture.project.original
    );
    assert!(!temporary.exists());
}

#[test]
fn concurrent_manifest_lock_or_config_changes_block_application() {
    if !uv_available() {
        return;
    }
    for name in [
        "pyproject.toml",
        "uv.lock",
        "uv.toml",
        ".python-version",
        "wheels/demo-2.0-py3-none-any.whl",
        "wheels/new-1-py3-none-any.whl",
    ] {
        let fixture = Fixture::new();
        let before = fixture.lock();
        let pending = fixture.pending("2.0");
        let verified = preflight(
            fixture.root(),
            std::slice::from_ref(&fixture.project),
            &pending,
            &ResolvePolicy::default(),
        )
        .unwrap();
        fs::write(fixture.root().join(name), "concurrent user edit").unwrap();
        let changes: Vec<_> = pending
            .iter()
            .map(|(project, updates)| prepare_change(project, updates).unwrap())
            .collect();
        assert!(verified.apply(&changes).is_err(), "{name}");
        assert_eq!(
            fs::read_to_string(fixture.root().join(name)).unwrap(),
            "concurrent user edit"
        );
        if name != "uv.lock" {
            assert_eq!(fixture.lock(), before);
        }
        if name != "pyproject.toml" {
            assert_eq!(
                fs::read(&fixture.project.path).unwrap(),
                fixture.project.original
            );
        }
    }
}

#[test]
fn edited_application_content_and_stale_caller_snapshots_are_refused() {
    if !uv_available() {
        return;
    }
    let fixture = Fixture::new();
    let pending = fixture.pending("2.0");
    let verified = preflight(
        fixture.root(),
        std::slice::from_ref(&fixture.project),
        &pending,
        &ResolvePolicy::default(),
    )
    .unwrap();
    let change = Change {
        project: &fixture.project,
        content: b"unverified changes".to_vec(),
        count: 1,
    };
    assert!(verified.apply(&[change]).is_err());
    fs::write(&fixture.project.path, "user edit").unwrap();
    assert!(
        prepare(
            fixture.root(),
            std::slice::from_ref(&fixture.project),
            &ResolvePolicy::default(),
            false
        )
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(&fixture.project.path).unwrap(),
        "user edit"
    );
}

#[test]
fn controlled_backend_probe_proves_no_build_alone_is_insufficient() {
    if !uv_available() {
        return;
    }
    let fixture = Fixture::new();
    let text = "[project]\nname='fixture'\nversion='1'\nrequires-python='>=3.11'\ndynamic=['dependencies']\n[build-system]\nrequires=[]\nbuild-backend='backend'\nbackend-path=['.']\n";
    fs::write(&fixture.project.path, text).unwrap();
    fs::write(fixture.root().join("backend.py"), "from pathlib import Path\nPath('backend-executed').touch()\nraise RuntimeError('controlled fixture backend')\n").unwrap();
    let before = fixture.lock();
    let output = uv(
        fixture.root(),
        &["lock", "--offline", "--no-build", "--no-python-downloads"],
    );
    assert!(!output.status.success());
    assert!(
        fixture.root().join("backend-executed").exists(),
        "uv --no-build did not execute the controlled first-party backend; review supported capabilities"
    );
    fs::remove_file(fixture.root().join("backend-executed")).unwrap();
    let project = read_project(&fixture.project.path).unwrap();
    let error = prepare(fixture.root(), &[project], &ResolvePolicy::default(), false).unwrap_err();
    assert!(error.to_string().contains("build-system"));
    assert!(!fixture.root().join("backend-executed").exists());
    assert_eq!(fs::read_to_string(&fixture.project.path).unwrap(), text);
    assert_eq!(fixture.lock(), before);
}

#[test]
fn unsafe_inputs_are_refused_before_starting_uv() {
    for suffix in [
        "\n[build-system]\nrequires=[]\nbuild-backend='backend'\n",
        "\n[tool.uv]\npackage=true\n",
        "\n[tool.uv.sources]\ndemo={path='../external'}\n",
        "\n[tool.uv]\nexclude-newer='2020-01-01'\n",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pyproject.toml");
        let text =
            format!("[project]\nname='fixture'\nversion='1'\ndependencies=['demo>=1']\n{suffix}");
        fs::write(&path, &text).unwrap();
        fs::write(temp.path().join("uv.lock"), "unchanged lock").unwrap();
        let project = read_project(&path).unwrap();
        assert!(prepare(temp.path(), &[project], &ResolvePolicy::default(), false).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert_eq!(
            fs::read_to_string(temp.path().join("uv.lock")).unwrap(),
            "unchanged lock"
        );
    }
    for text in [
        "[project]\nname='fixture'\nversion='1'\ndynamic=['dependencies']\n",
        "[project]\nname='fixture'\nversion='1'\ndependencies=['demo @ file:///external']\n",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pyproject.toml");
        fs::write(&path, text).unwrap();
        let project = read_project(path).unwrap();
        assert!(prepare(temp.path(), &[project], &ResolvePolicy::default(), false).is_err());
    }
}

#[test]
fn missing_lock_uv_toml_and_relative_find_links_are_refused() {
    if !uv_available() {
        return;
    }
    for name in ["uv.lock", "uv.toml"] {
        let fixture = Fixture::new();
        if name == "uv.lock" {
            fs::remove_file(fixture.root().join(name)).unwrap();
        } else {
            fs::write(fixture.root().join(name), "offline=true").unwrap();
        }
        assert!(
            prepare(
                fixture.root(),
                std::slice::from_ref(&fixture.project),
                &ResolvePolicy::default(),
                false
            )
            .is_err()
        );
    }
    let fixture = Fixture::new();
    let text = String::from_utf8(fixture.project.original.clone()).unwrap();
    fs::write(
        &fixture.project.path,
        text.replace(fixture.root().join("wheels").to_str().unwrap(), "wheels"),
    )
    .unwrap();
    let project = read_project(&fixture.project.path).unwrap();
    assert!(prepare(fixture.root(), &[project], &ResolvePolicy::default(), false).is_err());
}

#[test]
fn doctor_copy_preserves_source_files_and_checks_tree_changes() {
    if !uv_available() {
        return;
    }
    let fixture = Fixture::new();
    fs::write(fixture.root().join("test.py"), "assert True\n").unwrap();
    fs::create_dir(fixture.root().join(".venv")).unwrap();
    fs::write(fixture.root().join(".venv/ignored"), "not copied").unwrap();
    let prepared = prepare(
        fixture.root(),
        std::slice::from_ref(&fixture.project),
        &ResolvePolicy::default(),
        true,
    )
    .unwrap();
    assert!(prepared.directory().join("test.py").is_file());
    assert!(!prepared.directory().join(".venv").exists());
    fs::write(fixture.root().join("test.py"), "assert False\n").unwrap();
    assert!(prepared.check_snapshots().is_err());
}

#[test]
fn marker_specific_conflicting_targets_are_refused() {
    if !uv_available() {
        return;
    }
    let fixture = Fixture::new();
    let text = String::from_utf8(fixture.project.original.clone()).unwrap().replace("dependencies=['demo>=1.0']", "dependencies=[\"demo>=1.0; python_version < '3.13'\", \"demo>=1.0; python_version >= '3.13'\"]");
    fs::write(&fixture.project.path, text).unwrap();
    let project = read_project(&fixture.project.path).unwrap();
    let updates = dependencies(&project.document)
        .into_iter()
        .enumerate()
        .map(|(index, dependency)| Update {
            replacement: dependency.requirement.clone(),
            dependency,
            version: if index == 0 { "2.0" } else { "1.0" }.parse().unwrap(),
            release_link: None,
            project_link: None,
        })
        .collect();
    let error = preflight(
        fixture.root(),
        std::slice::from_ref(&project),
        &vec![(project.clone(), updates)],
        &ResolvePolicy::default(),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("Conflicting marker-specific"),
        "{error:#}"
    );
}

#[test]
fn workspace_globs_preserve_member_layout_and_detect_new_members() {
    if !uv_available() {
        return;
    }
    for concurrent in [false, true] {
        let fixture = Fixture::new();
        let text = format!(
            "{}\n[tool.uv.workspace]\nmembers=['packages/*']\n",
            String::from_utf8(fixture.project.original.clone()).unwrap()
        );
        fs::write(&fixture.project.path, text).unwrap();
        let member = fixture.root().join("packages/member/pyproject.toml");
        fs::create_dir_all(member.parent().unwrap()).unwrap();
        fs::write(
            &member,
            "[project]\nname='member'\nversion='1'\ndependencies=['demo>=1']\n",
        )
        .unwrap();
        let projects = vec![
            read_project(&fixture.project.path).unwrap(),
            read_project(member).unwrap(),
        ];
        let mut pending = fixture.pending("2.0");
        pending[0].0 = projects[0].clone();
        let verified = preflight(
            fixture.root(),
            &projects,
            &pending,
            &ResolvePolicy::default(),
        )
        .unwrap();
        assert!(
            verified
                .directory()
                .join("packages/member/pyproject.toml")
                .is_file()
        );
        assert!(
            fs::read_to_string(verified.directory().join("uv.lock"))
                .unwrap()
                .contains("virtual = \"packages/member\"")
        );
        let changes = pending
            .iter()
            .map(|(p, u)| prepare_change(p, u).unwrap())
            .collect::<Vec<_>>();
        if concurrent {
            fs::create_dir_all(fixture.root().join("packages/new")).unwrap();
            fs::write(
                fixture.root().join("packages/new/pyproject.toml"),
                "[project]\nname='new'\nversion='1'\n",
            )
            .unwrap();
            assert!(verified.apply(&changes).is_err());
        } else {
            verified.apply(&changes).unwrap();
        }
    }
}

#[test]
fn member_resolver_settings_are_refused_instead_of_using_a_different_index() {
    assert!(uv_available());
    let fixture = Fixture::new();
    let text = format!(
        "{}\n[tool.uv.workspace]\nmembers=['packages/*']\n",
        String::from_utf8(fixture.project.original.clone()).unwrap()
    );
    fs::write(&fixture.project.path, text).unwrap();
    let member = fixture.root().join("packages/member/pyproject.toml");
    fs::create_dir_all(member.parent().unwrap()).unwrap();
    fs::write(
        &member,
        "[project]\nname='member'\nversion='1'\n[tool.uv]\nindex-strategy='first-index'\n",
    )
    .unwrap();
    let projects = vec![
        read_project(&fixture.project.path).unwrap(),
        read_project(member).unwrap(),
    ];
    let before = fixture.lock();
    let error = prepare(fixture.root(), &projects, &ResolvePolicy::default(), false).unwrap_err();
    assert!(error.to_string().contains("workspace root"), "{error:#}");
    assert_eq!(fixture.lock(), before);
}

#[test]
fn doctor_copy_preserves_executable_mode() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !uv_available() {
            return;
        }
        let fixture = Fixture::new();
        fs::write(fixture.root().join("test.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(
            fixture.root().join("test.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let prepared = prepare(
            fixture.root(),
            std::slice::from_ref(&fixture.project),
            &ResolvePolicy::default(),
            true,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(prepared.directory().join("test.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}

#[test]
fn deadline_before_preparation_does_not_write() {
    let temp = tempfile::tempdir().unwrap();
    assert!(
        prepare_with_deadline(
            temp.path(),
            &[],
            &ResolvePolicy::default(),
            false,
            Instant::now() - Duration::from_secs(1)
        )
        .is_err()
    );
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed_or_applied() {
    use std::os::unix::fs::symlink;
    if !uv_available() {
        return;
    }
    let fixture = Fixture::new();
    let lock = fixture.root().join("uv.lock");
    let external = fixture.root().join("external.lock");
    fs::rename(&lock, &external).unwrap();
    symlink(&external, &lock).unwrap();
    assert!(
        prepare(
            fixture.root(),
            std::slice::from_ref(&fixture.project),
            &ResolvePolicy::default(),
            false
        )
        .is_err()
    );
    assert!(external.is_file());
}

// This helper runs alone in a subprocess so cancellation and PATH overrides
// cannot affect parallel tests in the parent process.
#[cfg(unix)]
#[test]
fn isolated_runner_scenario() {
    let Some(directory) = std::env::var_os("CU_RUNNER_SCENARIO") else {
        return;
    };
    let root = PathBuf::from(directory);
    let project = read_project(root.join("pyproject.toml")).unwrap();
    if std::env::var("CU_RUNNER_MODE").as_deref() == Ok("cancel") {
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            check_updates::INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }
    let start = Instant::now();
    let outcome = prepare_with_deadline(
        &root,
        &[project],
        &ResolvePolicy::default(),
        false,
        Instant::now() + Duration::from_millis(200),
    );
    if std::env::var("CU_RUNNER_MODE").as_deref() == Ok("clear") {
        assert!(outcome.is_ok(), "{outcome:?}");
        return;
    }
    let error = outcome.unwrap_err();
    assert!(!format!("{error:#}").contains("secret-fixture-token"));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[cfg(unix)]
#[test]
fn runner_timeout_kills_descendants_and_leaves_originals() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let root = temp.path();
    fs::write(
        root.join("pyproject.toml"),
        "[project]\nname='fixture'\nversion='1'\ndependencies=[]\n",
    )
    .unwrap();
    fs::write(
        root.join("uv.lock"),
        "version=1\nrevision=5\n[[package]]\nname='fixture'\nversion='1'\nsource={virtual='.'}\n",
    )
    .unwrap();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let marker = root.join("descendant-ran");
    fs::write(bin.join("uv"), "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(bin.join("uv"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let manifest = fs::read(root.join("pyproject.toml")).unwrap();
    let lock = fs::read(root.join("uv.lock")).unwrap();
    let mut modes = vec!["timeout", "cancel", "secret", "overflow", "env", "clear"];
    if cfg!(target_os = "linux") {
        modes.push("escaped");
    }
    for mode in modes {
        let script = match mode {
            "secret" => {
                "#!/bin/sh\nprintf secret-fixture-token\nprintf secret-fixture-token >&2\nexit 1\n"
                    .to_owned()
            }
            "overflow" => "#!/bin/sh\nyes secret-fixture-token\n".to_owned(),
            "env" => format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
            "clear" => "#!/bin/sh\n[ -z \"$UV_PROJECT$UV_FROZEN\" ] || exit 2\nprintf '%s' '--upgrade-package --exclude-newer --exclude-newer-package --prerelease --no-build --no-python-downloads --keyring-provider'\n".to_owned(),
            "escaped" => format!(
                "#!/bin/sh\nsetsid sh -c 'echo $$ > \"{0}\"; sleep 30' &\nwhile [ ! -f \"{0}\" ]; do sleep 0.01; done\nexit 0\n",
                root.join("escaped.pid").display()
            ),
            _ => format!(
                "#!/bin/sh\n(sleep 1; touch '{}') &\nsleep 5\n",
                marker.display()
            ),
        };
        fs::write(root.join("bin/uv"), script).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "isolated_runner_scenario", "--nocapture"])
            .env("CU_RUNNER_SCENARIO", root)
            .env("CU_RUNNER_MODE", mode)
            .env("PATH", &path);
        if mode == "env" {
            command.env("UV_RESOLUTION", "lowest");
        }
        if mode == "clear" {
            command
                .env("UV_PROJECT", "/invalid-project")
                .env("UV_FROZEN", "true");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-fixture-token"));
        if mode == "escaped"
            && let Ok(pid) = fs::read_to_string(root.join("escaped.pid"))
        {
            let pid: i32 = pid.trim().parse().unwrap();
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        if matches!(mode, "timeout" | "cancel") {
            std::thread::sleep(Duration::from_millis(1100));
        }
        assert!(!marker.exists());
        assert_eq!(fs::read(root.join("pyproject.toml")).unwrap(), manifest);
        assert_eq!(fs::read(root.join("uv.lock")).unwrap(), lock);
    }
    assert!(!root.join(".venv").exists());
}
