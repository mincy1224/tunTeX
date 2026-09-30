//! End-to-end tests for the client binary.
//!
//! Each test runs the *real* `tuntex.exe` against a minimal HTTP server
//! built from `std::net`, so the whole path is exercised: environment reading,
//! path mapping, archive creation, HTTP, and writeback.
//!
//! The server is deliberately dumb -- it answers with canned bytes -- which
//! makes the interesting assertions possible: what the client sent, and what it
//! did to the workspace when the answer was bad.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use tar::{Archive, Builder, Header};

const BINARY: &str = env!("CARGO_BIN_EXE_tuntex-client");
const PROTOCOL_VERSION: &str = "1";

// ---------------------------------------------------------------------------
// scratch directories
// ---------------------------------------------------------------------------

/// A throwaway directory tree under the crate, removed on drop.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        root.push(".runtime-tests");
        root.push(format!(
            "{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        std::fs::create_dir_all(root.join("temp")).unwrap();
        Self { root }
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    /// The child's `%TEMP%`, kept inside the crate so tests leave no trace.
    fn temp(&self) -> PathBuf {
        self.root.join("temp")
    }

    fn write(&self, relative: &str, content: &str) -> PathBuf {
        let target = self.workspace().join(to_native(relative));
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, content).unwrap();
        target
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.workspace().join(to_native(relative))).unwrap()
    }

    fn exists(&self, relative: &str) -> bool {
        self.workspace().join(to_native(relative)).exists()
    }

    fn workspace_string(&self) -> String {
        self.workspace().to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn to_native(relative: &str) -> PathBuf {
    PathBuf::from(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
}

// ---------------------------------------------------------------------------
// mock server
// ---------------------------------------------------------------------------

/// One request the mock server received.
#[derive(Debug, Clone)]
struct Received {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// What the server should answer with.
enum Reply {
    /// A successful gzip body, with the request id echoed or overridden.
    Ok {
        body: Vec<u8>,
        request_id: Option<String>,
    },
    /// An HTTP error with a JSON body.
    Status { code: u16, body: String },
}

type Handler = Box<dyn Fn(&Received) -> Reply + Send + Sync + 'static>;

struct MockServer {
    address: SocketAddr,
    received: Arc<std::sync::Mutex<Vec<Received>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockServer {
    fn start(handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();

        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_received = Arc::clone(&received);
        let thread_stop = Arc::clone(&stop);

        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                        if let Ok(Some(request)) = handle_connection(&mut stream) {
                            let reply = handler(&request);
                            thread_received.lock().unwrap().push(request);
                            let _ = write_reply(&mut stream, reply);
                        }
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            address,
            received,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Wait for the first request to arrive, or fail the test.
    fn wait_for_request(&self) -> Received {
        for _ in 0..600 {
            if let Some(request) = self.received.lock().unwrap().first() {
                return request.clone();
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("the client never contacted the mock server");
    }

    fn request_count(&self) -> usize {
        self.received.lock().unwrap().len()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(stream: &mut TcpStream) -> std::io::Result<Option<Received>> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end;

    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            header_end = position + 4;
            break;
        }
    }

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    if headers
        .get("expect")
        .map(|value| value.eq_ignore_ascii_case("100-continue"))
        .unwrap_or(false)
    {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    Ok(Some(Received {
        method,
        target,
        headers,
        body,
    }))
}

fn write_reply(stream: &mut TcpStream, reply: Reply) -> std::io::Result<()> {
    match reply {
        Reply::Ok { body, request_id } => {
            let mut head = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/gzip\r\n\
                 X-TunTeX-Protocol: {PROTOCOL_VERSION}\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n",
                body.len()
            );
            if let Some(id) = request_id {
                head.push_str(&format!("X-TunTeX-Request-Id: {id}\r\n"));
            }
            head.push_str("\r\n");
            stream.write_all(head.as_bytes())?;
            stream.write_all(&body)?;
        }
        Reply::Status { code, body } => {
            let head = format!(
                "HTTP/1.1 {code} Error\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(body.as_bytes())?;
        }
    }
    stream.flush()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// result archive construction
// ---------------------------------------------------------------------------

struct ResultArchive {
    request_id: String,
    exit_code: i32,
    timed_out: bool,
    cancelled: bool,
    duration_ms: u64,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    files: Vec<(String, Vec<u8>)>,
    declared_changed: Vec<String>,
    deleted: Vec<String>,
    protocol: u32,
}

impl ResultArchive {
    fn new(request_id: &str) -> Self {
        Self {
            request_id: request_id.to_string(),
            exit_code: 0,
            timed_out: false,
            cancelled: false,
            duration_ms: 5,
            stdout: Vec::new(),
            stderr: Vec::new(),
            files: Vec::new(),
            declared_changed: Vec::new(),
            deleted: Vec::new(),
            protocol: 1,
        }
    }

    fn exit_code(mut self, code: i32) -> Self {
        self.exit_code = code;
        self
    }

    fn stdout(mut self, content: &[u8]) -> Self {
        self.stdout = content.to_vec();
        self
    }

    fn stderr(mut self, content: &[u8]) -> Self {
        self.stderr = content.to_vec();
        self
    }

    fn file(mut self, relative: &str, content: &str) -> Self {
        self.files
            .push((relative.to_string(), content.as_bytes().to_vec()));
        self.declared_changed.push(relative.to_string());
        self
    }

    fn deleted(mut self, paths: &[&str]) -> Self {
        self.deleted = paths.iter().map(|value| value.to_string()).collect();
        self
    }

    fn protocol(mut self, version: u32) -> Self {
        self.protocol = version;
        self
    }

    fn build(&self) -> Vec<u8> {
        let metadata = serde_json::json!({
            "protocol": self.protocol,
            "request_id": self.request_id,
            "exit_code": self.exit_code,
            "timed_out": self.timed_out,
            "cancelled": self.cancelled,
            "duration_ms": self.duration_ms,
            "changed": self.declared_changed,
            "deleted": self.deleted,
        });

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut builder = Builder::new(&mut encoder);
            append(
                &mut builder,
                "meta/result.json",
                &serde_json::to_vec(&metadata).unwrap(),
            );
            append(&mut builder, "stdout.bin", &self.stdout);
            append(&mut builder, "stderr.bin", &self.stderr);
            for (relative, content) in &self.files {
                append(&mut builder, &format!("files/{relative}"), content);
            }
            builder.into_inner().unwrap();
        }
        encoder.finish().unwrap()
    }
}

fn append<W: Write>(builder: &mut Builder<W>, name: &str, payload: &[u8]) {
    let mut header = Header::new_gnu();
    header.set_size(payload.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, name, payload).unwrap();
}

/// Decode a request archive into its metadata and member names.
fn decode_request(body: &[u8]) -> (serde_json::Value, Vec<String>) {
    let mut archive = Archive::new(GzDecoder::new(body));
    let mut metadata = serde_json::Value::Null;
    let mut members = Vec::new();

    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut payload = Vec::new();
        entry.read_to_end(&mut payload).unwrap();
        if name == "meta/request.json" {
            metadata = serde_json::from_slice(&payload).unwrap();
        }
        members.push(name);
    }
    (metadata, members)
}

// ---------------------------------------------------------------------------
// running the client
// ---------------------------------------------------------------------------

struct ClientRun {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: i32,
}

impl ClientRun {
    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

fn run_client(scratch: &Scratch, url: &str, args: &[&str], env: &[(&str, &str)]) -> ClientRun {
    let engine = env
        .iter()
        .find_map(|(name, value)| (*name == "TUNTEX_ENGINE").then_some(*value));
    let executable = engine
        .map(|name| {
            let path = scratch.root.join(format!("{name}.exe"));
            if !path.exists() {
                std::fs::copy(BINARY, &path).unwrap();
            }
            std::fs::write(
                scratch.root.join("tun-tex-cfg.yaml"),
                format!("socket: {url}\ntex: {name}\n"),
            )
            .unwrap();
            path
        })
        .unwrap_or_else(|| PathBuf::from(BINARY));
    let mut command = Command::new(executable);
    for argument in args {
        command.arg(argument);
    }

    command
        .current_dir(scratch.workspace())
        .env("TUNTEX_URL", url)
        .env("TUNTEX_WORKSPACE", scratch.workspace_string())
        .env("TUNTEX_CWD", scratch.workspace_string())
        // Keep every temporary file inside the crate rather than the real %TEMP%.
        .env("TEMP", scratch.temp())
        .env("TMP", scratch.temp())
        // Never let the ambient environment leak into a test.
        .env_remove("TUNTEX_ENGINE")
        .env_remove("TUNTEX_TOKEN")
        .env_remove("TUNTEX_FORWARD_ENV")
        .env_remove("TUNTEX_DEBUG")
        .env_remove("TUNTEX_TIMEOUT");

    for (name, value) in env {
        if *name != "TUNTEX_ENGINE" {
            command.env(name, value);
        }
    }

    let output = command.output().expect("failed to run tuntex.exe");
    ClientRun {
        stdout: output.stdout,
        stderr: output.stderr,
        code: output.status.code().unwrap_or(-1),
    }
}

/// Run through a client configured for `latexmk`.
fn run_latexmk(scratch: &Scratch, url: &str, args: &[&str], env: &[(&str, &str)]) -> ClientRun {
    let mut all = vec![("TUNTEX_ENGINE", "latexmk")];
    all.extend_from_slice(env);
    run_client(scratch, url, args, &all)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn a_successful_compile_applies_products_and_forwards_output() {
    let scratch = Scratch::new("happy");
    scratch.write("main.tex", "original");
    let request_id = std::sync::Arc::new(std::sync::Mutex::new(String::new()));

    let captured = Arc::clone(&request_id);
    let server = MockServer::start(Box::new(move |request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        *captured.lock().unwrap() = id.clone();
        let body = ResultArchive::new(&id)
            .stdout(b"compiling main.tex\n")
            .stderr(b"warning: overfull hbox\n")
            .exit_code(0)
            .file("build/main.pdf", "%PDF-fake")
            .file("main.tex", "MODIFIED")
            .deleted(&["build/old.aux"])
            .build();
        Reply::Ok {
            body,
            request_id: Some(id),
        }
    }));

    scratch.write("build/old.aux", "stale");
    let run = run_latexmk(
        &scratch,
        &server.base_url(),
        &["-synctex=1", "-interaction=nonstopmode", "main.tex"],
        &[],
    );

    assert_eq!(run.code, 0, "stderr: {}", run.stderr_text());
    assert_eq!(
        run.stdout, b"compiling main.tex\n",
        "stdout must be byte-for-byte"
    );
    assert_eq!(run.stderr, b"warning: overfull hbox\n");
    assert_eq!(scratch.read("build/main.pdf"), "%PDF-fake");
    assert_eq!(scratch.read("main.tex"), "MODIFIED");
    assert!(
        !scratch.exists("build/old.aux"),
        "a deleted file must be removed"
    );
}

#[test]
fn a_non_zero_exit_code_is_returned_and_products_are_still_applied() {
    let scratch = Scratch::new("nonzero");
    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        let body = ResultArchive::new(&id)
            .exit_code(12)
            .stdout(b"! Undefined control sequence.\n")
            .file("main.log", "error log")
            .file("main.aux", "aux")
            .build();
        Reply::Ok {
            body,
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(&scratch, &server.base_url(), &["main.tex"], &[]);

    assert_eq!(
        run.code, 12,
        "the remote exit code must be returned unchanged"
    );
    assert!(run.stdout.starts_with(b"! Undefined control sequence."));
    assert_eq!(
        scratch.read("main.log"),
        "error log",
        "a failed build's files are the point"
    );
    assert_eq!(scratch.read("main.aux"), "aux");
}

#[test]
fn the_uploaded_request_describes_the_compile() {
    let scratch = Scratch::new("request-shape");
    scratch.write("main.tex", "\\documentclass{article}");
    scratch.write("chapters/intro.tex", "intro");

    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).build(),
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(
        &scratch,
        &server.base_url(),
        &["-synctex=1", "-outdir=build", "main.tex"],
        &[],
    );
    assert_eq!(run.code, 0, "stderr: {}", run.stderr_text());

    let request = server.wait_for_request();
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/compile");
    assert_eq!(request.header("content-type").unwrap(), "application/gzip");
    assert_eq!(
        request.header("x-tuntex-protocol").unwrap(),
        PROTOCOL_VERSION
    );
    assert!(request.header("x-tuntex-request-id").is_some());

    let (metadata, members) = decode_request(&request.body);
    assert_eq!(metadata["protocol"], 1);
    assert_eq!(metadata["engine"], "latexmk");
    assert_eq!(metadata["cwd"], "/workspace");
    assert_eq!(
        metadata["argv"],
        serde_json::json!(["-synctex=1", "-outdir=build", "main.tex"]),
        "relative paths and flags pass through untouched"
    );
    assert_eq!(metadata["request_id"].as_str().unwrap().len(), 36, "a UUID");

    assert!(members.contains(&"meta/request.json".to_string()));
    assert!(members.contains(&"workspace/main.tex".to_string()));
    assert!(members.contains(&"workspace/chapters/intro.tex".to_string()));
}

#[test]
fn windows_paths_in_arguments_are_mapped_to_the_virtual_root() {
    let scratch = Scratch::new("pathmap");
    scratch.write("main.tex", "x");
    let workspace = scratch.workspace();

    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).build(),
            request_id: Some(id),
        }
    }));

    let outdir = format!("-outdir={}", workspace.join("build").display());
    let document = format!("{}\\main.tex", workspace.display());
    // The entry file is passed as an absolute Windows path, as LaTeX Workshop does.
    let run = run_latexmk(&scratch, &server.base_url(), &[&outdir, &document], &[]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr_text());

    let (metadata, _) = decode_request(&server.wait_for_request().body);
    assert_eq!(
        metadata["argv"],
        serde_json::json!(["-outdir=/workspace/build", "/workspace/main.tex"]),
        "absolute Windows paths must become protocol paths"
    );
}

#[test]
fn the_request_id_is_unique_per_run() {
    let scratch = Scratch::new("unique-id");
    let ids = Arc::new(std::sync::Mutex::new(Vec::new()));

    let captured = Arc::clone(&ids);
    let server = MockServer::start(Box::new(move |request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        captured.lock().unwrap().push(id.clone());
        Reply::Ok {
            body: ResultArchive::new(&id).build(),
            request_id: Some(id),
        }
    }));

    run_latexmk(&scratch, &server.base_url(), &["main.tex"], &[]);
    run_latexmk(&scratch, &server.base_url(), &["main.tex"], &[]);

    let ids = ids.lock().unwrap();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "each run must use a fresh request id");
}

#[test]
fn a_response_for_a_different_request_is_refused_and_nothing_is_written() {
    let scratch = Scratch::new("mismatch");
    scratch.write("main.pdf", "the last good pdf");

    let server = MockServer::start(Box::new(|_request| {
        // The server answers about somebody else's job.
        let body = ResultArchive::new("11111111-1111-4111-8111-111111111111")
            .file("main.pdf", "CLOBBERED")
            .file("new.txt", "NEW")
            .build();
        Reply::Ok {
            body,
            request_id: None,
        }
    }));

    let run = run_latexmk(&scratch, &server.base_url(), &["main.tex"], &[]);

    assert_eq!(run.code, 74, "a mismatched result is a protocol error");
    assert!(run.stderr_text().contains("different request"));
    assert_eq!(scratch.read("main.pdf"), "the last good pdf");
    assert!(!scratch.exists("new.txt"));
}

#[test]
fn a_result_declaring_an_unsupported_protocol_is_refused() {
    let scratch = Scratch::new("protocol-mismatch");
    scratch.write("main.pdf", "the last good pdf");

    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        let body = ResultArchive::new(&id)
            .protocol(99)
            .file("main.pdf", "CLOBBERED")
            .build();
        Reply::Ok {
            body,
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(&scratch, &server.base_url(), &["main.tex"], &[]);

    assert_eq!(run.code, 74);
    assert!(run.stderr_text().contains("protocol"));
    assert_eq!(scratch.read("main.pdf"), "the last good pdf");
}

#[test]
fn an_http_server_error_leaves_the_workspace_untouched() {
    let scratch = Scratch::new("server-error");
    scratch.write("main.pdf", "the last good pdf");

    let server = MockServer::start(Box::new(|_request| Reply::Status {
        code: 500,
        body: r#"{"error":"internal_error","message":"the backend exploded"}"#.to_string(),
    }));

    let run = run_latexmk(&scratch, &server.base_url(), &["main.tex"], &[]);

    assert_eq!(
        run.code, 69,
        "a server-side failure is 'service unavailable'"
    );
    assert!(run.stderr_text().contains("the backend exploded"));
    assert_eq!(scratch.read("main.pdf"), "the last good pdf");
}

#[test]
fn a_rejected_request_reports_the_server_message() {
    let scratch = Scratch::new("rejected");
    let server = MockServer::start(Box::new(|_request| Reply::Status {
        code: 422,
        body: r#"{"error":"invalid_metadata","message":"unknown engine 'nope'"}"#.to_string(),
    }));

    let run = run_latexmk(
        &scratch,
        &server.base_url(),
        &["main.tex"],
        &[("TUNTEX_ENGINE", "nope")],
    );

    assert_eq!(run.code, 74);
    assert!(run.stderr_text().contains("unknown engine 'nope'"));
}

#[test]
fn an_authentication_failure_has_its_own_exit_code() {
    let scratch = Scratch::new("auth");
    let server = MockServer::start(Box::new(|_request| Reply::Status {
        code: 401,
        body: r#"{"error":"unauthorized","message":"a valid token is required"}"#.to_string(),
    }));

    let run = run_latexmk(&scratch, &server.base_url(), &["main.tex"], &[]);

    assert_eq!(run.code, 77);
    assert!(run.stderr_text().contains("TUNTEX_TOKEN"));
}

#[test]
fn the_bearer_token_is_sent_when_configured() {
    let scratch = Scratch::new("token");
    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).build(),
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(
        &scratch,
        &server.base_url(),
        &["main.tex"],
        &[("TUNTEX_TOKEN", "s3cret")],
    );
    assert_eq!(run.code, 0, "stderr: {}", run.stderr_text());

    let request = server.wait_for_request();
    assert_eq!(request.header("authorization").unwrap(), "Bearer s3cret");
}

#[test]
fn forwarded_variables_reach_the_request() {
    let scratch = Scratch::new("forward-env");
    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).build(),
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(
        &scratch,
        &server.base_url(),
        &["main.tex"],
        &[
            ("TUNTEX_FORWARD_ENV", "TEXINPUTS,TUNTEX_TEST_FORWARDED"),
            ("TEXINPUTS", "/workspace/tex//"),
            ("TUNTEX_TEST_FORWARDED", "hello"),
            ("TUNTEX_TEST_NOT_FORWARDED", "secret"),
        ],
    );
    assert_eq!(run.code, 0, "stderr: {}", run.stderr_text());

    let (metadata, _) = decode_request(&server.wait_for_request().body);
    let env = metadata["env"].as_object().unwrap();
    assert_eq!(env["TEXINPUTS"], "/workspace/tex//");
    assert_eq!(env["TUNTEX_TEST_FORWARDED"], "hello");
    assert!(
        !env.contains_key("TUNTEX_TEST_NOT_FORWARDED"),
        "only names listed in TUNTEX_FORWARD_ENV may be forwarded"
    );
}

#[test]
fn a_path_outside_the_workspace_is_refused_before_any_request() {
    let scratch = Scratch::new("escape");
    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).build(),
            request_id: Some(id),
        }
    }));

    let outside = scratch.root.join("outside").join("refs.bib");
    let run = run_latexmk(
        &scratch,
        &server.base_url(),
        &[outside.to_str().unwrap()],
        &[],
    );

    assert_eq!(run.code, 64);
    let stderr = run.stderr_text();
    assert!(
        stderr.contains("outside TUNTEX_WORKSPACE"),
        "stderr was: {stderr}"
    );
    assert!(stderr.contains("refs.bib"));
    assert_eq!(
        server.request_count(),
        0,
        "nothing may be uploaded when an argument points outside the workspace"
    );
}

#[test]
fn an_unreachable_service_is_reported_clearly() {
    let scratch = Scratch::new("offline");

    // Port 1 is privileged and nothing is listening on it.
    let run = run_latexmk(&scratch, "http://127.0.0.1:1", &["main.tex"], &[]);

    assert_eq!(run.code, 69);
    let stderr = run.stderr_text();
    assert!(stderr.contains("cannot connect"), "stderr was: {stderr}");
    assert!(stderr.contains("http://127.0.0.1:1"));
}

#[test]
fn a_missing_client_config_is_a_configuration_error() {
    let scratch = Scratch::new("no-engine");
    let run = run_client(&scratch, "http://127.0.0.1:1", &["main.tex"], &[]);

    assert_eq!(run.code, 64);
    assert!(run.stderr_text().contains("tun-tex-cfg.yaml"));
}

#[test]
fn the_engine_is_inferred_from_the_executable_name() {
    let scratch = Scratch::new("shim");
    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).build(),
            request_id: Some(id),
        }
    }));

    // Copy the binary to xelatex.exe and run it with no TUNTEX_ENGINE.
    let shim = scratch.root.join("xelatex.exe");
    std::fs::copy(BINARY, &shim).unwrap();
    std::fs::write(
        scratch.root.join("tun-tex-cfg.yaml"),
        format!("socket: {}\ntex: xelatex\n", server.base_url()),
    )
    .unwrap();
    let output = Command::new(&shim)
        .arg("main.tex")
        .current_dir(scratch.workspace())
        .env("TUNTEX_URL", server.base_url())
        .env("TUNTEX_WORKSPACE", scratch.workspace_string())
        .env("TUNTEX_CWD", scratch.workspace_string())
        .env("TEMP", scratch.temp())
        .env("TMP", scratch.temp())
        .env_remove("TUNTEX_ENGINE")
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (metadata, _) = decode_request(&server.wait_for_request().body);
    assert_eq!(metadata["engine"], "xelatex");
}

#[test]
fn no_temporary_files_survive_a_successful_run() {
    let scratch = Scratch::new("temp-cleanup");
    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).file("main.pdf", "%PDF").build(),
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(&scratch, &server.base_url(), &["main.tex"], &[]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr_text());

    let leftovers = std::fs::read_dir(scratch.temp().join("tuntex"))
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(
        leftovers, 0,
        "the per-request temporary directory must be removed"
    );
}

#[test]
fn a_compile_that_writes_nothing_leaves_nothing_behind() {
    let scratch = Scratch::new("replace-atomic");
    scratch.write("main.tex", "original");
    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).file("main.tex", "updated").build(),
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(&scratch, &server.base_url(), &["main.tex"], &[]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr_text());
    assert_eq!(scratch.read("main.tex"), "updated");

    let entries: Vec<String> = std::fs::read_dir(scratch.workspace())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "only main.tex should exist, found {entries:?}"
    );
    assert!(
        !entries.iter().any(|name| name.contains("tuntex-tmp")),
        "the atomic-replace temporary file must be gone"
    );
}

#[test]
fn unicode_workspace_content_round_trips() {
    let scratch = Scratch::new("unicode");
    scratch.write("résumé draft/chapitre-un.tex", "Unicode café");
    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        let body = ResultArchive::new(&id)
            .file("résumé draft/build/chapitre-un.pdf", "%PDF")
            .build();
        Reply::Ok {
            body,
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(
        &scratch,
        &server.base_url(),
        &["résumé draft/chapitre-un.tex"],
        &[],
    );
    assert_eq!(run.code, 0, "stderr: {}", run.stderr_text());

    let (metadata, members) = decode_request(&server.wait_for_request().body);
    assert_eq!(
        metadata["argv"],
        serde_json::json!(["résumé draft/chapitre-un.tex"])
    );
    assert!(members.iter().any(|name| name.contains("chapitre-un.tex")));
    assert_eq!(scratch.read("résumé draft/build/chapitre-un.pdf"), "%PDF");
}

#[test]
fn a_build_product_from_a_previous_run_is_uploaded() {
    // Incremental latexmk depends on seeing the previous run's .aux/.fls files.
    let scratch = Scratch::new("incremental");
    scratch.write("main.tex", "x");
    scratch.write("main.aux", "previous aux");
    scratch.write("main.fdb_latexmk", "previous database");
    scratch.write(".git/config", "must not be uploaded");

    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).build(),
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(&scratch, &server.base_url(), &["main.tex"], &[]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr_text());

    let (_, members) = decode_request(&server.wait_for_request().body);
    assert!(members.contains(&"workspace/main.aux".to_string()));
    assert!(members.contains(&"workspace/main.fdb_latexmk".to_string()));
    assert!(
        !members.iter().any(|name| name.contains(".git")),
        "version-control metadata must be excluded"
    );
}

#[test]
fn tuntexignore_excludes_files_from_the_upload() {
    let scratch = Scratch::new("ignore");
    scratch.write("main.tex", "x");
    scratch.write("huge.bin", "not really huge");
    scratch.write(".tuntexignore", "*.bin\n");

    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).build(),
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(&scratch, &server.base_url(), &["main.tex"], &[]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr_text());

    let (_, members) = decode_request(&server.wait_for_request().body);
    assert!(members.contains(&"workspace/main.tex".to_string()));
    assert!(!members.iter().any(|name| name.ends_with(".bin")));
}

#[test]
fn debug_output_goes_to_stderr_and_not_stdout() {
    let scratch = Scratch::new("debug");
    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).stdout(b"compiler output\n").build(),
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(
        &scratch,
        &server.base_url(),
        &["main.tex"],
        &[("TUNTEX_DEBUG", "1")],
    );

    assert_eq!(run.code, 0);
    assert_eq!(
        run.stdout, b"compiler output\n",
        "debug output must never pollute stdout"
    );
    assert!(run.stderr_text().contains("[tuntex]"));
}

#[test]
fn a_timeout_from_the_server_is_reported() {
    let scratch = Scratch::new("timeout");
    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        let mut archive = ResultArchive::new(&id).exit_code(124);
        archive.timed_out = true;
        Reply::Ok {
            body: archive.build(),
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(&scratch, &server.base_url(), &["main.tex"], &[]);
    assert_eq!(
        run.code, 124,
        "a timed-out build reports the remote timeout code"
    );
}

#[test]
fn the_compile_timeout_is_sent_to_the_server() {
    let scratch = Scratch::new("timeout-value");
    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).build(),
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(
        &scratch,
        &server.base_url(),
        &["main.tex"],
        &[("TUNTEX_TIMEOUT", "45")],
    );
    assert_eq!(run.code, 0, "stderr: {}", run.stderr_text());

    let (metadata, _) = decode_request(&server.wait_for_request().body);
    assert_eq!(metadata["timeout_seconds"], 45);
}

#[test]
fn an_oversized_workspace_is_refused_locally() {
    let scratch = Scratch::new("too-big");
    scratch.write("big.bin", &"x".repeat(4096));
    let server = MockServer::start(Box::new(|request| {
        let id = request.header("x-tuntex-request-id").unwrap().to_string();
        Reply::Ok {
            body: ResultArchive::new(&id).build(),
            request_id: Some(id),
        }
    }));

    let run = run_latexmk(
        &scratch,
        &server.base_url(),
        &["main.tex"],
        &[("TUNTEX_MAX_UPLOAD_SIZE", "512")],
    );

    assert_eq!(run.code, 64);
    assert!(run.stderr_text().contains("TUNTEX_MAX_UPLOAD_SIZE"));
    assert_eq!(server.request_count(), 0, "nothing should be uploaded");
}
