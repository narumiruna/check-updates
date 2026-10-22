#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct StalledIndex {
    url: String,
    requests: Arc<AtomicUsize>,
    received: mpsc::Receiver<()>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl StalledIndex {
    fn new(response: &'static [u8]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/simple/", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (send, received) = mpsc::channel();
        let count = requests.clone();
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            let mut connections: Vec<TcpStream> = Vec::new();
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut request = Vec::new();
                        let mut buffer = [0; 1024];
                        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                            match stream.read(&mut buffer) {
                                Ok(0) | Err(_) => break,
                                Ok(size) => request.extend_from_slice(&buffer[..size]),
                            }
                        }
                        if request.is_empty() {
                            continue;
                        }
                        count.fetch_add(1, Ordering::Relaxed);
                        stream.write_all(response).unwrap();
                        let _ = send.send(());
                        // Keep headers or the body incomplete until the test ends.
                        connections.push(stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("test listener failed: {error}"),
                }
            }
        });
        Self {
            url,
            requests,
            received,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for StalledIndex {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn sigint_cancels_stalled_headers_body_and_retries_without_writes() {
    for response in [
        b"".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 9999\r\n\r\nprivate-response-secret".as_slice(),
        b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\n\r\n".as_slice(),
    ] {
        for args in [vec!["--format", "json"], vec!["--upgrade"]] {
            let directory = tempfile::tempdir().unwrap();
            let manifest = directory.path().join("pyproject.toml");
            let original = "[project]\ndependencies=['demo>=1']\n";
            fs::write(&manifest, original).unwrap();
            fs::write(directory.path().join("uv.lock"), "original lock").unwrap();
            let bin = directory.path().join("bin");
            fs::create_dir(&bin).unwrap();
            let uv = bin.join("uv");
            fs::write(&uv, "#!/bin/sh\nprintf called > lock-called\n").unwrap();
            fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();
            let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))).unwrap();
            let index = StalledIndex::new(response);
            let mut child = ChildGuard(Some(Command::new(env!("CARGO_BIN_EXE_check-updates"))
                .current_dir(directory.path()).env("PATH", path)
                .args(["--index", &index.url, "--debug"]).args(&args)
                .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
                .spawn().unwrap()));
            index.received.recv_timeout(Duration::from_secs(5)).expect("metadata request did not start");
            // Let a complete 503 enter the retry delay before sending the signal.
            if response.starts_with(b"HTTP/1.1 503") { thread::sleep(Duration::from_millis(40)); }
            let started = Instant::now();
            assert!(Command::new("kill").args(["-INT", &child.0.as_ref().unwrap().id().to_string()]).status().unwrap().success());
            loop {
                if child.0.as_mut().unwrap().try_wait().unwrap().is_some() { break; }
                assert!(started.elapsed() < Duration::from_secs(2), "SIGINT did not cancel the metadata lookup within two seconds");
                thread::sleep(Duration::from_millis(10));
            }
            let output = child.0.take().unwrap().wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(130), "{}", String::from_utf8_lossy(&output.stderr));
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("Interrupted."));
            assert!(!stderr.contains("private-response-secret"));
            assert!(!stderr.contains(&index.url));
            assert_eq!(index.requests.load(Ordering::Relaxed), 1, "cancellation must not start another attempt");
            assert_eq!(fs::read_to_string(&manifest).unwrap(), original);
            assert_eq!(fs::read_to_string(directory.path().join("uv.lock")).unwrap(), "original lock");
            assert!(!directory.path().join("lock-called").exists());
        }
    }
}
