//! tunTeX client: a Windows executable that behaves like a local LaTeX compiler.
//!
//! The whole point is that the caller cannot tell the difference.  A build tool
//! invokes this binary with the same arguments it would pass to `latexmk`; the
//! workspace goes to a remote service, the compile happens there, and the
//! products, output, and exit code come back.
//!
//! The ordering in [`execute`] is not arbitrary -- see the comment there.

pub mod archive;
pub mod config;
pub mod error;
pub mod pathmap;
pub mod protocol;
pub mod remote;
pub mod signal;
pub mod sync;

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use config::Config;
use error::{Error, Result};
use pathmap::PathMapper;
use protocol::RequestMetadata;
use remote::Remote;
use uuid::Uuid;

/// Temporary directory removed on drop, unless `KEEP_TEMP` is set.
struct Scratch {
    path: PathBuf,
    keep: bool,
}

impl Scratch {
    fn create(request_id: &str, keep: bool) -> Result<Self> {
        let path = std::env::temp_dir().join("tuntex").join(request_id);
        fs::create_dir_all(&path).map_err(|error| {
            Error::software(format!(
                "could not create the temporary directory {}: {error}",
                path.display()
            ))
        })?;
        Ok(Self { path, keep })
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Run the proxy with the process's own arguments, returning an exit code.
pub fn run() -> i32 {
    let args: Vec<OsString> = std::env::args_os().collect();
    let debug = false;

    match execute(args) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            let mut stderr = io::stderr();
            let _ = report(&error, &mut stderr, debug);
            error.exit_code()
        }
    }
}

/// Print an error the way the protocol documents it.
pub fn report(error: &Error, out: &mut impl Write, debug: bool) -> io::Result<()> {
    writeln!(out, "tuntex: {}", error.message())?;
    if debug {
        for link in error.chain() {
            writeln!(out, "tuntex:   caused by: {link}")?;
        }
    }
    out.flush()
}

/// Map a single LaTeX argument onto the protocol's virtual paths.
///
/// Exposed so tests can cover the mapping without launching anything.
pub fn map_arguments(mapper: &PathMapper, arguments: &[OsString]) -> Result<Vec<String>> {
    arguments
        .iter()
        .map(|argument| mapper.map_argument(argument))
        .collect()
}

struct DebugLog {
    enabled: bool,
}

impl DebugLog {
    fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    fn line(&self, message: &str) {
        if self.enabled {
            eprintln!("[tuntex] {message}");
        }
    }
}

/// Perform one proxied compile.
///
/// `args` is the full argv including argv[0]; everything after it is the LaTeX
/// command line and is forwarded verbatim (after path mapping).
pub fn execute<I>(args: I) -> Result<i32>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let executable = args.next().unwrap_or_default();
    let latex_argv: Vec<OsString> = args.collect();

    let config = Config::from_env(&executable)?;
    let log = DebugLog::new(config.debug);

    let mapper = PathMapper::new(&config.workspace)?;
    if !mapper.contains(&config.cwd) {
        return Err(Error::config(format!(
            "TUNTEX_CWD is outside TUNTEX_WORKSPACE:\n  \
             cwd:       {}\n  \
             workspace: {}",
            config.cwd.display(),
            config.workspace.display()
        )));
    }

    let remote_argv = map_arguments(&mapper, &latex_argv)?;
    let remote_cwd = mapper.map_path(&config.cwd)?;
    let forwarded_env = config.collect_forwarded_env()?;

    let request_id = Uuid::new_v4().to_string();
    let scratch = Scratch::create(&request_id, config.keep_temp)?;

    log.line(&format!("request: {request_id}"));
    log.line(&format!("engine: {}", config.engine));
    log.line(&format!("workspace: {}", config.workspace.display()));
    log.line(&format!("cwd: {}", config.cwd.display()));
    log.line(&format!("url: {}", config.url));

    // From here on the caller may interrupt; make the remote job stoppable.
    signal::install_handler(config.url.clone(), config.token.clone(), request_id.clone())?;

    let metadata = RequestMetadata::new(
        request_id.clone(),
        config.engine.clone(),
        remote_argv,
        remote_cwd,
        forwarded_env,
        config.timeout_seconds,
    );

    let rules = archive::IgnoreRules::load(&config.workspace)?;
    let request_path = scratch.join("request.tar.gz");
    let report = archive::create_request_archive(
        &request_path,
        &config.workspace,
        &metadata.to_bytes()?,
        &rules,
        archive::Limits {
            max_file_count: config.max_file_count,
            max_upload_size: config.max_upload_size,
        },
    )?;

    log.line(&format!("files: {}", report.file_count));
    log.line(&format!(
        "upload size: {}",
        archive::human_size(report.total_bytes)
    ));

    let remote = Remote::new(&config.url, config.token.clone(), config.timeout_seconds)?;
    let result_path = scratch.join("result.tar.gz");
    let received = remote.compile(&request_path, &result_path, &request_id)?;
    log.line(&format!("result size: {}", archive::human_size(received)));

    let staging = scratch.join("staging");
    let applied =
        sync::apply_result_archive(&result_path, &staging, &config.workspace, &request_id)?;

    log.line(&format!("remote duration: {}ms", applied.duration_ms));
    log.line(&format!("changed files: {}", applied.changed_count));
    if applied.timed_out {
        log.line("the remote build timed out");
    }
    if applied.cancelled {
        log.line("the remote build was cancelled");
    }

    // Output and exit code are applied even when the compile failed: the log
    // and auxiliary files from a failed run are the reason to look at all.
    write_stream(&mut io::stdout(), &applied.stdout)?;
    write_stream(&mut io::stderr(), &applied.stderr)?;

    Ok(applied.exit_code)
}

fn write_stream(out: &mut impl Write, payload: &[u8]) -> Result<()> {
    if payload.is_empty() {
        return Ok(());
    }
    // Copied verbatim: TeX output is not guaranteed to be valid UTF-8, and
    // re-encoding it would corrupt the bytes the caller is inspecting.
    out.write_all(payload)
        .and_then(|()| out.flush())
        .map_err(|error| Error::software(format!("could not write to the output stream: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_arguments_rewrites_only_paths() {
        let mapper = PathMapper::new(std::path::Path::new("C:\\repo\\paper")).unwrap();
        let arguments = vec![
            OsString::from("-synctex=1"),
            OsString::from("-outdir=C:\\repo\\paper\\build"),
            OsString::from("C:\\repo\\paper\\main.tex"),
        ];
        assert_eq!(
            map_arguments(&mapper, &arguments).unwrap(),
            vec![
                "-synctex=1",
                "-outdir=/workspace/build",
                "/workspace/main.tex"
            ]
        );
    }

    #[test]
    fn a_path_outside_the_workspace_fails_the_whole_mapping() {
        let mapper = PathMapper::new(std::path::Path::new("C:\\repo\\paper")).unwrap();
        let arguments = vec![OsString::from("D:\\shared\\refs.bib")];
        let error = map_arguments(&mapper, &arguments).unwrap_err();
        assert_eq!(error.exit_code(), 64);
    }

    #[test]
    fn reporting_writes_one_line_per_error() {
        let error = Error::config("something went wrong");
        let mut buffer = Vec::new();
        report(&error, &mut buffer, false).unwrap();
        assert_eq!(
            String::from_utf8(buffer).unwrap(),
            "tuntex: something went wrong\n"
        );
    }

    #[test]
    fn empty_output_streams_are_not_written() {
        let mut buffer = Vec::new();
        write_stream(&mut buffer, b"").unwrap();
        assert!(buffer.is_empty());
    }

    #[test]
    fn non_utf8_output_is_written_byte_for_byte() {
        let mut buffer = Vec::new();
        write_stream(&mut buffer, b"\xff\xfe\x00raw").unwrap();
        assert_eq!(buffer, b"\xff\xfe\x00raw");
    }
}
