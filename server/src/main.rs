use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tar::{Archive, Builder, EntryType, Header};
use tokio::{io::AsyncWriteExt, sync::Semaphore};
use uuid::Uuid;

const PROTOCOL: u32 = 1;
const HEADER_PROTOCOL: &str = "x-tuntex-protocol";
const HEADER_REQUEST_ID: &str = "x-tuntex-request-id";

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ServerSection {
    host: IpAddr,
    port: u16,
    token: String,
    max_request_size: u64,
    max_result_size: u64,
    max_file_size: u64,
    max_file_count: usize,
    max_concurrent_builds: usize,
    max_timeout_seconds: u64,
    work_root: Option<PathBuf>,
    keep_temp: bool,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".parse().unwrap(),
            port: 38117,
            token: String::new(),
            max_request_size: 512 * 1024 * 1024,
            max_result_size: 512 * 1024 * 1024,
            max_file_size: 256 * 1024 * 1024,
            max_file_count: 50_000,
            max_concurrent_builds: 4,
            max_timeout_seconds: 600,
            work_root: None,
            keep_temp: false,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Engine {
    command: PathBuf,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    server: ServerSection,
    engines: BTreeMap<String, Engine>,
}

impl Config {
    fn load() -> Result<Self, String> {
        let path = env::var_os("TUNTEX_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("tuntex-server.yaml"));
        let raw = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let config: Self = serde_yaml::from_str(&raw)
            .map_err(|e| format!("invalid YAML in {}: {e}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        let s = &self.server;
        if self.engines.is_empty() {
            return Err("engines must not be empty".into());
        }
        if s.max_request_size == 0
            || s.max_result_size == 0
            || s.max_file_size == 0
            || s.max_file_count == 0
            || s.max_concurrent_builds == 0
            || s.max_timeout_seconds == 0
        {
            return Err("all resource limits must be positive".into());
        }
        for (name, engine) in &self.engines {
            if !valid_engine_name(name) {
                return Err(format!("invalid engine name {name:?}"));
            }
            if engine.command.as_os_str().is_empty() {
                return Err(format!("engine {name:?} has an empty command"));
            }
            if engine.args.iter().any(|v| v.contains('\0')) {
                return Err(format!("engine {name:?} contains a NUL argument"));
            }
        }
        Ok(())
    }
}

fn valid_engine_name(name: &str) -> bool {
    matches!(
        name,
        "latexmk" | "xelatex" | "pdflatex" | "lualatex" | "bibtex" | "biber" | "makeindex"
    )
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    slots: Arc<Semaphore>,
    jobs: Arc<Mutex<HashMap<Uuid, Arc<AtomicBool>>>>,
}

#[derive(Debug)]
struct ApiError(StatusCode, &'static str, String);

impl ApiError {
    fn bad(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, "bad_request", message.into())
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_metadata",
            message.into(),
        )
    }
    fn too_large(message: impl Into<String>) -> Self {
        Self(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            message.into(),
        )
    }
    fn internal(message: impl Into<String>) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            message.into(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(serde_json::json!({"error": self.1, "message": self.2})),
        )
            .into_response()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestMeta {
    protocol: u32,
    request_id: Uuid,
    engine: String,
    #[serde(default)]
    argv: Vec<String>,
    cwd: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    timeout_seconds: u64,
}

#[derive(Serialize)]
struct ResultMeta {
    protocol: u32,
    request_id: Uuid,
    exit_code: i32,
    timed_out: bool,
    cancelled: bool,
    duration_ms: u64,
    changed: Vec<String>,
    deleted: Vec<String>,
}

#[derive(Clone, Eq, PartialEq)]
struct FileStamp {
    size: u64,
    hash: [u8; 32],
}

#[tokio::main]
async fn main() {
    let result = match env::args().nth(1).as_deref() {
        None | Some("run") | Some("__run") => serve().await,
        Some("start") => start_server(),
        Some("ls") => list_server(),
        Some("stop") => stop_server(),
        Some(command) => Err(format!(
            "unknown command {command:?}; expected start, ls, stop, or run"
        )),
    };
    if let Err(error) = result {
        eprintln!("tuntex-server: {error}");
        std::process::exit(1);
    }
}

fn state_dir() -> Result<PathBuf, String> {
    #[cfg(windows)]
    let base = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    let path = base
        .ok_or_else(|| "cannot determine the per-user state directory".to_string())?
        .join("tuntex");
    fs::create_dir_all(&path)
        .map_err(|e| format!("cannot create state directory {}: {e}", path.display()))?;
    Ok(path)
}

fn pid_path() -> Result<PathBuf, String> {
    Ok(state_dir()?.join("server.pid"))
}

fn read_pid() -> Result<Option<u32>, String> {
    let path = pid_path()?;
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    raw.trim()
        .parse::<u32>()
        .map(Some)
        .map_err(|_| format!("invalid PID in {}", path.display()))
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

fn remove_stale_pid() -> Result<(), String> {
    let path = pid_path()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
    }
}

fn start_server() -> Result<(), String> {
    if let Some(pid) = read_pid()? {
        if process_alive(pid) {
            return Err(format!("already running (pid {pid})"));
        }
        remove_stale_pid()?;
    }

    let directory = state_dir()?;
    let log_path = directory.join("server.log");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("cannot open {}: {e}", log_path.display()))?;
    let stderr = log
        .try_clone()
        .map_err(|e| format!("cannot duplicate log handle: {e}"))?;
    let executable = env::current_exe().map_err(|e| format!("cannot locate executable: {e}"))?;
    let mut command = Command::new(executable);
    command
        .arg("__run")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    detach(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start background process: {e}"))?;
    std::thread::sleep(Duration::from_millis(200));
    if let Some(status) = child
        .try_wait()
        .map_err(|e| format!("cannot inspect background process: {e}"))?
    {
        return Err(format!(
            "background process exited with {status}; see {}",
            log_path.display()
        ));
    }
    let pid = child.id();
    fs::write(pid_path()?, format!("{pid}\n"))
        .map_err(|e| format!("cannot write PID file: {e}"))?;
    println!(
        "tuntex-server: started (pid {pid}, log {})",
        log_path.display()
    );
    Ok(())
}

fn list_server() -> Result<(), String> {
    match read_pid()? {
        Some(pid) if process_alive(pid) => {
            println!("tuntex-server: running (pid {pid})");
            Ok(())
        }
        Some(_) => {
            remove_stale_pid()?;
            println!("tuntex-server: stopped");
            Ok(())
        }
        None => {
            println!("tuntex-server: stopped");
            Ok(())
        }
    }
}

fn stop_server() -> Result<(), String> {
    let Some(pid) = read_pid()? else {
        return Err("not running".into());
    };
    if !process_alive(pid) {
        remove_stale_pid()?;
        return Err("not running (removed stale PID file)".into());
    }
    terminate(pid)?;
    for _ in 0..50 {
        if !process_alive(pid) {
            remove_stale_pid()?;
            println!("tuntex-server: stopped (pid {pid})");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("process {pid} did not stop within 5 seconds"))
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
}

#[cfg(unix)]
fn terminate(pid: u32) -> Result<(), String> {
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "cannot stop process {pid}: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(windows)]
fn terminate(pid: u32) -> Result<(), String> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T"])
        .status()
        .map_err(|e| format!("cannot run taskkill: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("taskkill failed for process {pid}"))
    }
}

async fn serve() -> Result<(), String> {
    let config = Arc::new(Config::load()?);
    let address = SocketAddr::new(config.server.host, config.server.port);
    let state = AppState {
        slots: Arc::new(Semaphore::new(config.server.max_concurrent_builds)),
        jobs: Arc::new(Mutex::new(HashMap::new())),
        config,
    };
    let app = Router::new()
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({"ok": true})) }),
        )
        .route("/info", get(info))
        .route("/compile", post(compile))
        .route("/jobs/{id}", delete(cancel))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|e| format!("cannot bind {address}: {e}"))?;
    eprintln!("tuntex-server: listening on http://{address}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(|e| e.to_string())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn info(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(
        serde_json::json!({"service":"tuntex","protocol":PROTOCOL,"engines":state.config.engines.keys().collect::<Vec<_>>()}),
    )
}

fn check_headers(headers: &HeaderMap, config: &Config) -> Result<Uuid, ApiError> {
    if headers.get(HEADER_PROTOCOL).and_then(|v| v.to_str().ok()) != Some("1") {
        return Err(ApiError::bad(format!(
            "missing or unsupported {HEADER_PROTOCOL}"
        )));
    }
    if !config.server.token.is_empty() {
        let supplied = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if supplied
            .as_bytes()
            .ct_eq(config.server.token.as_bytes())
            .unwrap_u8()
            != 1
        {
            return Err(ApiError(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "invalid bearer token".into(),
            ));
        }
    }
    let id = headers
        .get(HEADER_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::bad(format!("missing {HEADER_REQUEST_ID}")))?;
    Uuid::parse_str(id).map_err(|_| ApiError::bad("invalid request id"))
}

async fn compile(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let id = check_headers(&headers, &state.config)?;
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(""))
        != Some("application/gzip")
    {
        return Err(ApiError::bad("Content-Type must be application/gzip"));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut jobs = state
            .jobs
            .lock()
            .map_err(|_| ApiError::internal("job registry poisoned"))?;
        if jobs.insert(id, cancel.clone()).is_some() {
            return Err(ApiError::bad("request id is already active"));
        }
    }
    let result = compile_inner(state.clone(), id, cancel, body).await;
    if let Ok(mut jobs) = state.jobs.lock() {
        jobs.remove(&id);
    }
    result
}

async fn compile_inner(
    state: AppState,
    id: Uuid,
    cancel: Arc<AtomicBool>,
    body: Body,
) -> Result<Response, ApiError> {
    let root = state
        .config
        .server
        .work_root
        .clone()
        .unwrap_or_else(env::temp_dir)
        .join("tuntex-server");
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let job = tempfile::Builder::new()
        .prefix(&format!("{id}-"))
        .tempdir_in(root)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let request_path = job.path().join("request.tar.gz");
    let mut output = tokio::fs::File::create(&request_path)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let mut total = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ApiError::bad(format!("cannot read request body: {e}")))?;
        total = total
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| ApiError::too_large("request size overflow"))?;
        if total > state.config.server.max_request_size {
            return Err(ApiError::too_large("request exceeds configured limit"));
        }
        output
            .write_all(&chunk)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?;
    }
    output
        .flush()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    drop(output);
    if total == 0 {
        return Err(ApiError::bad("empty request body"));
    }
    let _permit = state
        .slots
        .acquire()
        .await
        .map_err(|_| ApiError::internal("server is shutting down"))?;
    let config = state.config.clone();
    let path = job.path().to_path_buf();
    let archive =
        tokio::task::spawn_blocking(move || run_job(&config, id, &path, &request_path, &cancel))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))??;
    let bytes = tokio::fs::read(archive)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if state.config.server.keep_temp {
        let _ = job.keep();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/gzip")
        .header(HEADER_PROTOCOL, PROTOCOL.to_string())
        .header(HEADER_REQUEST_ID, id.to_string())
        .body(Body::from(bytes))
        .map_err(|e| ApiError::internal(e.to_string()))
}

async fn cancel(
    State(state): State<AppState>,
    AxumPath(raw): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let header_id = check_headers(&headers, &state.config)?;
    let id = Uuid::parse_str(&raw).map_err(|_| ApiError::bad("invalid request id"))?;
    if header_id != id {
        return Err(ApiError::bad("request id header does not match path"));
    }
    let jobs = state
        .jobs
        .lock()
        .map_err(|_| ApiError::internal("job registry poisoned"))?;
    let flag = jobs.get(&id).ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "job_not_found",
            "job is not active".into(),
        )
    })?;
    flag.store(true, Ordering::Release);
    Ok(Json(serde_json::json!({"cancelled":true,"request_id":id})))
}

fn run_job(
    config: &Config,
    id: Uuid,
    job: &Path,
    request_archive: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf, ApiError> {
    let workspace = job.join("workspace");
    fs::create_dir_all(&workspace).map_err(|e| ApiError::internal(e.to_string()))?;
    extract_request(config, request_archive, job)?;
    let metadata_path = job.join("meta/request.json");
    let raw =
        fs::read(&metadata_path).map_err(|_| ApiError::invalid("missing meta/request.json"))?;
    if raw.len() > 1024 * 1024 {
        return Err(ApiError::invalid("request metadata is too large"));
    }
    let meta: RequestMeta = serde_json::from_slice(&raw)
        .map_err(|e| ApiError::invalid(format!("invalid request metadata: {e}")))?;
    validate_meta(config, id, &meta)?;
    let cwd = virtual_path(&workspace, &meta.cwd)?;
    if !cwd.is_dir() {
        return Err(ApiError::invalid("cwd is not a directory"));
    }
    let before = snapshot(&workspace)?;
    let engine = config
        .engines
        .get(&meta.engine)
        .ok_or_else(|| ApiError::invalid("engine is not configured"))?;
    let mapped_argv = meta
        .argv
        .iter()
        .map(|argument| map_argument(&workspace, argument))
        .collect::<Result<Vec<_>, _>>()?;
    prepare_output_directories(&workspace, &cwd, &mapped_argv)?;
    let mut command = Command::new(&engine.command);
    command
        .args(&engine.args)
        .args(&mapped_argv)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &meta.env {
        command.env(key, value);
    }
    configure_process_group(&mut command);
    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|e| ApiError::internal(format!("cannot start engine: {e}")))?;
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::internal("stdout pipe unavailable"))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ApiError::internal("stderr pipe unavailable"))?;
    let stream_limit = config.server.max_result_size;
    let out_reader = std::thread::spawn(move || read_pipe(stdout_pipe, stream_limit));
    let err_reader = std::thread::spawn(move || read_pipe(stderr_pipe, stream_limit));
    let timeout = Duration::from_secs(meta.timeout_seconds.min(config.server.max_timeout_seconds));
    let mut timed_out = false;
    let mut cancelled = false;
    loop {
        if cancel.load(Ordering::Acquire) {
            cancelled = true;
            terminate_tree(&mut child);
            break;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            terminate_tree(&mut child);
            break;
        }
        if child
            .try_wait()
            .map_err(|e| ApiError::internal(e.to_string()))?
            .is_some()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let status = child
        .wait()
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let stdout = out_reader
        .join()
        .map_err(|_| ApiError::internal("stdout reader panicked"))??;
    let stderr = err_reader
        .join()
        .map_err(|_| ApiError::internal("stderr reader panicked"))??;
    let exit_code = if timed_out {
        124
    } else if cancelled {
        130
    } else {
        status.code().unwrap_or(70)
    };
    let after = snapshot(&workspace)?;
    if after.len() > config.server.max_file_count {
        return Err(ApiError::too_large(
            "compiled workspace exceeds configured file-count limit",
        ));
    }
    let mut changed = Vec::new();
    let mut deleted = Vec::new();
    for (path, stamp) in &after {
        if before.get(path) != Some(stamp) {
            changed.push(path.clone());
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            deleted.push(path.clone());
        }
    }
    let result = ResultMeta {
        protocol: PROTOCOL,
        request_id: id,
        exit_code,
        timed_out,
        cancelled,
        duration_ms: started.elapsed().as_millis() as u64,
        changed: changed.clone(),
        deleted,
    };
    let result_path = job.join("result.tar.gz");
    build_result(
        config,
        &result_path,
        &workspace,
        &result,
        &stdout,
        &stderr,
        &changed,
    )?;
    Ok(result_path)
}

fn prepare_output_directories(
    workspace: &Path,
    cwd: &Path,
    argv: &[String],
) -> Result<(), ApiError> {
    let mut next_is_directory = false;
    for argument in argv {
        let value = if next_is_directory {
            next_is_directory = false;
            Some(argument.as_str())
        } else if let Some(value) = argument
            .strip_prefix("-output-directory=")
            .or_else(|| argument.strip_prefix("--output-directory="))
            .or_else(|| argument.strip_prefix("-outdir="))
            .or_else(|| argument.strip_prefix("--outdir="))
            .or_else(|| argument.strip_prefix("-aux-directory="))
            .or_else(|| argument.strip_prefix("--aux-directory="))
        {
            Some(value)
        } else if matches!(
            argument.as_str(),
            "-output-directory"
                | "--output-directory"
                | "-outdir"
                | "--outdir"
                | "-aux-directory"
                | "--aux-directory"
        ) {
            next_is_directory = true;
            None
        } else {
            None
        };
        let Some(value) = value else { continue };
        if value.is_empty() || value.starts_with('-') {
            return Err(ApiError::invalid("output directory is empty or invalid"));
        }
        let path = if value.starts_with("/workspace") {
            virtual_path(workspace, value)?
        } else {
            cwd.join(clean_relative(value)?)
        };
        fs::create_dir_all(path).map_err(|e| ApiError::internal(e.to_string()))?;
    }
    Ok(())
}

fn validate_meta(config: &Config, id: Uuid, meta: &RequestMeta) -> Result<(), ApiError> {
    if meta.protocol != PROTOCOL || meta.request_id != id {
        return Err(ApiError::invalid("protocol or request id mismatch"));
    }
    if !valid_engine_name(&meta.engine) || !config.engines.contains_key(&meta.engine) {
        return Err(ApiError::invalid("engine is not allowed"));
    }
    if meta.argv.len() > 4096 || meta.argv.iter().map(|v| v.len()).sum::<usize>() > 1024 * 1024 {
        return Err(ApiError::invalid("argv exceeds configured limits"));
    }
    if meta
        .argv
        .iter()
        .any(|v| v.contains('\0') || v.contains('\\'))
    {
        return Err(ApiError::invalid("argv contains an unsafe path or NUL"));
    }
    if meta.env.len() > 256 {
        return Err(ApiError::invalid("too many environment variables"));
    }
    const BLOCKED: &[&str] = &[
        "PATH",
        "TEMP",
        "TMP",
        "TMPDIR",
        "HOME",
        "USERPROFILE",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
    ];
    for (key, value) in &meta.env {
        if key.is_empty()
            || key.contains(['=', '\0'])
            || value.contains('\0')
            || BLOCKED.contains(&key.to_ascii_uppercase().as_str())
        {
            return Err(ApiError::invalid(format!(
                "environment variable {key:?} is not allowed"
            )));
        }
    }
    if meta.timeout_seconds == 0 {
        return Err(ApiError::invalid("timeout must be positive"));
    }
    Ok(())
}

fn read_pipe(pipe: impl Read, limit: u64) -> Result<Vec<u8>, ApiError> {
    let mut bytes = Vec::new();
    pipe.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if bytes.len() as u64 > limit {
        return Err(ApiError::too_large(
            "compiler output exceeds configured result limit",
        ));
    }
    Ok(bytes)
}

fn map_argument(workspace: &Path, argument: &str) -> Result<String, ApiError> {
    let (prefix, value) = argument
        .split_once('=')
        .map_or(("", argument), |(left, right)| (left, right));
    if value == "/workspace" || value.starts_with("/workspace/") {
        let mapped = virtual_path(workspace, value)?
            .to_string_lossy()
            .into_owned();
        return Ok(if prefix.is_empty() {
            mapped
        } else {
            format!("{prefix}={mapped}")
        });
    }
    if value.starts_with('/')
        || value.starts_with("//")
        || (value.len() >= 2 && value.as_bytes()[1] == b':')
    {
        return Err(ApiError::invalid(
            "argument contains an absolute path outside /workspace",
        ));
    }
    Ok(argument.to_string())
}

fn clean_relative(raw: &str) -> Result<PathBuf, ApiError> {
    if raw.is_empty() || raw.contains('\\') {
        return Err(ApiError::invalid("unsafe archive path"));
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(ApiError::invalid("absolute archive path"));
    }
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(value) => clean.push(value),
            Component::CurDir => {}
            _ => return Err(ApiError::invalid("archive path escapes its root")),
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(ApiError::invalid("empty archive path"));
    }
    Ok(clean)
}

fn extract_request(
    config: &Config,
    archive_path: &Path,
    destination: &Path,
) -> Result<(), ApiError> {
    let file = fs::File::open(archive_path).map_err(|e| ApiError::bad(e.to_string()))?;
    let mut archive = Archive::new(GzDecoder::new(file));
    let mut count = 0usize;
    let mut total = 0u64;
    let entries = archive
        .entries()
        .map_err(|e| ApiError::bad(format!("invalid tar.gz: {e}")))?;
    for item in entries {
        let mut entry = item.map_err(|e| ApiError::bad(e.to_string()))?;
        if entry.header().entry_type() == EntryType::Directory {
            continue;
        }
        if entry.header().entry_type() != EntryType::Regular {
            return Err(ApiError::invalid(
                "links and special archive entries are forbidden",
            ));
        }
        let raw = entry.path_bytes();
        let name = std::str::from_utf8(&raw)
            .map_err(|_| ApiError::invalid("archive path is not UTF-8"))?;
        let relative = clean_relative(name)?;
        let head = relative.components().next().and_then(|v| match v {
            Component::Normal(n) => n.to_str(),
            _ => None,
        });
        if !matches!(head, Some("meta" | "workspace")) {
            return Err(ApiError::invalid("archive member has an invalid prefix"));
        }
        count += 1;
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or_else(|| ApiError::too_large("expanded size overflow"))?;
        if count > config.server.max_file_count
            || size > config.server.max_file_size
            || total > config.server.max_request_size
        {
            return Err(ApiError::too_large(
                "expanded archive exceeds configured limits",
            ));
        }
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| ApiError::internal(e.to_string()))?;
        }
        let mut out = fs::File::create(target).map_err(|e| ApiError::internal(e.to_string()))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| ApiError::bad(e.to_string()))?;
    }
    Ok(())
}

fn virtual_path(workspace: &Path, value: &str) -> Result<PathBuf, ApiError> {
    if value != "/workspace" && !value.starts_with("/workspace/") {
        return Err(ApiError::invalid("path is outside /workspace"));
    }
    let relative = value
        .strip_prefix("/workspace")
        .ok_or_else(|| ApiError::invalid("cwd is outside /workspace"))?
        .trim_start_matches('/');
    if relative.is_empty() {
        Ok(workspace.to_path_buf())
    } else {
        Ok(workspace.join(clean_relative(relative)?))
    }
}

fn snapshot(root: &Path) -> Result<BTreeMap<String, FileStamp>, ApiError> {
    fn visit(
        root: &Path,
        directory: &Path,
        output: &mut BTreeMap<String, FileStamp>,
    ) -> Result<(), ApiError> {
        for item in fs::read_dir(directory).map_err(|e| ApiError::internal(e.to_string()))? {
            let item = item.map_err(|e| ApiError::internal(e.to_string()))?;
            let ty = item
                .file_type()
                .map_err(|e| ApiError::internal(e.to_string()))?;
            if ty.is_symlink() {
                return Err(ApiError::invalid("workspace links are forbidden"));
            }
            if ty.is_dir() {
                visit(root, &item.path(), output)?;
            } else if ty.is_file() {
                let path = item.path();
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let mut file =
                    fs::File::open(&path).map_err(|e| ApiError::internal(e.to_string()))?;
                let mut hash = Sha256::new();
                let size = std::io::copy(&mut file, &mut hash)
                    .map_err(|e| ApiError::internal(e.to_string()))?;
                output.insert(
                    relative,
                    FileStamp {
                        size,
                        hash: hash.finalize().into(),
                    },
                );
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result)?;
    Ok(result)
}

fn append_bytes<W: Write>(
    builder: &mut Builder<W>,
    name: &str,
    bytes: &[u8],
) -> Result<(), ApiError> {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_data(&mut header, name, bytes)
        .map_err(|e| ApiError::internal(e.to_string()))
}

fn build_result(
    config: &Config,
    path: &Path,
    workspace: &Path,
    meta: &ResultMeta,
    stdout: &[u8],
    stderr: &[u8],
    changed: &[String],
) -> Result<(), ApiError> {
    let metadata = serde_json::to_vec(meta).map_err(|e| ApiError::internal(e.to_string()))?;
    let mut total = metadata.len() as u64 + stdout.len() as u64 + stderr.len() as u64;
    for name in changed {
        total = total
            .checked_add(
                fs::metadata(workspace.join(clean_relative(name)?))
                    .map_err(|e| ApiError::internal(e.to_string()))?
                    .len(),
            )
            .ok_or_else(|| ApiError::too_large("result size overflow"))?;
    }
    if total > config.server.max_result_size {
        return Err(ApiError::too_large("result exceeds configured limit"));
    }
    let file = fs::File::create(path).map_err(|e| ApiError::internal(e.to_string()))?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = Builder::new(encoder);
    append_bytes(&mut builder, "meta/result.json", &metadata)?;
    append_bytes(&mut builder, "stdout.bin", stdout)?;
    append_bytes(&mut builder, "stderr.bin", stderr)?;
    for name in changed {
        builder
            .append_path_with_name(
                workspace.join(clean_relative(name)?),
                format!("files/{name}"),
            )
            .map_err(|e| ApiError::internal(e.to_string()))?;
    }
    builder
        .into_inner()
        .and_then(|e| e.finish())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(())
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0000_0200);
}

#[cfg(unix)]
fn terminate_tree(child: &mut std::process::Child) {
    let _ = Command::new("kill")
        .args(["-TERM", &format!("-{}", child.id())])
        .status();
    std::thread::sleep(Duration::from_millis(200));
    let _ = Command::new("kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .status();
    let _ = child.kill();
}

#[cfg(windows)]
fn terminate_tree(child: &mut std::process::Child) {
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .status();
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_names_are_a_closed_allowlist() {
        for name in [
            "latexmk",
            "xelatex",
            "pdflatex",
            "lualatex",
            "bibtex",
            "biber",
            "makeindex",
        ] {
            assert!(valid_engine_name(name));
        }
        for name in ["", "cmd", "../latexmk", "latexmk.exe", "LATEXMK"] {
            assert!(!valid_engine_name(name));
        }
    }

    #[test]
    fn relative_paths_cannot_escape() {
        for path in ["../secret", "/absolute", "a\\b", "C:/secret", ""] {
            assert!(clean_relative(path).is_err(), "accepted {path:?}");
        }
        assert_eq!(clean_relative("a/./b").unwrap(), PathBuf::from("a/b"));
    }

    #[test]
    fn virtual_paths_require_an_exact_workspace_prefix() {
        let root = Path::new("root");
        assert_eq!(virtual_path(root, "/workspace").unwrap(), root);
        assert_eq!(virtual_path(root, "/workspace/a").unwrap(), root.join("a"));
        assert!(virtual_path(root, "/workspace-evil/a").is_err());
        assert!(virtual_path(root, "/etc/passwd").is_err());
    }

    #[test]
    fn arguments_map_only_protocol_paths() {
        let root = Path::new("root");
        assert_eq!(map_argument(root, "main.tex").unwrap(), "main.tex");
        assert_eq!(
            map_argument(root, "-outdir=/workspace/build").unwrap(),
            format!("-outdir={}", root.join("build").display())
        );
        assert!(map_argument(root, "/etc/passwd").is_err());
        assert!(map_argument(root, "-outdir=C:/temp").is_err());
    }

    #[test]
    fn output_reader_enforces_its_limit() {
        assert_eq!(read_pipe(&b"1234"[..], 4).unwrap(), b"1234");
        assert!(read_pipe(&b"12345"[..], 4).is_err());
    }

    #[test]
    fn yaml_rejects_unknown_server_keys() {
        let raw = "server:\n  typo: true\nengines:\n  latexmk:\n    command: latexmk\n";
        assert!(serde_yaml::from_str::<Config>(raw).is_err());
    }

    #[test]
    fn yaml_rejects_non_latex_engines() {
        let raw = "engines:\n  shell:\n    command: cmd\n";
        let config: Config = serde_yaml::from_str(raw).unwrap();
        assert!(config.validate().is_err());
    }
}
