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
    workspace: Option<PathBuf>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    token: Option<String>,
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
    #[serde(default)]
    keep_temp: Option<bool>,
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
pub const FORBIDDEN_FORWARD: [&str; 13] = [
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
    /// Read configuration from the process environment.
    pub fn from_env(exe_name: &OsStr) -> Result<Self> {
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
        let file: ClientFileConfig = serde_yaml::from_str(&raw).map_err(|error| {
            Error::config(format!(
                "client config {} is not valid YAML: {error}",
                path.display()
            ))
        })?;
        Self::from_sources(exe_name, &|_| None, Some(file))
    }

    /// Read configuration from an arbitrary lookup, so tests need not mutate
    /// the real process environment.
    pub fn from_lookup(exe_name: &OsStr, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        Self::from_sources(exe_name, lookup, None)
    }

    fn from_sources(
        exe_name: &OsStr,
        lookup: &dyn Fn(&str) -> Option<String>,
        file: Option<ClientFileConfig>,
    ) -> Result<Self> {
        let (engine, configured_url) = match &file {
            Some(file) => {
                let engine = validate_configured_engine(exe_name, &file.tex)?;
                let socket = file.socket.trim();
                if socket.is_empty() {
                    return Err(Error::config("client config 'socket' must not be empty"));
                }
                let url = if socket.starts_with("http://") || socket.starts_with("https://") {
                    socket.to_string()
                } else {
                    format!("http://{socket}")
                };
                (engine, url)
            }
            None => (
                resolve_engine(exe_name)?,
                lookup("TUNTEX_URL").unwrap_or_else(|| DEFAULT_URL.to_string()),
            ),
        };

        let workspace_raw = lookup("TUNTEX_WORKSPACE")
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                file.as_ref()
                    .and_then(|config| config.workspace.as_ref())
                    .map(|path| path.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        let workspace = PathBuf::from(&workspace_raw);
        if !workspace.is_absolute() {
            return Err(Error::config(format!(
                "TUNTEX_WORKSPACE must be an absolute path: {workspace_raw}"
            )));
        }
        if !workspace.is_dir() {
            return Err(Error::config(format!(
                "TUNTEX_WORKSPACE is not an existing directory: {workspace_raw}"
            )));
        }

        let cwd = match lookup("TUNTEX_CWD").or_else(|| {
            file.as_ref()
                .and_then(|config| config.cwd.as_ref())
                .map(|path| path.to_string_lossy().into_owned())
        }) {
            Some(value) if !value.trim().is_empty() => PathBuf::from(value),
            _ => workspace.clone(),
        };
        if !cwd.is_absolute() {
            return Err(Error::config(format!(
                "TUNTEX_CWD must be an absolute path: {}",
                cwd.display()
            )));
        }

        let url = lookup("TUNTEX_URL")
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(configured_url);
        if url.trim().is_empty() {
            return Err(Error::config("client config 'socket' must not be empty"));
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(Error::config(format!(
                "TUNTEX_URL must start with http:// or https://: {url}"
            )));
        }

        let timeout_seconds = parse_u64(
            lookup,
            "TUNTEX_TIMEOUT",
            file.as_ref()
                .and_then(|config| config.timeout_seconds)
                .unwrap_or(DEFAULT_TIMEOUT_SECONDS),
        )?;
        if timeout_seconds == 0 {
            return Err(Error::config("TUNTEX_TIMEOUT must be greater than zero"));
        }
        let max_upload_size = parse_u64(
            lookup,
            "TUNTEX_MAX_UPLOAD_SIZE",
            file.as_ref()
                .and_then(|config| config.max_upload_size)
                .unwrap_or(DEFAULT_MAX_UPLOAD_SIZE),
        )?;
        if max_upload_size == 0 {
            return Err(Error::config("max_upload_size must be greater than zero"));
        }
        let max_file_count = parse_u64(
            lookup,
            "TUNTEX_MAX_FILE_COUNT",
            file.as_ref()
                .and_then(|config| config.max_file_count)
                .unwrap_or(DEFAULT_MAX_FILE_COUNT) as u64,
        )?;
        let max_file_count = usize::try_from(max_file_count)
            .map_err(|_| Error::config("TUNTEX_MAX_FILE_COUNT is too large for this platform"))?;
        if max_file_count == 0 {
            return Err(Error::config("max_file_count must be greater than zero"));
        }

        let token = lookup("TUNTEX_TOKEN")
            .or_else(|| file.as_ref().and_then(|config| config.token.clone()))
            .filter(|value| !value.is_empty());

        let forward_env = lookup("TUNTEX_FORWARD_ENV")
            .filter(|value| !value.trim().is_empty())
            .map(|value| split_list(&value))
            .unwrap_or_else(|| {
                file.as_ref()
                    .map(|config| config.forward_env.clone())
                    .unwrap_or_default()
            });

        let debug = parse_bool_optional(lookup, "TUNTEX_DEBUG")?
            .or_else(|| file.as_ref().and_then(|config| config.debug))
            .unwrap_or(false);
        let keep_temp = parse_bool_optional(lookup, "TUNTEX_KEEP_TEMP")?
            .or_else(|| file.as_ref().and_then(|config| config.keep_temp))
            .unwrap_or(false);

        Ok(Self {
            url: url.trim_end_matches('/').to_string(),
            engine,
            workspace,
            cwd,
            timeout_seconds,
            token,
            debug,
            forward_env,
            max_upload_size,
            max_file_count,
            keep_temp,
        })
    }

    /// Collect the environment variables requested by `TUNTEX_FORWARD_ENV`.
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
            "TUNTEX_FORWARD_ENV lists {name}, which must not be forwarded to the remote service"
        )));
    }
    Ok(())
}

/// Work out which backend to ask for.
///
/// Precedence: `TUNTEX_ENGINE`, then the executable's own name (so the
/// binary can be copied to `xelatex.exe` and act as a drop-in shim), then an
/// error -- never a guess.
fn resolve_engine(exe_name: &OsStr) -> Result<String> {
    let stem = Path::new(exe_name)
        .file_stem()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if KNOWN_ENGINES.contains(&stem.as_str()) {
        return Ok(stem);
    }

    let exe_display = Path::new(exe_name)
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| exe_name.to_string_lossy().into_owned());

    Err(Error::config(format!(
        "invalid client executable name {exe_display:?}; expected one of {}. \
         Rename or hard-link tuntex-client to <engine>.exe",
        KNOWN_ENGINES.join(", ")
    )))
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

fn parse_u64(lookup: &dyn Fn(&str) -> Option<String>, name: &str, default: u64) -> Result<u64> {
    match lookup(name) {
        Some(value) if !value.trim().is_empty() => value.trim().parse::<u64>().map_err(|_| {
            Error::config(format!("{name} must be a positive integer, got {value:?}"))
        }),
        _ => Ok(default),
    }
}

fn parse_bool_optional(
    lookup: &dyn Fn(&str) -> Option<String>,
    name: &str,
) -> Result<Option<bool>> {
    match lookup(name) {
        Some(value) if !value.trim().is_empty() => match value.trim().to_ascii_lowercase().as_str()
        {
            "1" | "true" | "yes" | "on" => Ok(Some(true)),
            "0" | "false" | "no" | "off" => Ok(Some(false)),
            other => Err(Error::config(format!(
                "{name} must be a boolean-ish value (0/1/true/false), got {other:?}"
            ))),
        },
        _ => Ok(None),
    }
}

/// Split a comma or semicolon separated list, dropping blanks.
fn split_list(value: &str) -> Vec<String> {
    value
        .split([',', ';'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        }
    }

    fn config_from(exe: &str, pairs: &[(&str, &str)]) -> Result<Config> {
        let lookup = lookup_from(pairs);
        Config::from_lookup(OsStr::new(exe), &lookup)
    }

    #[test]
    fn engine_override_cannot_bypass_executable_allowlist() {
        let error = config_from(
            "not-an-engine.exe",
            &[
                ("TUNTEX_ENGINE", "latexmk"),
                ("TUNTEX_WORKSPACE", env!("CARGO_MANIFEST_DIR")),
            ],
        )
        .unwrap_err();
        assert!(error.message().contains("invalid client executable name"));
    }

    #[test]
    fn configured_engine_is_strictly_validated() {
        assert_eq!(
            validate_configured_engine(OsStr::new("tuntex-client.exe"), "xelatex").unwrap(),
            "xelatex"
        );
        assert!(validate_configured_engine(OsStr::new("tuntex-client.exe"), "cmd").is_err());
        assert!(validate_configured_engine(OsStr::new("random.exe"), "xelatex").is_err());
        assert!(validate_configured_engine(OsStr::new("pdflatex.exe"), "xelatex").is_err());
    }

    #[test]
    fn yaml_can_define_every_client_setting() {
        let raw = format!(
            "socket: example.test:9000\n\
             tex: xelatex\n\
             workspace: '{}'\n\
             cwd: '{}'\n\
             token: secret\n\
             timeout_seconds: 45\n\
             forward_env: [TEXINPUTS, SOURCE_DATE_EPOCH]\n\
             max_upload_size: 123456\n\
             max_file_count: 321\n\
             debug: true\n\
             keep_temp: true\n",
            env!("CARGO_MANIFEST_DIR"),
            env!("CARGO_MANIFEST_DIR")
        );
        let file: ClientFileConfig = serde_yaml::from_str(&raw).unwrap();
        let config =
            Config::from_sources(OsStr::new("tuntex-client.exe"), &|_| None, Some(file)).unwrap();
        assert_eq!(config.url, "http://example.test:9000");
        assert_eq!(config.engine, "xelatex");
        assert_eq!(config.timeout_seconds, 45);
        assert_eq!(config.token.as_deref(), Some("secret"));
        assert_eq!(config.forward_env, ["TEXINPUTS", "SOURCE_DATE_EPOCH"]);
        assert_eq!(config.max_upload_size, 123456);
        assert_eq!(config.max_file_count, 321);
        assert!(config.debug);
        assert!(config.keep_temp);
    }

    #[test]
    fn engine_is_inferred_from_the_executable_name() {
        for engine in KNOWN_ENGINES {
            let exe = format!("{engine}.exe");
            let config =
                config_from(&exe, &[("TUNTEX_WORKSPACE", env!("CARGO_MANIFEST_DIR"))]).unwrap();
            assert_eq!(config.engine, engine);
        }
    }

    #[test]
    fn unknown_executable_name_is_an_error_not_a_guess() {
        let error = config_from(
            "tuntex-client.exe",
            &[("TUNTEX_WORKSPACE", env!("CARGO_MANIFEST_DIR"))],
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 64);
        assert!(error.message().contains("invalid client executable name"));
    }

    #[test]
    fn workspace_defaults_to_current_directory() {
        let config = config_from("latexmk.exe", &[]).unwrap();
        assert_eq!(config.workspace, std::env::current_dir().unwrap());
    }

    #[test]
    fn relative_workspace_is_rejected() {
        let error =
            config_from("latexmk.exe", &[("TUNTEX_WORKSPACE", "relative/path")]).unwrap_err();
        assert!(error.message().contains("absolute"));
    }

    #[test]
    fn nonexistent_workspace_is_rejected() {
        let error = config_from(
            "latexmk.exe",
            &[("TUNTEX_WORKSPACE", "Z:\\definitely\\not\\here")],
        )
        .unwrap_err();
        assert!(error.message().contains("existing directory"));
    }

    #[test]
    fn cwd_defaults_to_the_workspace() {
        let config = config_from(
            "latexmk.exe",
            &[("TUNTEX_WORKSPACE", env!("CARGO_MANIFEST_DIR"))],
        )
        .unwrap();
        assert_eq!(config.cwd, config.workspace);
    }

    #[test]
    fn defaults_are_applied() {
        let config = config_from(
            "latexmk.exe",
            &[("TUNTEX_WORKSPACE", env!("CARGO_MANIFEST_DIR"))],
        )
        .unwrap();
        assert_eq!(config.url, DEFAULT_URL);
        assert_eq!(config.timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
        assert!(config.token.is_none());
        assert!(!config.debug);
        assert!(config.forward_env.is_empty());
    }

    #[test]
    fn trailing_slash_is_stripped_from_the_url() {
        let config = config_from(
            "latexmk.exe",
            &[
                ("TUNTEX_WORKSPACE", env!("CARGO_MANIFEST_DIR")),
                ("TUNTEX_URL", "http://127.0.0.1:9999/"),
            ],
        )
        .unwrap();
        assert_eq!(config.url, "http://127.0.0.1:9999");
    }

    #[test]
    fn url_without_a_scheme_is_rejected() {
        let error = config_from(
            "latexmk.exe",
            &[
                ("TUNTEX_WORKSPACE", env!("CARGO_MANIFEST_DIR")),
                ("TUNTEX_URL", "127.0.0.1:38117"),
            ],
        )
        .unwrap_err();
        assert!(error.message().contains("http://"));
    }

    #[test]
    fn zero_timeout_is_rejected() {
        let error = config_from(
            "latexmk.exe",
            &[
                ("TUNTEX_WORKSPACE", env!("CARGO_MANIFEST_DIR")),
                ("TUNTEX_TIMEOUT", "0"),
            ],
        )
        .unwrap_err();
        assert!(error.message().contains("greater than zero"));
    }

    #[test]
    fn non_numeric_timeout_is_rejected() {
        let error = config_from(
            "latexmk.exe",
            &[
                ("TUNTEX_WORKSPACE", env!("CARGO_MANIFEST_DIR")),
                ("TUNTEX_TIMEOUT", "soon"),
            ],
        )
        .unwrap_err();
        assert!(error.message().contains("positive integer"));
    }

    #[test]
    fn forward_env_list_accepts_commas_and_semicolons() {
        let config = config_from(
            "latexmk.exe",
            &[
                ("TUNTEX_WORKSPACE", env!("CARGO_MANIFEST_DIR")),
                (
                    "TUNTEX_FORWARD_ENV",
                    "TEXINPUTS, TEXMFHOME;SOURCE_DATE_EPOCH",
                ),
            ],
        )
        .unwrap();
        assert_eq!(
            config.forward_env,
            vec!["TEXINPUTS", "TEXMFHOME", "SOURCE_DATE_EPOCH"]
        );
    }

    #[test]
    fn forwarding_a_system_variable_is_refused() {
        let config = config_from(
            "latexmk.exe",
            &[
                ("TUNTEX_WORKSPACE", env!("CARGO_MANIFEST_DIR")),
                ("TUNTEX_FORWARD_ENV", "TEXINPUTS,PATH"),
            ],
        )
        .unwrap();
        let error = config.collect_forwarded_env().unwrap_err();
        assert!(error.message().contains("PATH"));
    }

    #[test]
    fn unset_forwarded_variables_are_skipped() {
        let config = config_from(
            "latexmk.exe",
            &[
                ("TUNTEX_WORKSPACE", env!("CARGO_MANIFEST_DIR")),
                ("TUNTEX_FORWARD_ENV", "TUNTEX_TEST_VARIABLE_THAT_IS_NOT_SET"),
            ],
        )
        .unwrap();
        assert!(config.collect_forwarded_env().unwrap().is_empty());
    }
}
