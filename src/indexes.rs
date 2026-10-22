//! Release metadata from public PyPI and first-match Simple indexes.

use anyhow::{Result, anyhow, bail};
use pep440_rs::Version;
use percent_encoding::percent_decode_str;
use reqwest::blocking::{Client, ClientBuilder, Request};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use scraper::{Html, Selector};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const CACHE_TTL: f64 = 3600.0;
const INVALID_INDEX: &str =
    "Invalid index URL or authentication policy. Check authenticate and index credentials.";

type CacheKey = (String, Vec<Index>);
type Metadata = (Vec<Release>, Option<String>);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Release {
    pub version: Version,
    pub requires_python: Option<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Index {
    pub url: String,
    pub name: Option<String>,
    pub explicit: bool,
    pub default: bool,
    pub authenticate: String,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            url: String::new(),
            name: None,
            explicit: false,
            default: false,
            authenticate: "auto".into(),
        }
    }
}

fn normalize(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    let mut separator = false;
    for c in name.chars().flat_map(char::to_lowercase) {
        if matches!(c, '-' | '_' | '.') {
            if !separator {
                result.push('-');
            }
            separator = true;
        } else {
            result.push(c);
            separator = false;
        }
    }
    result
}

fn printable_url(value: &str) -> bool {
    // Python str.isprintable excludes Unicode separators and format characters.
    static NONPRINTABLE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[\p{C}\p{Z}]").unwrap());
    !NONPRINTABLE.is_match(value)
}

fn raw_authority(value: &str) -> Option<&str> {
    let (_, rest) = value.split_once("://")?;
    rest.split(['/', '?', '#'])
        .next()
        .filter(|part| !part.is_empty())
}

impl Index {
    fn credentials(
        &self,
        parsed: &Url,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<(String, String)> {
        if !matches!(self.authenticate.as_str(), "auto" | "always" | "never") {
            bail!(INVALID_INDEX);
        }
        let mut username = percent_decode_str(parsed.username())
            .decode_utf8_lossy()
            .into_owned();
        let mut password = percent_decode_str(parsed.password().unwrap_or(""))
            .decode_utf8_lossy()
            .into_owned();
        if let Some(name) = self.name.as_ref().filter(|name| !name.is_empty()) {
            let key: String = name
                .to_uppercase()
                .chars()
                .map(|c| {
                    if c.is_ascii_uppercase() || c.is_ascii_digit() {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let prefix = format!("UV_INDEX_{key}");
            username = env(&format!("{prefix}_USERNAME")).unwrap_or(username);
            password = env(&format!("{prefix}_PASSWORD")).unwrap_or(password);
        }
        // Check the raw authority as Url removes an empty userinfo component.
        let has_url_credentials = raw_authority(&self.url).is_some_and(|value| value.contains('@'));
        if self.authenticate == "never"
            && (has_url_credentials || !username.is_empty() || !password.is_empty())
        {
            bail!(INVALID_INDEX);
        }
        if self.authenticate == "always" && username.is_empty() && password.is_empty() {
            bail!(INVALID_INDEX);
        }
        if (!username.is_empty() || !password.is_empty()) && parsed.scheme() != "https" {
            bail!(INVALID_INDEX);
        }
        Ok((username, password))
    }

    fn request(&self, name: &str, client: &Client) -> Result<(Request, String)> {
        if !printable_url(&self.url)
            || self.url.contains('\\')
            || raw_authority(&self.url).is_none()
        {
            bail!(INVALID_INDEX);
        }
        let mut parsed = Url::parse(&self.url).map_err(|_| anyhow!(INVALID_INDEX))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || parsed.query().is_some_and(|q| !q.is_empty())
            || parsed.fragment().is_some_and(|f| !f.is_empty())
            || parsed.port() == Some(0)
        {
            bail!(INVALID_INDEX);
        }
        let (username, password) = self.credentials(&parsed, |key| std::env::var(key).ok())?;
        // Keep an explicitly supplied default port in the display link, as Python does.
        let explicit_port = raw_authority(&self.url)
            .and_then(|a| a.rsplit('@').next())
            .and_then(|a| {
                let (_, port) = a.rsplit_once(':')?;
                if port.is_empty() || !port.bytes().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                port.parse::<u16>().ok()
            });
        parsed
            .set_username("")
            .map_err(|_| anyhow!(INVALID_INDEX))?;
        parsed
            .set_password(None)
            .map_err(|_| anyhow!(INVALID_INDEX))?;
        parsed.set_query(None);
        parsed.set_fragment(None);
        let path = format!(
            "{}/{}/",
            parsed.path().trim_end_matches('/'),
            normalize(name)
        );
        parsed.set_path(&path);
        let link = if let Some(port) = explicit_port {
            format!(
                "{}://{}:{}{}",
                parsed.scheme(),
                parsed.host_str().unwrap(),
                port,
                parsed.path()
            )
        } else {
            parsed.to_string()
        };
        let mut builder = client.get(parsed).header(
            ACCEPT,
            "application/vnd.pypi.simple.v1+json, text/html;q=0.9",
        );
        if !username.is_empty() || !password.is_empty() {
            builder = builder.basic_auth(username, Some(password));
        }
        let request = builder.build().map_err(|_| anyhow!(INVALID_INDEX))?;
        Ok((request, link))
    }
}

fn safe_redirect(old: &Url, target: &Url) -> bool {
    old.origin() == target.origin() && target.username().is_empty() && target.password().is_none()
}

fn client_builder() -> ClientBuilder {
    Client::builder()
        .timeout(Duration::from_secs(15))
        // Match urllib's one-connection-per-request behavior.
        .pool_max_idle_per_host(0)
        // Retrying here, not inside reqwest, bounds the total number of attempts.
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let target = attempt.url();
            if attempt.previous().len() >= 10 {
                return attempt.error("Too many index redirects.");
            }
            if attempt
                .previous()
                .first()
                .is_none_or(|old| !safe_redirect(old, target))
            {
                return attempt.error("Index redirect to another origin is not allowed.");
            }
            attempt.follow()
        }))
}

#[derive(Debug)]
enum ReadError {
    Http(u16),
    Network,
}

pub struct ReleaseClient {
    no_cache: bool,
    http: Client,
    cache: HashMap<CacheKey, Vec<Release>>,
    links: HashMap<CacheKey, String>,
    notes: HashMap<String, String>,
}

impl ReleaseClient {
    pub fn new(no_cache: bool) -> Result<Self> {
        Ok(Self {
            no_cache,
            http: client_builder()
                .build()
                .map_err(|_| anyhow!("Could not initialize metadata HTTP client."))?,
            cache: HashMap::new(),
            links: HashMap::new(),
            notes: HashMap::new(),
        })
    }

    fn read(&self, request: Request) -> std::result::Result<(Vec<u8>, String), ReadError> {
        for attempt in 0..3 {
            let result = match self.http.execute(
                request
                    .try_clone()
                    .expect("metadata GET request is cloneable"),
            ) {
                Ok(response) => {
                    let status = response.status();
                    if !status.is_success() {
                        Err(ReadError::Http(status.as_u16()))
                    } else {
                        let content_type = response
                            .headers()
                            .get(CONTENT_TYPE)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or("")
                            .to_owned();
                        response
                            .bytes()
                            .map(|body| (body.to_vec(), content_type))
                            .map_err(|_| ReadError::Network)
                    }
                }
                Err(_) => Err(ReadError::Network),
            };
            let retry = matches!(
                result,
                Err(ReadError::Network | ReadError::Http(408 | 429 | 500 | 502 | 503 | 504))
            );
            if !retry || attempt == 2 {
                return result;
            }
            thread::sleep(Duration::from_millis(250 * (attempt + 1)));
        }
        unreachable!("bounded metadata retry loop")
    }

    fn private(&self, name: &str, indexes: &[Index]) -> Result<Option<(Vec<Release>, String)>> {
        for index in indexes {
            if index.explicit {
                continue;
            }
            let (request, link) = index.request(name, &self.http)?;
            let (body, content_type) = match self.read(request) {
                Ok(response) => response,
                Err(ReadError::Http(404)) => continue,
                Err(ReadError::Http(code)) => {
                    bail!("Index lookup failed for {name} (HTTP {code}).")
                }
                Err(ReadError::Network) => bail!(
                    "Index lookup failed for {name}. Check connectivity and index credentials."
                ),
            };
            let releases = simple_metadata(&body, &content_type, name)
                .map_err(|_| anyhow!("Invalid Simple index metadata for {name}."))?;
            return Ok(Some((releases, link)));
        }
        Ok(None)
    }

    fn public(&mut self, name: &str) -> Result<Vec<Release>> {
        self.public_from(
            name,
            &format!("https://pypi.org/pypi/{name}/json"),
            cache_path(name),
        )
    }

    fn public_from(
        &mut self,
        name: &str,
        url: &str,
        path: Option<PathBuf>,
    ) -> Result<Vec<Release>> {
        if !self.no_cache
            && let Some((releases, note)) = path.as_ref().and_then(|p| load_cache(p, now()))
        {
            if let Some(note) = note {
                self.notes.insert(name.into(), note);
            }
            return Ok(releases);
        }
        let request = self
            .http
            .get(url)
            .build()
            .map_err(|_| anyhow!("Invalid public PyPI URL."))?;
        let (body, _) = match self.read(request) {
            Ok(response) => response,
            Err(ReadError::Http(404)) => bail!(
                "Package {name} was not found on public PyPI (HTTP 404). Check the package name; it may be private or unpublished. Use --index or configure tool.uv.index for private packages. For uv workspace dependencies, declare workspace = true in [tool.uv.sources] and use -w to include workspace root sources and check members. No files were written."
            ),
            Err(ReadError::Http(code)) => bail!("PyPI lookup failed for {name} (HTTP {code})."),
            Err(ReadError::Network) => {
                bail!("PyPI response could not be read for {name}. No files were written.")
            }
        };
        let data: Value = serde_json::from_slice(&body)
            .map_err(|_| anyhow!("Invalid PyPI release metadata for {name}."))?;
        let (releases, note) = public_releases(&data, name)?;
        if let Some(note) = &note {
            self.notes.insert(name.into(), note.clone());
        }
        if !self.no_cache
            && let Some(path) = path
        {
            let rows: Vec<Value> = releases
                .iter()
                .map(|r| json!([r.version.to_string(), r.requires_python]))
                .collect();
            let payload = json!({"time": now(), "releases": rows, "project_urls": note.map(|n| json!({"Releases": n})).unwrap_or(json!({}))});
            // Cache writes are disposable; failures must not fail a lookup.
            if path
                .parent()
                .is_some_and(|parent| fs::create_dir_all(parent).is_ok())
            {
                let _ = fs::write(path, payload.to_string());
            }
        }
        Ok(releases)
    }

    pub fn get(&mut self, name: &str, indexes: &[Index]) -> Result<Vec<Release>> {
        let name = normalize(name);
        let key = (name.clone(), indexes.to_vec());
        if let Some(releases) = self.cache.get(&key) {
            return Ok(releases.clone());
        }
        let (releases, link) = if let Some(found) = self.private(&name, indexes)? {
            found
        } else {
            if indexes.iter().any(|index| index.default) {
                bail!("Package {name} was not found on configured indexes. No files were written.");
            }
            (
                self.public(&name)?,
                format!("https://pypi.org/project/{name}/"),
            )
        };
        self.links.insert(key.clone(), link);
        self.cache.insert(key, releases.clone());
        Ok(releases)
    }

    pub fn release_link(&self, name: &str, indexes: &[Index]) -> Option<String> {
        self.links
            .get(&(normalize(name), indexes.to_vec()))
            .cloned()
    }

    pub fn project_link(&self, name: &str) -> Option<String> {
        self.notes.get(&normalize(name)).cloned()
    }
}

fn stable(value: &str) -> Option<Version> {
    let version: Version = value.parse().ok()?;
    (!version.is_pre() && !version.is_dev()).then_some(version)
}

fn distribution(filename: &str) -> Option<(String, Version)> {
    let (name, version) = if let Some(stem) = filename.strip_suffix(".whl") {
        let parts: Vec<&str> = stem.split('-').collect();
        if !matches!(parts.len(), 5 | 6) {
            return None;
        }
        let name = parts[0];
        if name.is_empty()
            || name.contains("__")
            || !name
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.'))
        {
            return None;
        }
        if parts.len() == 6 && !parts[2].as_bytes().first().is_some_and(u8::is_ascii_digit) {
            return None;
        }
        // Empty wheel tag components cannot identify a distribution.
        if parts[parts.len() - 3..]
            .iter()
            .any(|tag| tag.split('.').any(str::is_empty))
        {
            return None;
        }
        (name, parts[1])
    } else {
        let stem = filename
            .strip_suffix(".tar.gz")
            .or_else(|| filename.strip_suffix(".zip"))?;
        let (name, version) = stem.rsplit_once('-')?;
        if name.is_empty() {
            return None;
        }
        (name, version)
    };
    Some((normalize(name), stable(version)?))
}

fn python_requirement(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(s)) => Some(Some(s.clone())),
        _ => None,
    }
}

fn simple_releases(files: &Value, name: &str) -> Result<Vec<Release>> {
    let files = files
        .as_array()
        .filter(|files| files.iter().all(Value::is_object))
        .ok_or_else(|| anyhow!("Invalid file metadata."))?;
    let mut releases = Vec::new();
    for file in files {
        // PEP 691 yanked is false or a reason string. Even an empty reason is yanked.
        if file
            .get("yanked")
            .is_some_and(|yanked| yanked != &Value::Bool(false))
        {
            continue;
        }
        let Some(filename) = file.get("filename").and_then(Value::as_str) else {
            continue;
        };
        let Some((package, version)) = distribution(filename) else {
            continue;
        };
        if package == name
            && let Some(requires_python) = python_requirement(file.get("requires-python"))
        {
            releases.push(Release {
                version,
                requires_python,
            });
        }
    }
    Ok(releases)
}

fn simple_metadata(body: &[u8], content_type: &str, name: &str) -> Result<Vec<Release>> {
    if content_type.to_ascii_lowercase().contains("json") {
        let data: Value = serde_json::from_slice(body)?;
        simple_releases(
            data.get("files").ok_or_else(|| anyhow!("Missing files."))?,
            name,
        )
    } else {
        let html = Html::parse_document(std::str::from_utf8(body)?);
        static ANCHORS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());
        let files: Vec<Value> = html.select(&ANCHORS).filter_map(|anchor| {
            let href = anchor.value().attr("href")?;
            if href.is_empty() { return None; }
            let path = href.split('#').next()?.split('?').next()?;
            let filename = percent_decode_str(path.rsplit('/').next()?).decode_utf8_lossy();
            Some(json!({"filename": filename, "requires-python": anchor.value().attr("data-requires-python"), "yanked": anchor.value().attr("data-yanked").is_some()}))
        }).collect();
        simple_releases(&Value::Array(files), name)
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn public_releases(data: &Value, name: &str) -> Result<Metadata> {
    let entries = data
        .get("releases")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("Invalid PyPI release metadata for {name}."))?;
    let note = release_notes(data.get("info").and_then(|info| info.get("project_urls")));
    let mut releases = Vec::new();
    for (value, files) in entries {
        let Some(version) = stable(value) else {
            continue;
        };
        let files = files
            .as_array()
            .filter(|files| files.iter().all(Value::is_object))
            .ok_or_else(|| anyhow!("Invalid PyPI file metadata for {name}."))?;
        for file in files {
            if !file.get("yanked").is_some_and(truthy)
                && let Some(requires_python) = python_requirement(file.get("requires_python"))
            {
                releases.push(Release {
                    version: version.clone(),
                    requires_python,
                });
            }
        }
    }
    Ok((releases, note))
}

fn release_notes(urls: Option<&Value>) -> Option<String> {
    for (key, value) in urls?.as_object()? {
        if !matches!(
            key.to_lowercase().as_str(),
            "changelog" | "release notes" | "releases"
        ) {
            continue;
        }
        let Some(value) = value.as_str() else {
            continue;
        };
        if !printable_url(value) || value.contains('\\') {
            continue;
        }
        let Ok(parsed) = Url::parse(value) else {
            continue;
        };
        if matches!(parsed.scheme(), "https" | "http")
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none_or(str::is_empty)
        {
            return Some(value.to_owned());
        }
    }
    None
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn cache_path_from(name: &str, xdg: Option<&OsStr>, home: Option<PathBuf>) -> Option<PathBuf> {
    let root = xdg
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|home| home.join(".cache"))
        })?;
    let digest = format!("{:x}", Sha256::digest(normalize(name).as_bytes()));
    Some(root.join("check-updates").join(format!("v1-{digest}.json")))
}

fn cache_path(name: &str) -> Option<PathBuf> {
    #[allow(deprecated)]
    let home = std::env::home_dir();
    cache_path_from(name, std::env::var_os("XDG_CACHE_HOME").as_deref(), home)
}

fn load_cache(path: &PathBuf, now: f64) -> Option<Metadata> {
    let data: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let age = now - data.get("time")?.as_f64()?;
    if !(0.0..CACHE_TTL).contains(&age) {
        return None;
    }
    let mut releases = Vec::new();
    for row in data.get("releases")?.as_array()? {
        let row = row.as_array()?;
        if row.len() != 2 {
            return None;
        }
        let version = row[0].as_str()?.parse().ok()?;
        let requires_python = python_requirement(Some(&row[1]))?;
        releases.push(Release {
            version,
            requires_python,
        });
    }
    Some((releases, release_notes(data.get("project_urls"))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread::JoinHandle;

    // Local TCP fixtures keep HTTP, redirect, and truncated-body tests offline.
    struct Server {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }

    impl Server {
        fn new(responses: impl FnOnce(&str) -> Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let mut responses = VecDeque::from(responses(&url));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let seen = requests.clone();
            let stopped = stop.clone();
            let worker = thread::spawn(move || {
                while !stopped.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(stream) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("test accept: {error}"),
                    };
                    // Accepted sockets can inherit O_NONBLOCK on macOS.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 1024];
                    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = stream.read(&mut buffer).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                    }
                    if bytes.is_empty() {
                        continue;
                    }
                    seen.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&bytes).into_owned());
                    let response = responses
                        .pop_front()
                        .unwrap_or_else(|| response(500, "text/plain", "unexpected request"));
                    let _ = stream.write_all(response.as_bytes());
                    // Send FIN after the response, then drain the peer before closing.
                    // An immediate close can send RST and discard the response on macOS.
                    let _ = stream.shutdown(std::net::Shutdown::Write);
                    while stream.read(&mut buffer).unwrap_or(0) != 0 {}
                }
            });
            Self {
                url,
                requests,
                stop,
                worker: Some(worker),
            }
        }

        fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn response(code: u16, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {code} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn client(no_cache: bool) -> ReleaseClient {
        let mut client = ReleaseClient::new(no_cache).unwrap();
        client.http = client_builder().no_proxy().build().unwrap();
        client
    }

    fn index(url: &str) -> Index {
        Index {
            url: url.into(),
            default: true,
            ..Index::default()
        }
    }
    fn release(version: &str, python: Option<&str>) -> Release {
        Release {
            version: version.parse().unwrap(),
            requires_python: python.map(str::to_owned),
        }
    }

    #[test]
    fn simple_json_and_html_contracts() {
        let files = json!([
            {"filename": "demo-1.0.tar.gz", "requires-python": ">=3.10"},
            {"filename": "demo-1.9-py3-none-any.whl", "requires-python": ">=3.12"},
            {"filename": "demo-2.0.zip", "yanked": true},
            {"filename": "demo-2.1.tar.gz", "yanked": ""},
            {"filename": "demo-3.0rc1.tar.gz"}, {"filename": "demo-4.0.dev1.tar.gz"},
            {"filename": "other-5.0.tar.gz"}, {"filename": "junk"}
        ]);
        let expected = vec![release("1", Some(">=3.10")), release("1.9", Some(">=3.12"))];
        assert_eq!(simple_releases(&files, "demo").unwrap(), expected);
        let html = br#"<a href="../demo-1.0.tar.gz#hash" data-requires-python="&gt;=3.10">a</a>
            <a href="demo-1%2E9-py3-none-any.whl" data-requires-python="&gt;=3.12">b</a>
            <a href="demo-2.zip" data-yanked="">c</a><a href="other-7.zip">d</a>
            <a href="demo-4.dev1.tar.gz">e</a><a href="">f</a>"#;
        assert_eq!(
            simple_metadata(html, "text/html", "demo").unwrap(),
            expected
        );
        let server = Server::new(|_| {
            vec![response(
                200,
                "APPLICATION/JSON",
                &json!({"files": files}).to_string(),
            )]
        });
        assert_eq!(
            client(false).get("Demo", &[index(&server.url)]).unwrap(),
            expected
        );
    }

    #[test]
    fn metadata_validation_and_file_filtering() {
        for payload in [
            "[]",
            r#"{"files":null}"#,
            r#"{"files":["bad"]}"#,
            "not JSON",
        ] {
            assert!(simple_metadata(payload.as_bytes(), "application/json", "demo").is_err());
        }
        let files = json!([
            {"filename": "demo-1.zip", "yanked": null}, {"filename": "demo-2.zip", "yanked": 0},
            {"filename": "demo-3.zip", "requires-python": 12}, {"filename": 17},
            {"filename": "demo-4.0+local-py3-none-any.whl"},
            {"filename": "demo-5.post1.tar.gz"}, {"filename": "demo-6-1build-py3-none-any.whl"}
        ]);
        assert_eq!(
            simple_releases(&files, "demo").unwrap(),
            vec![
                release("4+local", None),
                release("5.post1", None),
                release("6", None)
            ]
        );
        for filename in [
            "demo.whl",
            "demo-x.tar.gz",
            "demo-1.exe",
            "demo-1-abc-py3-none-any.whl",
            "de__mo-1-py3-none-any.whl",
            "demo-1--none-any.whl",
        ] {
            assert!(distribution(filename).is_none(), "{filename}");
        }
        assert_eq!(distribution("Demo_Pkg-1.zip").unwrap().0, "demo-pkg");
    }

    #[test]
    fn public_metadata_contracts() {
        let data = json!({"releases": {
            "1": [{"requires_python": ">=3.10"}, {"yanked": ""}, {"yanked": true}, {"requires_python": 3}],
            "2rc1": null, "3.dev1": [{}], "bad": null, "4": [], "5.post1": [{"yanked": 0}]
        }, "info": {"project_urls": {"Changelog": "https://example.org/releases"}}});
        let (found, note) = public_releases(&data, "demo").unwrap();
        assert_eq!(
            found,
            vec![
                release("1", Some(">=3.10")),
                release("1", None),
                release("5.post1", None)
            ]
        );
        assert_eq!(note.as_deref(), Some("https://example.org/releases"));
        for data in [
            json!([]),
            json!({"releases": []}),
            json!({"releases": {"1": null}}),
            json!({"releases": {"1": [4]}}),
        ] {
            assert!(
                public_releases(&data, "demo")
                    .unwrap_err()
                    .to_string()
                    .starts_with("Invalid PyPI")
            );
        }
    }

    #[test]
    fn first_match_empty_result_and_memory_cache() {
        let first = Server::new(|_| vec![response(200, "application/json", r#"{"files":[]}"#)]);
        let second = Server::new(|_| vec![]);
        let indexes = [index(&first.url), index(&second.url)];
        let mut client = client(true);
        assert_eq!(client.get("Demo_Pkg", &indexes).unwrap(), vec![]);
        assert_eq!(client.get("demo-pkg", &indexes).unwrap(), vec![]);
        assert_eq!(first.count(), 1);
        assert_eq!(second.count(), 0);
        assert_eq!(
            client.release_link("DEMO.pkg", &indexes),
            Some(format!("{}/demo-pkg/", first.url))
        );
        assert_eq!(client.project_link("demo-pkg"), None);
        assert_eq!(client.release_link("demo-pkg", &[]), None);
    }

    #[test]
    fn not_found_only_moves_to_next_index() {
        let first = Server::new(|_| vec![response(404, "text/plain", "missing")]);
        let second = Server::new(|_| {
            vec![response(
                200,
                "application/json",
                r#"{"files":[{"filename":"demo-2.zip"}]}"#,
            )]
        });
        let mut client = client(true);
        let indexes = [index(&first.url), index(&second.url)];
        assert_eq!(
            client.get("demo", &indexes).unwrap(),
            vec![release("2", None)]
        );
        assert_eq!(first.count(), 1);
        assert_eq!(second.count(), 1);
        let explicit = Index {
            explicit: true,
            ..index("not even a URL")
        };
        let error = client.get("other", &[explicit]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not found on configured indexes")
        );
    }

    #[test]
    fn all_404_and_explicit_default_block_public() {
        let first = Server::new(|_| vec![response(404, "text/plain", "missing")]);
        let indexes = [
            index(&first.url),
            Index {
                explicit: true,
                ..index("https://unused.invalid")
            },
        ];
        assert!(
            client(true)
                .get("demo", &indexes)
                .unwrap_err()
                .to_string()
                .contains("configured indexes")
        );
        assert_eq!(first.count(), 1);
    }

    #[test]
    fn cache_key_includes_index_configuration() {
        let first = Server::new(|_| {
            vec![response(
                200,
                "application/json",
                r#"{"files":[{"filename":"demo-1.zip"}]}"#,
            )]
        });
        let second = Server::new(|_| {
            vec![response(
                200,
                "application/json",
                r#"{"files":[{"filename":"demo-2.zip"}]}"#,
            )]
        });
        let mut client = client(true);
        assert_eq!(
            client.get("demo", &[index(&first.url)]).unwrap(),
            vec![release("1", None)]
        );
        assert_eq!(
            client.get("demo", &[index(&second.url)]).unwrap(),
            vec![release("2", None)]
        );
    }

    #[test]
    fn unsafe_index_urls_fail_without_request_and_without_secrets() {
        let client = client(true);
        for url in [
            "http://user:secret@example.org/simple",
            "file:///tmp/simple",
            "https:example.org/simple",
            "https:////example.org/simple",
            "https://example.org/simple?token=secret",
            "https://example.org/simple#secret",
            "https://example.org/\x1b[31m",
            "https://example.org/white space",
            "https://example.org/\u{200b}",
            "https://user:secret@example.org:0/simple",
            "https://[::1]:0/simple",
            "https://example.org:65536",
            "https://example.org\\@other.org",
        ] {
            let error = index(url).request("demo", &client.http).unwrap_err();
            assert_eq!(format!("{error:?}"), INVALID_INDEX, "{url}");
        }
    }

    #[test]
    fn credential_decoding_policy_and_environment_overrides() {
        let client = client(true);
        let mut source = index("https://user%40example:secret%3Avalue@example.org/simple");
        let (request, link) = source.request("Demo_Pkg", &client.http).unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://example.org/simple/demo-pkg/"
        );
        assert_eq!(link, "https://example.org/simple/demo-pkg/");
        assert_eq!(
            request.headers()["authorization"],
            "Basic dXNlckBleGFtcGxlOnNlY3JldDp2YWx1ZQ=="
        );
        for name in ["internal.proxy", "internal-proxy", "internal.proxy-cache_2"] {
            source.name = Some(name.into());
            let expected = if name.ends_with('2') {
                "UV_INDEX_INTERNAL_PROXY_CACHE_2"
            } else {
                "UV_INDEX_INTERNAL_PROXY"
            };
            let parsed = Url::parse(&source.url).unwrap();
            let credentials = source
                .credentials(&parsed, |key| {
                    if key == format!("{expected}_USERNAME") {
                        Some("env-user".into())
                    } else if key == format!("{expected}_PASSWORD") {
                        Some("env-password".into())
                    } else {
                        panic!("unexpected credential key: {key}");
                    }
                })
                .unwrap();
            assert_eq!(credentials, ("env-user".into(), "env-password".into()));
        }
        for policy in ["never", "invalid"] {
            source.authenticate = policy.into();
            assert!(source.request("demo", &client.http).is_err());
        }
        let mut source = index("https://example.org/simple");
        source.authenticate = "always".into();
        assert!(source.request("demo", &client.http).is_err());
        source.name = Some("required".into());
        assert_eq!(
            source
                .credentials(&Url::parse(&source.url).unwrap(), |_| Some("env".into()))
                .unwrap(),
            ("env".into(), "env".into())
        );
        source.authenticate = "never".into();
        assert!(
            source
                .credentials(&Url::parse(&source.url).unwrap(), |_| Some("secret".into()))
                .is_err()
        );
        assert!(
            !source
                .request("demo", &client.http)
                .unwrap()
                .0
                .headers()
                .contains_key("authorization")
        );
        source.url = "https://@example.org/simple".into();
        assert!(source.request("demo", &client.http).is_err());
        assert_eq!(Index::default().authenticate, "auto");
    }

    #[test]
    fn display_links_preserve_ports_and_ipv6_without_credentials() {
        let client = client(true);
        for host in ["packages.example", "[::1]"] {
            for port in [1, 443, 8443, 65535] {
                let url = format!("https://user:secret@{host}:{port}/simple");
                let (request, link) = index(&url).request("demo", &client.http).unwrap();
                assert_eq!(link, format!("https://{host}:{port}/simple/demo/"));
                assert!(!request.url().as_str().contains("secret"));
            }
        }
    }

    #[test]
    fn private_http_and_metadata_failures_are_sanitized() {
        for code in [401, 403, 429, 500] {
            let count = if matches!(code, 429 | 500) { 3 } else { 1 };
            let server = Server::new(|_| {
                (0..count)
                    .map(|_| response(code, "text/plain", "response-secret"))
                    .collect()
            });
            let unused = Server::new(|_| vec![]);
            let error = client(true)
                .get("demo", &[index(&server.url), index(&unused.url)])
                .unwrap_err();
            assert_eq!(
                format!("{error:?}"),
                format!("Index lookup failed for demo (HTTP {code}).")
            );
            assert_eq!(server.count(), count);
            assert_eq!(unused.count(), 0);
        }
        let server = Server::new(|_| {
            vec![response(
                200,
                "application/json",
                r#"{"secret":"response-secret"}"#,
            )]
        });
        let error = client(true).get("demo", &[index(&server.url)]).unwrap_err();
        assert_eq!(
            format!("{error:?}"),
            "Invalid Simple index metadata for demo."
        );
    }

    #[test]
    fn transient_status_and_truncated_body_retry() {
        for code in [408, 429, 500, 502, 503, 504] {
            let server = Server::new(|_| {
                vec![
                    response(code, "text/plain", "retry"),
                    response(code, "text/plain", "retry"),
                    response(200, "application/json", r#"{"files":[]}"#),
                ]
            });
            let found = client(true)
                .get("demo", &[index(&server.url)])
                .unwrap_or_else(|error| {
                    panic!(
                        "HTTP {code}: {error:?}; requests: {:?}",
                        server.requests.lock().unwrap()
                    );
                });
            assert_eq!(found, vec![]);
            assert_eq!(server.count(), 3);
        }
        let truncated =
            "HTTP/1.1 200 OK\r\nContent-Length: 999\r\nConnection: close\r\n\r\nresponse-secret";
        let server = Server::new(|_| {
            vec![
                truncated.into(),
                truncated.into(),
                response(200, "application/json", r#"{"files":[]}"#),
            ]
        });
        assert!(
            client(true)
                .get("demo", &[index(&server.url)])
                .unwrap()
                .is_empty()
        );
        assert_eq!(server.count(), 3);
        let server = Server::new(|_| vec![truncated.into(); 3]);
        let error = client(true).get("demo", &[index(&server.url)]).unwrap_err();
        assert_eq!(
            format!("{error:?}"),
            "Index lookup failed for demo. Check connectivity and index credentials."
        );
        assert_eq!(server.count(), 3);
    }

    #[test]
    fn same_origin_redirect_follows_but_cross_origin_never_connects() {
        let server = Server::new(|base| {
            vec![
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {base}/new/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                response(200, "application/json", r#"{"files":[]}"#),
            ]
        });
        let mut client = client(true);
        assert!(
            client
                .get("demo", &[index(&server.url)])
                .unwrap()
                .is_empty()
        );
        assert_eq!(server.count(), 2);
        assert!(server.requests.lock().unwrap()[1].starts_with("GET /new/ HTTP/1.1"));
        // The release link is the initial index URL, not the redirect destination.
        assert_eq!(
            client.release_link("demo", &[index(&server.url)]),
            Some(format!("{}/demo/", server.url))
        );
        let target = Server::new(|_| vec![]);
        let server = Server::new(|_| {
            vec![
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {}/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    target.url
                );
                3
            ]
        });
        assert!(client.get("other", &[index(&server.url)]).is_err());
        assert_eq!(server.count(), 3);
        assert_eq!(target.count(), 0);
    }

    #[test]
    fn redirect_policy_rejects_downgrades_and_credentials_and_keeps_same_origin_auth() {
        let old = Url::parse("https://example.org/simple/").unwrap();
        for destination in [
            "https://other.org/simple/",
            "http://example.org/simple/",
            "https://example.org:8443/simple/",
            "https://user:secret@example.org/simple/",
        ] {
            assert!(!safe_redirect(&old, &Url::parse(destination).unwrap()));
        }
        assert!(safe_redirect(
            &old,
            &Url::parse("https://example.org/new/").unwrap()
        ));
        let server = Server::new(|base| {
            vec![
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {base}/new/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                response(200, "text/plain", "done"),
            ]
        });
        let client = client(true);
        // Test header retention separately; Index itself forbids credentials over HTTP.
        let request = client
            .http
            .get(&server.url)
            .basic_auth("user", Some("secret"))
            .build()
            .unwrap();
        assert_eq!(client.read(request).unwrap().0, b"done");
        assert_eq!(server.count(), 2);
        for request in server.requests.lock().unwrap().iter() {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: basic dxnlcjpzzwnyzxq=")
            );
        }
    }

    #[test]
    fn request_timeout_retries_are_bounded() {
        // The kernel accepts connections, but no server sends response headers.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = client(true);
        client.http = client_builder()
            .no_proxy()
            .timeout(Duration::from_millis(30))
            .build()
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let start = std::time::Instant::now();
        let error = client.get("demo", &[index(&url)]).unwrap_err();
        assert_eq!(
            format!("{error:?}"),
            "Index lookup failed for demo. Check connectivity and index credentials."
        );
        assert!(start.elapsed() >= Duration::from_millis(750));
        listener.set_nonblocking(true).unwrap();
        let mut attempts = 0;
        while listener.accept().is_ok() {
            attempts += 1;
        }
        assert_eq!(attempts, 3);
    }

    #[test]
    fn safe_release_note_links_only() {
        for value in [
            "https://user:secret@example.org/notes",
            "javascript:alert(1)",
            "https://example.org/notes?token=secret",
            "https://example.org/\x1b[31m",
            "https://example.org/\u{200b}",
            "https://example.org/a b",
            "https://example.org\\@other.org",
        ] {
            assert_eq!(
                release_notes(Some(&json!({"Changelog": value}))),
                None,
                "{value}"
            );
        }
        assert_eq!(
            release_notes(Some(
                &json!({"Homepage": "https://example.org", "RELEASE NOTES": "https://example.org/notes#v1"})
            )),
            Some("https://example.org/notes#v1".into())
        );
        assert_eq!(
            release_notes(Some(
                &json!({"Changelog": 42, "Releases": "http://example.org"})
            )),
            Some("http://example.org".into())
        );
        assert_eq!(release_notes(Some(&json!([]))), None);
    }

    #[test]
    fn cache_path_is_absolute_normalized_and_hashed() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        for setting in [None, Some(""), Some("relative-cache"), Some(".")] {
            let path =
                cache_path_from("Demo_Pkg", setting.map(OsStr::new), Some(home.clone())).unwrap();
            assert_eq!(path.parent().unwrap(), home.join(".cache/check-updates"));
            assert_eq!(
                Some(path.clone()),
                cache_path_from("demo-pkg", setting.map(OsStr::new), Some(home.clone()))
            );
            assert!(path.is_absolute());
            assert!(
                path.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("v1-")
            );
            assert_eq!(path.file_name().unwrap().len(), 72);
        }
        let configured = temp.path().join("configured");
        assert_eq!(
            cache_path_from("demo", Some(configured.as_os_str()), None)
                .unwrap()
                .parent()
                .unwrap(),
            configured.join("check-updates")
        );
        assert!(cache_path_from("demo", Some(OsStr::new("relative")), None).is_none());
    }

    #[test]
    fn cache_validation_ttl_future_and_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let data = json!({"time": 10000, "releases": [["2", ">=3.10"]], "project_urls": {"Releases": "https://example.org/notes"}});
        fs::write(&path, data.to_string()).unwrap();
        assert_eq!(
            load_cache(&path, 10000.0).unwrap().0,
            vec![release("2", Some(">=3.10"))]
        );
        assert!(load_cache(&path, 13599.0).is_some());
        assert!(load_cache(&path, 13600.0).is_none());
        assert!(load_cache(&path, 9999.0).is_none());
        for data in [
            json!([]),
            json!({"time":10000, "releases":[["bad", null]]}),
            json!({"time":10000, "releases":[["1", false]]}),
            json!({"time":10000, "releases":[["1"]]}),
            json!({"time":"10000", "releases":[]}),
        ] {
            fs::write(&path, data.to_string()).unwrap();
            assert!(load_cache(&path, 10000.0).is_none());
        }
        fs::write(&path, "broken").unwrap();
        assert!(load_cache(&path, 10000.0).is_none());
    }

    #[test]
    fn public_disk_cache_hit_expiry_corruption_bypass_and_write_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache/demo.json");
        let data = r#"{"releases":{"2":[{"requires_python":">=3.10"}]},"info":{"project_urls":{"Changelog":"https://example.org/notes"}}}"#;
        let server = Server::new(|_| vec![response(200, "application/json", data); 5]);
        let expected = vec![release("2", Some(">=3.10"))];
        assert_eq!(
            client(false)
                .public_from("demo", &server.url, Some(path.clone()))
                .unwrap(),
            expected
        );
        let mut cached = client(false);
        assert_eq!(
            cached
                .public_from("demo", &server.url, Some(path.clone()))
                .unwrap(),
            expected
        );
        assert_eq!(
            cached.project_link("Demo"),
            Some("https://example.org/notes".into())
        );
        assert_eq!(server.count(), 1);
        let mut payload: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        payload["time"] = json!(now() - CACHE_TTL - 1.0);
        fs::write(&path, payload.to_string()).unwrap();
        assert_eq!(
            client(false)
                .public_from("demo", &server.url, Some(path.clone()))
                .unwrap(),
            expected
        );
        assert_eq!(server.count(), 2);
        fs::write(&path, "broken").unwrap();
        assert_eq!(
            client(false)
                .public_from("demo", &server.url, Some(path.clone()))
                .unwrap(),
            expected
        );
        assert_eq!(server.count(), 3);
        let before = fs::read(&path).unwrap();
        assert_eq!(
            client(true)
                .public_from("demo", &server.url, Some(path.clone()))
                .unwrap(),
            expected
        );
        assert_eq!(server.count(), 4);
        assert_eq!(fs::read(&path).unwrap(), before);
        let impossible = path.join("child.json");
        assert_eq!(
            client(false)
                .public_from("demo", &server.url, Some(impossible))
                .unwrap(),
            expected
        );
        assert_eq!(server.count(), 5);
    }

    #[test]
    fn public_errors_are_actionable_and_sanitized() {
        let server = Server::new(|_| vec![response(404, "text/plain", "response-secret")]);
        let error = client(true)
            .public_from("demo", &server.url, None)
            .unwrap_err();
        let text = format!("{error:?}");
        assert!(text.contains("private or unpublished"));
        assert!(!text.contains("response-secret"));
        let truncated =
            "HTTP/1.1 200 OK\r\nContent-Length: 999\r\nConnection: close\r\n\r\nresponse-secret";
        let server = Server::new(|_| vec![truncated.into(); 3]);
        let error = client(true)
            .public_from("demo", &server.url, None)
            .unwrap_err();
        assert_eq!(
            format!("{error:?}"),
            "PyPI response could not be read for demo. No files were written."
        );
        assert_eq!(server.count(), 3);
    }
}
