//! Client configuration from the YAML file beside the executable.
//!
//! Nothing here ever touches `argv`.  LaTeX Workshop passes real compiler
//! arguments through, so any proxy option smuggled onto the command line would
//! end up forwarded to the backend as a bogus TeX flag.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use serde::Deserialize;

pub const DEFAULT_URL: &str = "http://127.0.0.1:38117";
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 120;
pub const DEFAULT_MAX_UPLOAD_SIZE: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_FILE_COUNT: usize = 50_000;
pub const CONFIG_FILE_NAME: &str = "tun-tex-cfg.yaml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientFileConfig {
    socket: String,
    tex: String,
    #[serde(default)]
    input_mode: InputMode,
    #[serde(default)]
    workspace: Option<PathBuf>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(skip)]
    token: Option<String>,
    #[serde(default)]
    project_key: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    forward_env: Vec<String>,
    #[serde(default)]
    max_upload_size: Option<u64>,
    #[serde(default)]
    max_file_count: Option<usize>,
    #[serde(default)]
    debug: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InputMode {
    #[default]
    Project,
    Temporary,
}

/// Engine names the proxy recognises when inferring from the executable name.
pub const KNOWN_ENGINES: [&str; 7] = [
    "latexmk",
    "xelatex",
    "pdflatex",
    "lualatex",
    "bibtex",
    "biber",
    "makeindex",
];

/// Environment variables whose values are never forwarded to the server.
///
/// The client-side mirror of the server's own blocklist.  These are checked
/// before anything is sent so the failure is local and immediate.
pub const FORBIDDEN_FORWARD: [&str; 18] = [
    "HOME",
    "TMPDIR",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "PATH",
    "TEMP",
    "TMP",
    "TMPDIR",
    "APPDATA",
    "LOCALAPPDATA",
    "USERPROFILE",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "PROGRAMDATA",
];

/// Fully resolved client settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub input_mode: InputMode,
    pub url: String,
    pub engine: String,
    /// Absolute Windows path of the project root that gets uploaded.
    pub workspace: PathBuf,
    /// Absolute Windows path the compile runs in.  Must be inside `workspace`.
    pub cwd: PathBuf,
    pub timeout_seconds: u64,
    pub token: Option<String>,
    pub debug: bool,
    /// Environment variable names to forward, in the order given.
    pub forward_env: Vec<String>,
    pub max_upload_size: u64,
    pub max_file_count: usize,
    pub keep_temp: bool,
}

impl Config {
    /// Read the adjacent instance YAML.
    pub fn from_env(exe_name: &OsStr) -> Result<Self> {
        Self::from_invocation(exe_name, &[], false)
    }

    pub fn from_invocation(
        exe_name: &OsStr,
        arguments: &[std::ffi::OsString],
        information: bool,
    ) -> Result<Self> {
        let executable = std::env::current_exe().map_err(|error| {
            Error::config(format!("could not locate the client executable: {error}"))
        })?;
        let directory = executable
            .parent()
            .ok_or_else(|| Error::config("could not determine the client executable directory"))?;
        let path = directory.join(CONFIG_FILE_NAME);
        let raw = std::fs::read_to_string(&path).map_err(|error| {
            Error::config(format!(
                "could not read client config {}: {error}",
                path.display()
            ))
        })?;
        let mut file: ClientFileConfig = serde_yaml::from_str(&raw).map_err(|error| {
            Error::config(format!(
                "client config {} is not valid YAML: {error}",
                path.display()
            ))
        })?;
        validate_configured_engine(exe_name, &file.tex)?;
        if let Some(key) = file.project_key.take() {
            if key.trim().is_empty() {
                return Err(Error::config("project_key must not be empty"));
            }
            file.token = Some(key);
        } else {
            return Err(Error::config(
                "project_key is required in the instance YAML",
            ));
        }
        if information {
            file.workspace = Some(directory.to_path_buf());
            file.cwd = Some(directory.to_path_buf());
        } else if file.input_mode == InputMode::Temporary {
            let current = std::env::current_dir().map_err(|e| Error::config(e.to_string()))?;
            let inputs: Vec<_> = arguments
                .iter()
                .filter(|argument| {
                    Path::new(argument)
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("tex"))
                })
                .collect();
            if inputs.len() != 1 {
                return Err(Error::config(
                    "temporary mode requires exactly one .tex input",
                ));
            }
            let entry = current
                .join(inputs[0])
                .canonicalize()
                .map_err(|e| Error::config(format!("cannot resolve temporary input: {e}")))?;
            file.workspace = Some(entry.parent().unwrap().to_path_buf());
            file.cwd = file.workspace.clone();
        } else if file.workspace.is_none() {
            return Err(Error::config(
                "project mode requires an explicit workspace in YAML",
            ));
        }
        let root = file
            .workspace
            .as_ref()
            .unwrap()
            .canonicalize()
            .map_err(|e| Error::config(format!("cannot resolve workspace: {e}")))?;
        let system_directory = std::env::var_os("SystemRoot")
            .and_then(|path| crate::pathmap::PathMapper::new(Path::new(&path)).ok())
            .is_some_and(|mapper| mapper.contains(&root));
        if root.parent().is_none() || system_directory {
            return Err(Error::config("workspace must be a document directory, not a filesystem root or Windows directory"));
        }
        Self::from_file(exe_name, file)
    }

    fn from_file(exe_name: &OsStr, file: ClientFileConfig) -> Result<Self> {
        let engine = validate_configured_engine(exe_name, &file.tex)?;
        let socket = file.socket.trim();
        if socket.is_empty() {
            return Err(Error::config("socket must not be empty"));
        }
        let url = if socket.starts_with("http://") || socket.starts_with("https://") {
            socket.to_string()
        } else {
            format!("http://{socket}")
        };
        let parsed = reqwest::Url::parse(&url)
            .map_err(|e| Error::config(format!("invalid socket URL: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(Error::config(
                "socket must be an HTTP(S) URL without credentials, query, or fragment",
            ));
        }
        let workspace = file
            .workspace
            .ok_or_else(|| Error::config("workspace is required"))?;
        if !workspace.is_absolute() || !workspace.is_dir() {
            return Err(Error::config(
                "workspace must be an existing absolute directory",
            ));
        }
        let cwd = file.cwd.unwrap_or_else(|| workspace.clone());
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(Error::config("cwd must be an existing absolute directory"));
        }
        if !crate::pathmap::PathMapper::new(&workspace)?.contains(&cwd) {
            return Err(Error::config("cwd must stay inside workspace"));
        }
        let timeout_seconds = file.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
        let max_upload_size = file.max_upload_size.unwrap_or(DEFAULT_MAX_UPLOAD_SIZE);
        let max_file_count = file.max_file_count.unwrap_or(DEFAULT_MAX_FILE_COUNT);
        if timeout_seconds == 0 || max_upload_size == 0 || max_file_count == 0 {
            return Err(Error::config(
                "timeout_seconds, max_upload_size, and max_file_count must be positive",
            ));
        }
        for name in &file.forward_env {
            check_forwardable(name)?;
        }
        Ok(Self {
            input_mode: file.input_mode,
            url: url.trim_end_matches('/').into(),
            engine,
            workspace,
            cwd,
            timeout_seconds,
            token: file.project_key.or(file.token),
            debug: file.debug.unwrap_or(false),
            forward_env: file.forward_env,
            max_upload_size,
            max_file_count,
            keep_temp: false,
        })
    }

    /// Collect explicitly allowed engine environment variables.
    ///
    /// A name that is not set is skipped rather than sent as empty: the server
    /// should inherit its own value rather than be told the variable is blank.
    pub fn collect_forwarded_env(&self) -> Result<BTreeMap<String, String>> {
        let mut collected = BTreeMap::new();
        for name in &self.forward_env {
            check_forwardable(name)?;
            if let Ok(value) = std::env::var(name) {
                collected.insert(name.clone(), value);
            }
        }
        Ok(collected)
    }
}

/// Reject a name the server would refuse, with a clearer client-side message.
fn check_forwardable(name: &str) -> Result<()> {
    let upper = name.to_ascii_uppercase();
    if FORBIDDEN_FORWARD.contains(&upper.as_str()) {
        return Err(Error::config(format!(
            "forward_env lists {name}, which must not be forwarded to the remote service"
        )));
    }
    Ok(())
}

fn validate_configured_engine(exe_name: &OsStr, configured: &str) -> Result<String> {
    let engine = configured.trim().to_ascii_lowercase();
    if !KNOWN_ENGINES.contains(&engine.as_str()) {
        return Err(Error::config(format!(
            "client config engine {configured:?} is invalid; expected one of {}",
            KNOWN_ENGINES.join(", ")
        )));
    }
    let stem = Path::new(exe_name)
        .file_stem()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if stem != "tuntex-client" && !KNOWN_ENGINES.contains(&stem.as_str()) {
        return Err(Error::config(format!(
            "invalid client executable name {stem:?}"
        )));
    }
    if KNOWN_ENGINES.contains(&stem.as_str()) && stem != engine {
        return Err(Error::config(format!(
            "executable name selects {stem}, but {CONFIG_FILE_NAME} selects {engine}"
        )));
    }
    Ok(engine)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(yaml: &str) -> ClientFileConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn base() -> String {
        format!(
            "socket: 127.0.0.1:38117\ntex: latexmk\nproject_key: secret\nworkspace: '{}'\n",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    #[test]
    fn yaml_defaults_are_applied() {
        let config = Config::from_file(OsStr::new("tuntex-client.exe"), file(&base())).unwrap();
        assert_eq!(config.url, DEFAULT_URL);
        assert_eq!(config.cwd, config.workspace);
        assert_eq!(config.timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(config.token.as_deref(), Some("secret"));
        assert!(!config.keep_temp);
    }

    #[test]
    fn engine_and_executable_allowlists_are_enforced() {
        for engine in KNOWN_ENGINES {
            assert_eq!(
                validate_configured_engine(OsStr::new(&format!("{engine}.exe")), engine).unwrap(),
                engine
            );
        }
        for (exe, engine) in [
            ("shell.exe", "latexmk"),
            ("xelatex.exe", "pdflatex"),
            ("tuntex-client.exe", "shell"),
        ] {
            assert!(validate_configured_engine(OsStr::new(exe), engine).is_err());
        }
    }

    #[test]
    fn yaml_accepts_all_client_fields() {
        let yaml = format!("{}cwd: '{}'\ntimeout_seconds: 45\nforward_env: [TEXINPUTS]\nmax_upload_size: 100000\nmax_file_count: 100\ndebug: true\n",base(),env!("CARGO_MANIFEST_DIR"));
        let config = Config::from_file(OsStr::new("latexmk.exe"), file(&yaml)).unwrap();
        assert_eq!(config.timeout_seconds, 45);
        assert_eq!(config.forward_env, vec!["TEXINPUTS"]);
        assert_eq!(config.max_upload_size, 100000);
        assert_eq!(config.max_file_count, 100);
        assert!(config.debug);
    }

    #[test]
    fn invalid_yaml_and_obsolete_settings_are_rejected() {
        for extra in [
            "token: secret",
            "keep_temp: true",
            "typo: 1",
            "timeout_seconds: nope",
            "input_mode: arbitrary",
        ] {
            assert!(
                serde_yaml::from_str::<ClientFileConfig>(&format!("{}{extra}\n", base())).is_err(),
                "{extra}"
            );
        }
    }

    #[test]
    fn zero_limits_and_invalid_paths_are_rejected() {
        for extra in [
            "timeout_seconds: 0",
            "max_upload_size: 0",
            "max_file_count: 0",
            "cwd: relative",
        ] {
            assert!(
                Config::from_file(
                    OsStr::new("latexmk.exe"),
                    file(&format!("{}{extra}\n", base()))
                )
                .is_err(),
                "{extra}"
            );
        }
        for workspace in [
            None,
            Some(PathBuf::from("relative")),
            Some(PathBuf::from("Z:/not-a-directory")),
        ] {
            let mut config = file(&base());
            config.workspace = workspace;
            assert!(Config::from_file(OsStr::new("latexmk.exe"), config).is_err());
        }
    }

    #[test]
    fn socket_validation_is_strict() {
        for socket in [
            "",
            "http://user:pass@localhost",
            "http://localhost?secret=1",
            "http://localhost#fragment",
        ] {
            let mut config = file(&base());
            config.socket = socket.into();
            assert!(Config::from_file(OsStr::new("latexmk.exe"), config).is_err());
        }
        let mut config = file(&base());
        config.socket = "http://localhost:38117/".into();
        assert_eq!(
            Config::from_file(OsStr::new("latexmk.exe"), config)
                .unwrap()
                .url,
            "http://localhost:38117"
        );
    }

    #[test]
    fn unsafe_engine_environment_variables_are_rejected() {
        for name in ["PATH", "HOME", "LD_PRELOAD", "TMPDIR"] {
            assert!(check_forwardable(name).is_err());
        }
        assert!(check_forwardable("TEXINPUTS").is_ok());
    }
}
