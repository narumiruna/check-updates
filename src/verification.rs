//! Resolve static uv projects in a temporary workspace, then apply guarded snapshots.
//!
//! This is not a sandbox or a transaction. Build backends, dynamic metadata,
//! relative/external sources, inherited uv configuration, and unknown resolver
//! overrides are refused. Static virtual workspace members and absolute wheel
//! directories are supported. Recovery never replaces bytes changed by another writer.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use pep440_rs::Version;
use pep508_rs::{Requirement, VersionOrUrl};
use tempfile::{NamedTempFile, TempDir};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use toml_edit::{DocumentMut, Item, TableLike, Value};

use crate::changes::{self, Change, safe_text};
use crate::dependencies::{dependencies, normalize};
use crate::output::Pending;
use crate::workspace::Project;

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 10_000;

#[derive(Clone, Debug, Default)]
pub struct ResolvePolicy {
    pub cutoff: Option<i64>,
    pub package_cutoffs: Vec<(String, i64)>,
    pub prerelease: bool,
}

#[derive(Debug)]
struct Snapshot {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
}

/// Temporary inputs. All project bytes must still match the caller's snapshots.
#[derive(Debug)]
pub struct Prepared {
    temp: TempDir,
    root: PathBuf,
    projects: Vec<Project>,
    snapshots: Vec<Snapshot>,
    policy: ResolvePolicy,
    copied_tree: bool,
    artifact_directories: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
}

/// A validated, relocatable lockfile and the exact manifest edits it resolved.
#[derive(Debug)]
pub struct Verified {
    prepared: Prepared,
    manifests: BTreeMap<PathBuf, Vec<u8>>,
    lock: Vec<u8>,
}

fn budget(deadline: Instant) -> Result<()> {
    crate::check_interrupt()?;
    ensure!(
        Instant::now() < deadline,
        "Verification time limit exceeded; original files were not changed"
    );
    Ok(())
}

fn regular_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "Verification requires regular input files (no symlinks)"
            );
            ensure!(
                meta.len() <= MAX_BYTES,
                "Verification input exceeds the size limit"
            );
            let mut bytes = Vec::new();
            fs::File::open(path)
                .context("Cannot read verification input")?
                .take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= MAX_BYTES,
                "Verification input exceeds the size limit"
            );
            Ok(Some(bytes))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Cannot inspect verification input"),
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => (),
            Component::ParentDir => {
                normalized.pop();
            }
            part => normalized.push(part.as_os_str()),
        }
    }
    Ok(normalized)
}

fn no_symlink_components(path: &Path) -> Result<()> {
    for component in path.ancestors() {
        if let Ok(meta) = fs::symlink_metadata(component) {
            ensure!(
                !meta.file_type().is_symlink(),
                "Verification does not support symlink input paths"
            );
        }
    }
    Ok(())
}

fn snapshot(snapshots: &mut Vec<Snapshot>, path: PathBuf) -> Result<()> {
    if !snapshots.iter().any(|s| s.path == path) {
        no_symlink_components(&path)?;
        let bytes = regular_bytes(&path)?;
        let total = snapshots
            .iter()
            .filter_map(|s| s.bytes.as_ref())
            .map(Vec::len)
            .sum::<usize>()
            + bytes.as_ref().map_or(0, Vec::len);
        ensure!(
            snapshots.len() < MAX_FILES && total as u64 <= MAX_BYTES,
            "Verification inputs exceed 10000 files or 64 MiB"
        );
        snapshots.push(Snapshot { bytes, path });
    }
    Ok(())
}

fn controlled_environment(command: &mut Command) -> Result<()> {
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("UV_") {
            // These values cannot affect resolution, or are explicitly controlled here.
            let allowed = matches!(
                name.as_ref(),
                "UV_PROJECT"
                    | "UV_FROZEN"
                    | "UV_PUBLISH_TOKEN"
                    | "UV_CACHE_DIR"
                    | "UV_OFFLINE"
                    | "UV_NO_PROGRESS"
                    | "UV_PYTHON_DOWNLOADS"
                    | "UV_PYTHON_INSTALL_DIR"
                    | "UV_TOOL_BIN_DIR"
                    | "UV_TOOL_DIR"
            ) || (name.starts_with("UV_INDEX_")
                && (name.ends_with("_USERNAME") || name.ends_with("_PASSWORD")));
            ensure!(
                allowed,
                "Verification does not support an inherited uv resolver override; remove the UV_* override and retry"
            );
            if !matches!(
                name.as_ref(),
                "UV_CACHE_DIR" | "UV_OFFLINE" | "UV_PYTHON_INSTALL_DIR"
            ) && !(name.starts_with("UV_INDEX_")
                && (name.ends_with("_USERNAME") || name.ends_with("_PASSWORD")))
            {
                command.env_remove(&key);
            }
        }
        ensure!(
            !matches!(name.as_ref(), "PYTHONPATH" | "PYTHONHOME"),
            "Verification does not support PYTHONPATH or PYTHONHOME overrides"
        );
    }
    command
        .env_remove("VIRTUAL_ENV")
        .env_remove("CONDA_PREFIX")
        .env("UV_PYTHON_DOWNLOADS", "never")
        .env("UV_NO_PROGRESS", "1");
    Ok(())
}

struct Running {
    child: std::process::Child,
    finished: bool,
}
impl Running {
    fn kill(&mut self) {
        if self.finished {
            return;
        }
        #[cfg(unix)]
        unsafe {
            // A dedicated process group also contains backend/helper descendants.
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.finished = true;
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.kill();
    }
}

fn run_uv(directory: &Path, args: &[String], deadline: Instant) -> Result<String> {
    budget(deadline)?;
    let mut command = Command::new("uv");
    command
        .args(args)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    controlled_environment(&mut command)?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = Running {
        child: command
            .spawn()
            .context("Cannot start uv for verification")?,
        finished: false,
    };
    let mut stdout = Capture::new(child.child.stdout.take().context("Missing uv stdout")?)?;
    let mut stderr = Capture::new(child.child.stderr.take().context("Missing uv stderr")?)?;
    let status = loop {
        budget(deadline)?;
        stdout.drain(deadline)?;
        stderr.drain(deadline)?;
        if let Some(status) = child
            .child
            .try_wait()
            .context("Cannot wait for uv verification")?
        {
            break status;
        }
        sleep(Duration::from_millis(25));
    };
    // No reader waits for EOF: a descendant can retain an inherited pipe even
    // after the direct child exits or leaves its process group.
    child.kill();
    stdout.drain(deadline)?;
    stderr.drain(deadline)?;
    if !status.success() {
        bail!(
            "uv verification failed ({status}). Resolver diagnostics were suppressed because they can contain credentials. Run uv lock --dry-run locally to inspect the resolver failure"
        );
    }
    Ok(String::from_utf8_lossy(&stdout.bytes).into_owned())
}

trait Pipe: Read {
    fn initialize(&self) -> std::io::Result<()>;
    fn available(&self) -> std::io::Result<usize>;
}

#[cfg(unix)]
impl<T: Read + std::os::fd::AsRawFd> Pipe for T {
    fn initialize(&self) -> std::io::Result<()> {
        unsafe {
            let flags = libc::fcntl(self.as_raw_fd(), libc::F_GETFL);
            if flags < 0
                || libc::fcntl(self.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
    fn available(&self) -> std::io::Result<usize> {
        Ok(4096)
    }
}

#[cfg(windows)]
impl<T: Read + std::os::windows::io::AsRawHandle> Pipe for T {
    fn initialize(&self) -> std::io::Result<()> {
        Ok(())
    }
    fn available(&self) -> std::io::Result<usize> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn PeekNamedPipe(
                handle: *mut std::ffi::c_void,
                buffer: *mut std::ffi::c_void,
                size: u32,
                read: *mut u32,
                available: *mut u32,
                remaining: *mut u32,
            ) -> i32;
        }
        let mut available = 0;
        let success = unsafe {
            PeekNamedPipe(
                self.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if success == 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(109 | 233)) {
                return Ok(0);
            }
            return Err(error);
        }
        Ok((available as usize).min(4096))
    }
}

struct Capture<T> {
    pipe: T,
    bytes: Vec<u8>,
}
impl<T: Pipe> Capture<T> {
    fn new(pipe: T) -> Result<Self> {
        pipe.initialize()
            .context("Cannot initialize nonblocking uv output")?;
        Ok(Self {
            pipe,
            bytes: Vec::new(),
        })
    }
    fn drain(&mut self, deadline: Instant) -> Result<()> {
        let mut chunk = [0; 4096];
        loop {
            budget(deadline)?;
            let available = self.pipe.available().context("Cannot inspect uv output")?;
            if available == 0 {
                return Ok(());
            }
            match self.pipe.read(&mut chunk[..available.min(4096)]) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    ensure!(
                        self.bytes.len() + count <= 64 * 1024,
                        "uv verification diagnostic output exceeded 64 KiB"
                    );
                    self.bytes.extend_from_slice(&chunk[..count]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("Cannot read uv output"),
            }
        }
    }
}

fn check_capabilities(directory: &Path, deadline: Instant) -> Result<()> {
    let help = run_uv(directory, &["help".into(), "lock".into()], deadline)?;
    for flag in [
        "--upgrade-package",
        "--exclude-newer",
        "--exclude-newer-package",
        "--prerelease",
        "--no-build",
        "--no-python-downloads",
        "--keyring-provider",
    ] {
        ensure!(
            help.contains(flag),
            "Installed uv does not support required verification capabilities"
        );
    }
    Ok(())
}

fn safe_location(value: &str) -> bool {
    // Windows drive paths parse as URLs with a one-letter scheme.
    if Path::new(value).is_absolute() {
        return true;
    }
    if let Ok(url) = url::Url::parse(value) {
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    } else {
        Path::new(value).is_absolute()
    }
}

fn check_source(source: &dyn TableLike) -> Result<()> {
    for (key, item) in source.iter() {
        ensure!(
            matches!(key, "workspace" | "index" | "extra" | "marker"),
            "Verification does not support relative, URL, Git, or external sources"
        );
        if key == "workspace" {
            ensure!(
                item.as_bool() == Some(true),
                "Verification supports only local workspace=true sources"
            );
        }
    }
    Ok(())
}

fn check_project(project: &Project) -> Result<()> {
    let doc = &project.document;
    ensure!(
        doc.get("build-system").is_none(),
        "Verification does not support build-system projects; --no-build does not prevent first-party backend execution"
    );
    ensure!(
        doc.get("project").and_then(|p| p.get("dynamic")).is_none(),
        "Verification does not support dynamic project metadata"
    );
    for dependency in dependencies(doc) {
        let requirement: Requirement = dependency.requirement.parse().map_err(|_| {
            anyhow::anyhow!("Verification requires static, valid dependency requirements")
        })?;
        ensure!(
            !matches!(requirement.version_or_url, Some(VersionOrUrl::Url(_))),
            "Verification does not support direct URL or path requirements"
        );
    }
    if let Some(uv) = doc.get("tool").and_then(|t| t.get("uv")) {
        let uv = uv.as_table_like().context("tool.uv must be a table")?;
        for (key, value) in uv.iter() {
            ensure!(
                matches!(
                    key,
                    "workspace"
                        | "sources"
                        | "index"
                        | "find-links"
                        | "no-index"
                        | "resolution"
                        | "index-strategy"
                        | "package"
                ),
                "Verification does not support this tool.uv setting; remove unsupported resolver/build policies explicitly"
            );
            match key {
                "index-strategy" => ensure!(
                    value.as_str() == Some("first-index"),
                    "Verification requires first-index routing to match metadata lookup"
                ),
                "package" => ensure!(
                    value.as_bool() == Some(false),
                    "Verification supports only non-build projects"
                ),
                "find-links" => {
                    let links = value.as_array().context("find-links must be an array")?;
                    for link in links {
                        ensure!(
                            link.as_str().is_some_and(safe_location),
                            "Verification requires absolute find-links without embedded credentials"
                        );
                    }
                }
                "index" => {
                    let indexes: Vec<&dyn TableLike> =
                        if let Some(tables) = value.as_array_of_tables() {
                            tables.iter().map(|t| t as &dyn TableLike).collect()
                        } else {
                            value
                                .as_array()
                                .context("index must be an array")?
                                .iter()
                                .map(|v| {
                                    v.as_inline_table()
                                        .map(|t| t as &dyn TableLike)
                                        .context("index entry must be a table")
                                })
                                .collect::<Result<_>>()?
                        };
                    for index in indexes {
                        ensure!(
                            index
                                .get("url")
                                .and_then(Item::as_str)
                                .is_some_and(safe_location),
                            "Verification requires absolute index URLs without embedded credentials"
                        );
                        if let (Some(name), Some(url)) = (
                            index.get("name").and_then(Item::as_str),
                            index.get("url").and_then(Item::as_str),
                        ) {
                            let name: String = name
                                .chars()
                                .map(|c| {
                                    if c.is_ascii_alphanumeric() {
                                        c.to_ascii_uppercase()
                                    } else {
                                        '_'
                                    }
                                })
                                .collect();
                            let credentials = [
                                format!("UV_INDEX_{name}_USERNAME"),
                                format!("UV_INDEX_{name}_PASSWORD"),
                            ]
                            .iter()
                            .any(|key| std::env::var_os(key).is_some());
                            ensure!(
                                !credentials
                                    || url::Url::parse(url)
                                        .is_ok_and(|url| url.scheme() == "https"),
                                "Verification requires HTTPS for authenticated indexes"
                            );
                        }
                        ensure!(
                            index.get("keyring-provider").is_none(),
                            "Verification does not execute keyring providers"
                        );
                    }
                }
                "sources" => {
                    for (_, source) in value
                        .as_table_like()
                        .context("sources must be a table")?
                        .iter()
                    {
                        if let Some(entries) = source.as_array() {
                            for entry in entries {
                                check_source(
                                    entry.as_inline_table().context("source must be a table")?,
                                )?;
                            }
                        } else if let Some(entries) = source.as_array_of_tables() {
                            for entry in entries {
                                check_source(entry)?;
                            }
                        } else {
                            check_source(
                                source.as_table_like().context("source must be a table")?,
                            )?;
                        }
                    }
                }
                _ => (),
            }
        }
    }
    Ok(())
}

fn member_paths(root: &Path, projects: &[Project], deadline: Instant) -> Result<BTreeSet<PathBuf>> {
    budget(deadline)?;
    let mut expected = BTreeSet::from([root.join("pyproject.toml")]);
    let workspace = projects[0]
        .document
        .get("tool")
        .and_then(|t| t.get("uv"))
        .and_then(|u| u.get("workspace"));
    if let Some(workspace) = workspace {
        let table = workspace
            .as_table_like()
            .context("workspace must be a table")?;
        let mut excluded = BTreeSet::new();
        for (key, target) in [("exclude", &mut excluded), ("members", &mut expected)] {
            if let Some(patterns) = table.get(key) {
                for pattern in patterns
                    .as_array()
                    .context("workspace patterns must be arrays")?
                {
                    budget(deadline)?;
                    let pattern = pattern
                        .as_str()
                        .context("workspace patterns must be strings")?;
                    ensure!(
                        !Path::new(pattern).is_absolute()
                            && !Path::new(pattern)
                                .components()
                                .any(|c| matches!(c, Component::ParentDir)),
                        "Verification requires workspace members inside the root"
                    );
                    let escaped = glob::Pattern::escape(
                        root.to_str().context("Workspace path is not UTF-8")?,
                    );
                    for entry in glob::glob_with(
                        &format!("{escaped}/{pattern}"),
                        glob::MatchOptions {
                            require_literal_separator: true,
                            ..Default::default()
                        },
                    )? {
                        budget(deadline)?;
                        ensure!(
                            target.len() < MAX_FILES,
                            "Verification has too many workspace members"
                        );
                        let path = absolute(&entry?)?.join("pyproject.toml");
                        no_symlink_components(&path)?;
                        target.insert(path);
                    }
                }
            }
        }
        for path in excluded {
            expected.remove(&path);
        }
        expected.insert(root.join("pyproject.toml"));
    }
    let supplied = projects
        .iter()
        .map(|p| absolute(&p.path))
        .collect::<Result<BTreeSet<_>>>()?;
    ensure!(
        expected == supplied,
        "Verification requires the complete workspace context; use -w and include every member"
    );
    Ok(expected)
}

fn local_artifact_files(directory: &Path) -> Result<BTreeSet<PathBuf>> {
    no_symlink_components(directory)?;
    let mut files = BTreeSet::new();
    for entry in fs::read_dir(directory).context("Cannot read local wheel directory")? {
        let entry = entry?;
        ensure!(
            entry.file_type()?.is_file(),
            "Verification local wheel directories require regular files only (no symlinks or nested directories)"
        );
        ensure!(
            files.len() < MAX_FILES,
            "Local wheel directory has too many files"
        );
        ensure!(
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "whl"),
            "Verification local artifact directories support wheel files only"
        );
        files.insert(entry.path());
    }
    Ok(files)
}

fn local_artifact_directories(projects: &[Project]) -> Result<BTreeSet<PathBuf>> {
    let mut directories = BTreeSet::new();
    for project in projects {
        if let Some(links) = project
            .document
            .get("tool")
            .and_then(|t| t.get("uv"))
            .and_then(|u| u.get("find-links"))
            .and_then(Item::as_array)
        {
            for link in links {
                if let Some(path) = link.as_str().filter(|s| Path::new(s).is_absolute()) {
                    directories.insert(PathBuf::from(path));
                }
            }
        }
    }
    Ok(directories)
}

fn config_snapshots(
    root: &Path,
    projects: &[Project],
    snapshots: &mut Vec<Snapshot>,
) -> Result<()> {
    let mut directories = BTreeSet::new();
    for project in projects {
        let path = absolute(&project.path)?;
        for directory in path.parent().context("Project has no parent")?.ancestors() {
            directories.insert(directory.to_owned());
        }
    }
    for directory in directories {
        let config = directory.join("uv.toml");
        snapshot(snapshots, config.clone())?;
        ensure!(
            !config.try_exists()?,
            "Verification does not support uv.toml configuration; it cannot be silently dropped"
        );
        let python = directory.join(".python-version");
        snapshot(snapshots, python.clone())?;
        ensure!(
            directory.starts_with(root) || !python.try_exists()?,
            "Verification does not support inherited .python-version files"
        );
        if !directory.starts_with(root) {
            let path = directory.join("pyproject.toml");
            snapshot(snapshots, path.clone())?;
            if let Some(bytes) = regular_bytes(&path)? {
                let document: DocumentMut = std::str::from_utf8(&bytes)?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Cannot parse inherited project configuration"))?;
                ensure!(
                    document.get("tool").and_then(|t| t.get("uv")).is_none(),
                    "Verification does not support inherited uv configuration or an enclosing workspace; use -w at its root"
                );
            }
        }
    }
    // uv also discovers user/system config independently of the project tree.
    let mut global = Vec::new();
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        global.push(PathBuf::from(path).join("uv/uv.toml"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        global.push(PathBuf::from(home).join(".config/uv/uv.toml"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        global.push(PathBuf::from(appdata).join("uv/uv.toml"));
    }
    #[cfg(unix)]
    // macOS /etc is a system alias for /private/etc, not a project symlink.
    global.push(
        fs::canonicalize("/etc")
            .unwrap_or_else(|_| PathBuf::from("/etc"))
            .join("uv/uv.toml"),
    );
    #[cfg(windows)]
    if let Some(programdata) = std::env::var_os("PROGRAMDATA") {
        global.push(PathBuf::from(programdata).join("uv/uv.toml"));
    }
    for path in global {
        snapshot(snapshots, path.clone())?;
        ensure!(
            !path.try_exists()?,
            "Verification does not support inherited user/system uv configuration"
        );
    }
    Ok(())
}

fn tree_paths(root: &Path, deadline: Instant) -> Result<Vec<PathBuf>> {
    let mut pending = vec![root.to_owned()];
    let mut paths = Vec::new();
    let mut total = 0;
    let mut directories = 0;
    while let Some(directory) = pending.pop() {
        budget(deadline)?;
        directories += 1;
        ensure!(
            directories <= MAX_FILES,
            "Doctor copy exceeds 10000 directories"
        );
        for entry in fs::read_dir(directory)? {
            budget(deadline)?;
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(!kind.is_symlink(), "Doctor copy does not support symlinks");
            if kind.is_dir() {
                if !matches!(
                    entry.file_name().to_str(),
                    Some(
                        ".git"
                            | ".venv"
                            | "target"
                            | "dist"
                            | ".cache"
                            | "__pycache__"
                            | ".pytest_cache"
                            | ".mypy_cache"
                            | ".ruff_cache"
                    )
                ) {
                    pending.push(entry.path());
                }
            } else {
                ensure!(kind.is_file(), "Doctor copy requires regular files");
                total += entry.metadata()?.len();
                paths.push(entry.path());
                ensure!(
                    paths.len() <= MAX_FILES && total <= MAX_BYTES,
                    "Doctor copy exceeds 10000 files or 64 MiB"
                );
            }
        }
    }
    paths.sort();
    Ok(paths)
}

/// Prepare static resolver inputs. `allow_builds` also copies source/test files
/// for Doctor, but does not authorize unsupported dynamic/build-system projects.
pub fn prepare(
    root: &Path,
    projects: &[Project],
    policy: &ResolvePolicy,
    allow_builds: bool,
) -> Result<Prepared> {
    prepare_with_deadline(
        root,
        projects,
        policy,
        allow_builds,
        Instant::now() + TIMEOUT,
    )
}

pub fn prepare_with_deadline(
    root: &Path,
    projects: &[Project],
    policy: &ResolvePolicy,
    allow_builds: bool,
    deadline: Instant,
) -> Result<Prepared> {
    budget(deadline)?;
    let root = absolute(root)?;
    no_symlink_components(&root)?;
    ensure!(
        !projects.is_empty(),
        "Verification requires project context"
    );
    ensure!(
        projects.len() <= MAX_FILES,
        "Verification has too many workspace projects"
    );
    ensure!(
        absolute(&projects[0].path)? == root.join("pyproject.toml"),
        "Verification must run at the workspace root; use -w"
    );
    member_paths(&root, projects, deadline)?;
    let mut snapshots = Vec::new();
    for (position, project) in projects.iter().enumerate() {
        budget(deadline)?;
        ensure!(
            project.document.to_string().as_bytes() == project.original,
            "Project document differs from its original source snapshot"
        );
        check_project(project)?;
        if position > 0
            && let Some(uv) = project
                .document
                .get("tool")
                .and_then(|t| t.get("uv"))
                .and_then(Item::as_table_like)
        {
            ensure!(
                uv.iter()
                    .all(|(key, _)| matches!(key, "sources" | "package")),
                "Verification requires resolver/index settings at the workspace root; member-specific settings cannot be mapped safely to uv"
            );
        }
        let path = absolute(&project.path)?;
        ensure!(
            path.starts_with(&root),
            "Verification does not support external workspace members"
        );
        snapshot(&mut snapshots, path.clone())?;
        ensure!(
            regular_bytes(&path)?.as_deref() == Some(project.original.as_slice()),
            "Project changed during review; original files were not changed"
        );
    }
    config_snapshots(&root, projects, &mut snapshots)?;
    let mut artifact_directories = BTreeMap::new();
    for directory in local_artifact_directories(projects)? {
        budget(deadline)?;
        let files = local_artifact_files(&directory)?;
        for path in &files {
            budget(deadline)?;
            snapshot(&mut snapshots, path.clone())?;
        }
        artifact_directories.insert(directory, files);
    }
    let lock_path = root.join("uv.lock");
    snapshot(&mut snapshots, lock_path.clone())?;
    let lock =
        regular_bytes(&lock_path)?.context("Verification requires an existing regular uv.lock")?;
    validate_lock(&lock, &root, projects, None, None)?;
    if allow_builds {
        for path in tree_paths(&root, deadline)? {
            snapshot(&mut snapshots, path)?;
        }
    }
    let total: usize = snapshots
        .iter()
        .filter_map(|s| s.bytes.as_ref())
        .map(Vec::len)
        .sum();
    ensure!(
        snapshots.len() <= MAX_FILES && total as u64 <= MAX_BYTES,
        "Verification inputs exceed 10000 files or 64 MiB"
    );
    let temporary_parent = std::env::temp_dir()
        .canonicalize()
        .context("Cannot locate temporary verification directory")?;
    let temp = tempfile::tempdir_in(temporary_parent)
        .context("Cannot create temporary verification workspace")?;
    // The temporary location must not introduce a different enclosing workspace,
    // resolver configuration, or interpreter request.
    for directory in temp.path().parent().into_iter().flat_map(Path::ancestors) {
        for filename in ["uv.toml", ".python-version", "pyproject.toml"] {
            let path = directory.join(filename);
            snapshot(&mut snapshots, path.clone())?;
            if let Some(bytes) = regular_bytes(&path)? {
                if filename == "pyproject.toml" {
                    let document: DocumentMut =
                        std::str::from_utf8(&bytes)?.parse().map_err(|_| {
                            anyhow::anyhow!("Cannot parse temporary parent project configuration")
                        })?;
                    ensure!(
                        document.get("tool").and_then(|t| t.get("uv")).is_none(),
                        "Temporary directory has unsupported inherited uv configuration"
                    );
                } else {
                    bail!(
                        "Temporary directory has unsupported inherited uv configuration or interpreter selection"
                    );
                }
            }
        }
    }
    check_capabilities(temp.path(), deadline)?;
    for input in &snapshots {
        budget(deadline)?;
        if let (Ok(relative), Some(bytes)) = (input.path.strip_prefix(&root), &input.bytes) {
            let destination = temp.path().join(relative);
            fs::create_dir_all(destination.parent().context("Input has no parent")?)?;
            fs::write(&destination, bytes)?;
            fs::set_permissions(destination, fs::metadata(&input.path)?.permissions())?;
        }
    }
    let prepared = Prepared {
        temp,
        root,
        projects: projects.to_vec(),
        snapshots,
        policy: policy.clone(),
        copied_tree: allow_builds,
        artifact_directories,
    };
    prepared.check_snapshots_with_deadline(deadline)?;
    Ok(prepared)
}

pub fn preflight(
    root: &Path,
    projects: &[Project],
    pending: &Pending,
    policy: &ResolvePolicy,
) -> Result<Verified> {
    prepare(root, projects, policy, false)?.resolve(pending)
}

fn timestamp(seconds: i64) -> Result<String> {
    Ok(OffsetDateTime::from_unix_timestamp(seconds)
        .context("Invalid resolver cutoff")?
        .format(&Rfc3339)?)
}

impl Prepared {
    pub fn directory(&self) -> &Path {
        self.temp.path()
    }

    pub fn check_snapshots(&self) -> Result<()> {
        self.check_snapshots_with_deadline(Instant::now() + TIMEOUT)
    }

    pub fn check_snapshots_with_deadline(&self, deadline: Instant) -> Result<()> {
        budget(deadline)?;
        for input in &self.snapshots {
            budget(deadline)?;
            no_symlink_components(&input.path)?;
            ensure!(
                regular_bytes(&input.path)? == input.bytes,
                "Verification input changed during review; no further files were written. Review the affected manifests and uv.lock, then retry"
            );
        }
        member_paths(&self.root, &self.projects, deadline)?;
        for (directory, files) in &self.artifact_directories {
            budget(deadline)?;
            ensure!(
                &local_artifact_files(directory)? == files,
                "Local wheel directory changed during review; original files were not changed"
            );
        }
        if self.copied_tree {
            let current = tree_paths(&self.root, deadline)?;
            let original: Vec<_> = self
                .snapshots
                .iter()
                .filter(|s| s.bytes.is_some() && s.path.starts_with(&self.root))
                .map(|s| s.path.clone())
                .collect();
            ensure!(
                current.into_iter().collect::<BTreeSet<_>>() == original.into_iter().collect(),
                "Source tree changed during review; no files were written"
            );
        }
        Ok(())
    }

    pub fn resolve(self, pending: &Pending) -> Result<Verified> {
        self.resolve_with_deadline(pending, Instant::now() + TIMEOUT)
    }

    pub fn resolve_with_deadline(self, pending: &Pending, deadline: Instant) -> Result<Verified> {
        budget(deadline)?;
        self.check_snapshots_with_deadline(deadline)?;
        let mut manifests = BTreeMap::new();
        let mut selected = BTreeMap::<String, Version>::new();
        for (project, updates) in pending {
            budget(deadline)?;
            let path = absolute(&project.path)?;
            let original = self
                .projects
                .iter()
                .find(|p| absolute(&p.path).is_ok_and(|p| p == path))
                .context("Pending project is not in the prepared workspace")?;
            ensure!(
                original.original == project.original,
                "Pending project snapshot differs from prepared input"
            );
            let change = changes::prepare_change(original, updates)?;
            let changed_project = Project {
                path: original.path.clone(),
                original: change.content.clone(),
                document: std::str::from_utf8(&change.content)?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Cannot parse proposed verification input"))?,
            };
            check_project(&changed_project)?;
            ensure!(
                manifests
                    .insert(path.clone(), change.content.clone())
                    .is_none(),
                "Duplicate pending project"
            );
            for update in updates {
                let requirement: Requirement = update
                    .dependency
                    .requirement
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid selected dependency requirement"))?;
                let name = normalize(requirement.name.as_ref());
                if let Some(previous) = selected.insert(name, update.version.clone()) {
                    ensure!(
                        previous == update.version,
                        "Conflicting marker-specific selected versions are not supported by verification"
                    );
                }
            }
            fs::write(
                self.directory().join(path.strip_prefix(&self.root)?),
                &change.content,
            )?;
        }
        let mut args: Vec<String> = [
            "lock",
            "--no-build",
            "--no-python-downloads",
            "--keyring-provider",
            "disabled",
            "--prerelease",
            if self.policy.prerelease {
                "allow"
            } else {
                "disallow"
            },
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        if self.policy.cutoff.is_some() || !self.policy.package_cutoffs.is_empty() {
            // A full fresh resolution avoids retaining a recent, already-locked artifact.
            fs::remove_file(self.directory().join("uv.lock"))?;
            ensure!(
                !self.projects.iter().any(|p| p
                    .document
                    .get("tool")
                    .and_then(|t| t.get("uv"))
                    .is_some_and(|u| u.get("find-links").is_some())),
                "Cooldown verification requires timestamped registry artifacts; find-links has no upload timestamps"
            );
        }
        if let Some(cutoff) = self.policy.cutoff {
            args.extend(["--exclude-newer".into(), timestamp(cutoff)?]);
        }
        let mut package_cutoffs = BTreeSet::new();
        for (name, cutoff) in &self.policy.package_cutoffs {
            let requirement: Requirement = name
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid package cutoff name"))?;
            ensure!(
                requirement.version_or_url.is_none()
                    && requirement.extras.is_empty()
                    && requirement.marker.is_true(),
                "Package cutoff requires a plain package name"
            );
            let name = normalize(requirement.name.as_ref());
            ensure!(
                package_cutoffs.insert(name.clone()),
                "Duplicate normalized package cutoff"
            );
            args.extend([
                "--exclude-newer-package".into(),
                format!("{name}={}", timestamp(*cutoff)?),
            ]);
        }
        for (name, version) in &selected {
            args.extend(["--upgrade-package".into(), format!("{name}=={version}")]);
        }
        run_uv(self.directory(), &args, deadline)?;
        budget(deadline)?;
        self.check_snapshots_with_deadline(deadline)?;
        let lock = regular_bytes(&self.directory().join("uv.lock"))?
            .context("uv did not create a regular lockfile")?;
        validate_lock(
            &lock,
            &self.root,
            &self.projects,
            Some(self.directory()),
            Some(&selected),
        )?;
        Ok(Verified {
            prepared: self,
            manifests,
            lock,
        })
    }
}

fn strings(value: &Value, check: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
    match value {
        Value::String(value) => check(value.value()),
        Value::Array(array) => {
            for value in array {
                strings(value, check)?;
            }
            Ok(())
        }
        Value::InlineTable(table) => {
            for (_, value) in table.iter() {
                strings(value, check)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn item_strings(item: &Item, check: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
    if let Some(value) = item.as_value() {
        strings(value, check)?;
    }
    if let Some(table) = item.as_table_like() {
        for (_, item) in table.iter() {
            item_strings(item, check)?;
        }
    }
    if let Some(array) = item.as_array_of_tables() {
        for table in array {
            for (_, item) in table.iter() {
                item_strings(item, check)?;
            }
        }
    }
    Ok(())
}

fn validate_lock(
    bytes: &[u8],
    root: &Path,
    projects: &[Project],
    temporary: Option<&Path>,
    selected: Option<&BTreeMap<String, Version>>,
) -> Result<()> {
    let text = std::str::from_utf8(bytes).context("uv.lock is not UTF-8")?;
    let document: DocumentMut = text
        .parse()
        .map_err(|_| anyhow::anyhow!("Cannot parse uv.lock"))?;
    ensure!(
        document.get("version").and_then(Item::as_integer) == Some(1),
        "Unsupported uv.lock format"
    );
    let revision = document
        .get("revision")
        .map(|item| item.as_integer().context("Invalid uv.lock revision"))
        .transpose()?
        .unwrap_or(0);
    ensure!((0..=5).contains(&revision), "Unsupported uv.lock revision");
    if let Some(temporary) = temporary {
        let native = temporary.to_string_lossy();
        let forward = native.replace('\\', "/");
        let encoded = url::Url::from_directory_path(temporary)
            .ok()
            .map(|u| u.to_string());
        ensure!(
            !text.contains(native.as_ref())
                && !text.contains(&forward)
                && !encoded.as_ref().is_some_and(|path| text.contains(path)),
            "Resolved lockfile contains temporary workspace paths and cannot be applied"
        );
        item_strings(document.as_item(), &mut |value| {
            ensure!(
                !value.contains(native.as_ref()) && !value.contains(&forward),
                "Resolved lockfile contains temporary paths"
            );
            Ok(())
        })?;
    }
    let members = projects
        .iter()
        .map(|p| {
            absolute(&p.path).map(|p| p.parent().unwrap().strip_prefix(root).unwrap().to_owned())
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let mut found = BTreeSet::new();
    let local_directories = local_artifact_directories(projects)?;
    for package in document
        .get("package")
        .and_then(Item::as_array_of_tables)
        .context("uv.lock has no package records")?
    {
        let source = package
            .get("source")
            .and_then(Item::as_table_like)
            .context("Lock package has no source")?;
        ensure!(source.len() == 1, "Ambiguous lock package source");
        if let Some(path) = source.get("virtual").and_then(Item::as_str) {
            ensure!(
                !Path::new(path).is_absolute()
                    && !Path::new(path)
                        .components()
                        .any(|c| matches!(c, Component::ParentDir)),
                "Lockfile contains an external project source"
            );
            let relative = if path == "." {
                PathBuf::new()
            } else {
                PathBuf::from(path)
            };
            ensure!(
                members.contains(&relative),
                "Lockfile references a project outside the prepared workspace"
            );
        } else {
            let registry = source.get("registry").and_then(Item::as_str).context("Verification supports only registry wheels and static virtual projects, not editable/path/Git sources")?;
            ensure!(
                safe_location(registry),
                "Lockfile contains a relative or credential-bearing registry source"
            );
            if temporary.is_some() && Path::new(registry).is_absolute() {
                ensure!(
                    local_directories.contains(Path::new(registry)),
                    "Resolved lock references an unverified local artifact directory"
                );
            }
        }
        if let Some(selected) = selected {
            let name = normalize(
                package
                    .get("name")
                    .and_then(Item::as_str)
                    .context("Lock package has no name")?,
            );
            if let Some(expected) = selected.get(&name) {
                let actual: Version = package
                    .get("version")
                    .and_then(Item::as_str)
                    .context("Selected package has no locked version")?
                    .parse()
                    .map_err(|_| {
                        anyhow::anyhow!("Selected package has an invalid locked version")
                    })?;
                ensure!(
                    &actual == expected,
                    "Resolved package version does not match the exact selected candidate"
                );
                ensure!(
                    source.get("registry").is_some(),
                    "Selected candidate resolved to a non-registry source"
                );
                found.insert(name);
            }
        }
    }
    if let Some(selected) = selected {
        ensure!(
            found.len() == selected.len(),
            "An exact selected candidate is absent from the resolved lockfile (inactive marker or unsupported fork)"
        );
    }
    Ok(())
}

impl Verified {
    pub fn directory(&self) -> &Path {
        self.prepared.directory()
    }
    pub fn check_snapshots(&self) -> Result<()> {
        self.prepared.check_snapshots()
    }
    pub fn check_snapshots_with_deadline(&self, deadline: Instant) -> Result<()> {
        self.prepared.check_snapshots_with_deadline(deadline)
    }

    /// Apply only the exact verified edits. Each replacement has a final byte
    /// guard. On failure, restore only files that still contain our bytes.
    /// There remains a concurrent-write window between the guard and rename.
    pub fn apply(&self, changes: &[Change<'_>]) -> Result<()> {
        self.apply_inner(changes, |_| Ok(()))
    }

    fn apply_inner(
        &self,
        changes: &[Change<'_>],
        mut after_write: impl FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        self.check_snapshots()?;
        let mut supplied = BTreeMap::new();
        for change in changes {
            let path = absolute(&change.project.path)?;
            ensure!(
                supplied.insert(path, change.content.clone()).is_none(),
                "Duplicate application path"
            );
        }
        ensure!(
            supplied == self.manifests,
            "Application edits differ from the verified candidate set"
        );
        let lock_path = self.prepared.root.join("uv.lock");
        supplied.insert(lock_path, self.lock.clone());
        let mut written = Vec::new();
        let result = (|| -> Result<()> {
            for (path, bytes) in &supplied {
                crate::check_interrupt()?;
                let before = self
                    .prepared
                    .snapshots
                    .iter()
                    .find(|s| &s.path == path)
                    .and_then(|s| s.bytes.as_ref())
                    .context("Missing original application snapshot")?;
                guarded_replace(path, before, bytes)?;
                written.push((path, before, bytes));
                after_write(path)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let mut unrestored = Vec::new();
            for (path, before, bytes) in written.into_iter().rev() {
                if guarded_replace(path, bytes, before).is_err() {
                    unrestored.push(safe_text(&path.to_string_lossy(), false));
                }
            }
            if unrestored.is_empty() {
                return Err(error.context("Verified application failed; tool-written files were restored. Review manifests and uv.lock before retrying"));
            }
            bail!(
                "{error:#}\nRecovery did not overwrite concurrent changes. Review these files and uv.lock together: {}. Restore only reviewed files from your backup or version control; do not reset the whole worktree",
                unrestored.join(", ")
            );
        }
        Ok(())
    }
}

fn guarded_replace(path: &Path, before: &[u8], after: &[u8]) -> Result<()> {
    no_symlink_components(path)?;
    ensure!(
        regular_bytes(path)?.as_deref() == Some(before),
        "Application input changed; refusing to overwrite it"
    );
    let mut replacement =
        NamedTempFile::new_in(path.parent().context("Application path has no parent")?)?;
    use std::io::Write;
    replacement
        .as_file()
        .set_permissions(fs::metadata(path)?.permissions())?;
    replacement.write_all(after)?;
    replacement.as_file().sync_all()?;
    ensure!(
        regular_bytes(path)?.as_deref() == Some(before),
        "Application input changed during replacement; refusing to overwrite it"
    );
    replacement
        .persist(path)
        .map_err(|e| e.error)
        .context("Cannot replace verified input")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_first_index_strategy_matches_metadata_routing() {
        for (strategy, valid) in [
            ("first-index", true),
            ("unsafe-best-match", false),
            ("unsafe-first-match", false),
        ] {
            let text = format!(
                "[project]\nname='fixture'\nversion='1'\n[tool.uv]\nindex-strategy='{strategy}'\n"
            );
            let project = Project {
                path: "pyproject.toml".into(),
                document: text.parse().unwrap(),
                original: text.into_bytes(),
            };
            assert_eq!(check_project(&project).is_ok(), valid);
        }
    }

    #[test]
    fn authenticated_http_indexes_are_refused_without_exposing_credentials() {
        const FLAG: &str = "CHECK_UPDATES_AUTH_FIXTURE";
        if std::env::var_os(FLAG).is_some() {
            let text = "[project]\nname='fixture'\nversion='1'\n[[tool.uv.index]]\nname='fixture-private'\nurl='http://example.invalid/simple/'\n";
            let project = Project {
                path: "pyproject.toml".into(),
                document: text.parse().unwrap(),
                original: text.as_bytes().to_vec(),
            };
            let error = check_project(&project).unwrap_err();
            assert!(error.to_string().contains("HTTPS"));
            assert!(!error.to_string().contains("credential-fixture-secret"));
            return;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact","verification::tests::authenticated_http_indexes_are_refused_without_exposing_credentials"])
            .env(FLAG,"1").env("UV_INDEX_FIXTURE_PRIVATE_PASSWORD","credential-fixture-secret")
            .stdin(Stdio::null()).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("credential-fixture-secret"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("credential-fixture-secret"));
    }

    #[test]
    fn absolute_artifact_locations_are_not_mistaken_for_url_schemes() {
        let temp = tempfile::tempdir().unwrap();
        assert!(safe_location(temp.path().to_str().unwrap()));
        assert!(safe_location("https://example.org/simple/"));
        assert!(!safe_location("https://user:secret@example.org/simple/"));
        assert!(!safe_location("wheels/relative"));
        #[cfg(windows)]
        {
            assert!(safe_location(r"C:\wheels"));
            assert!(safe_location("C:/wheels"));
        }
    }

    fn result_fixture() -> (TempDir, Project, Verified) {
        let original = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let root = original.path().to_owned();
        let path = root.join("pyproject.toml");
        fs::write(&path, "[project]\nname='fixture'\nversion='1'\n").unwrap();
        fs::write(root.join("uv.lock"), "original lock").unwrap();
        let project = crate::workspace::read_project(&path).unwrap();
        let snapshots = vec![
            Snapshot {
                path: path.clone(),
                bytes: Some(project.original.clone()),
            },
            Snapshot {
                path: root.join("uv.lock"),
                bytes: Some(b"original lock".to_vec()),
            },
        ];
        let prepared = Prepared {
            temp: tempfile::tempdir().unwrap(),
            root,
            projects: vec![project.clone()],
            snapshots,
            policy: ResolvePolicy::default(),
            copied_tree: false,
            artifact_directories: BTreeMap::new(),
        };
        let verified = Verified {
            prepared,
            manifests: BTreeMap::from([(path, b"verified manifest".to_vec())]),
            lock: b"verified lock".to_vec(),
        };
        (original, project, verified)
    }

    #[test]
    fn failed_final_write_restores_manifest_and_lock_together() {
        let (_directory, project, verified) = result_fixture();
        let change = Change {
            project: &project,
            content: b"verified manifest".to_vec(),
            count: 1,
        };
        let mut writes = 0;
        let error = verified
            .apply_inner(&[change], |_| {
                writes += 1;
                if writes == 2 {
                    bail!("injected final write failure");
                }
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("restored"));
        assert_eq!(fs::read(&project.path).unwrap(), project.original);
        assert_eq!(
            fs::read(verified.prepared.root.join("uv.lock")).unwrap(),
            b"original lock"
        );
    }

    #[test]
    fn recovery_preserves_a_concurrent_user_edit() {
        let (_directory, project, verified) = result_fixture();
        let change = Change {
            project: &project,
            content: b"verified manifest".to_vec(),
            count: 1,
        };
        let error = verified
            .apply_inner(&[change], |path| {
                fs::write(path, b"concurrent user edit")?;
                bail!("injected write failure")
            })
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("did not overwrite concurrent changes")
        );
        assert_eq!(fs::read(&project.path).unwrap(), b"concurrent user edit");
        assert_eq!(
            fs::read(verified.prepared.root.join("uv.lock")).unwrap(),
            b"original lock"
        );
    }

    #[test]
    fn lock_validation_refuses_temporary_paths_unknown_revisions_and_wrong_versions() {
        let (directory, project, _verified) = result_fixture();
        let selected = BTreeMap::from([("demo".to_owned(), "2".parse().unwrap())]);
        let base = "version=1\nrevision=5\n[[package]]\nname='demo'\nversion='2'\nsource={registry='https://example.invalid/simple'}\n";
        assert!(
            validate_lock(
                base.as_bytes(),
                directory.path(),
                std::slice::from_ref(&project),
                None,
                Some(&selected)
            )
            .is_ok()
        );
        for invalid in [
            base.replace("revision=5", "revision=-1"),
            base.replace("revision=5", "revision=6"),
            base.replace("revision=5", "revision='5'"),
            base.replace("revision=5", "revision=false"),
            base.replace("version='2'", "version='1'"),
        ] {
            assert!(
                validate_lock(
                    invalid.as_bytes(),
                    directory.path(),
                    std::slice::from_ref(&project),
                    None,
                    Some(&selected)
                )
                .is_err()
            );
        }
        let temporary = tempfile::tempdir().unwrap();
        let invalid = format!(
            "{base}\n[metadata]\npath={}\n",
            Value::from(temporary.path().to_str().unwrap())
        );
        assert!(
            validate_lock(
                invalid.as_bytes(),
                directory.path(),
                &[project],
                Some(temporary.path()),
                None
            )
            .is_err()
        );
    }
}
