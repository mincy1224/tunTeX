//! Building the request archive, and the path rules shared with extraction.
//!
//! The whole workspace is uploaded, not just the entry `.tex` file: `\input`
//! chains, figures, custom `.sty`/`.cls`, and the previous run's `.aux`/`.fls`
//! files all have to be there for `latexmk` to build incrementally.
//!
//! What is *not* uploaded by default is the version-control metadata.  Note
//! that build products are deliberately kept -- a `.gitignore` is a poor
//! upload filter for exactly this reason.

use std::fs::File;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;
use tar::{Builder, Header};

use crate::error::{Error, Result};
use crate::protocol;

/// Name of the optional ignore file, read from the workspace root.
pub const IGNORE_FILE_NAME: &str = ".tuntexignore";

/// Directories excluded when no ignore file says otherwise.
pub const DEFAULT_IGNORES: [&str; 4] = [".git/", ".svn/", ".hg/", ".tuntex/"];

/// Upload limits enforced before anything is sent.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_file_count: usize,
    pub max_upload_size: u64,
}

/// What a completed archive contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveReport {
    pub file_count: usize,
    pub total_bytes: u64,
}

/// A single ignore pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    text: String,
    /// A trailing `/` restricts the pattern to directories.
    directory_only: bool,
}

impl Pattern {
    fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }
        let directory_only = trimmed.ends_with('/');
        let text = trimmed.trim_end_matches('/').to_string();
        if text.is_empty() {
            return None;
        }
        Some(Self {
            text,
            directory_only,
        })
    }

    /// Match against a workspace-relative path.
    ///
    /// A pattern containing `/` is matched against the whole relative path;
    /// otherwise it is matched against the final component, which is the
    /// behaviour people expect from a short entry like `node_modules/`.
    fn matches(&self, relative: &str, is_dir: bool) -> bool {
        if self.directory_only && !is_dir {
            return false;
        }
        if self.text.contains('/') {
            return glob_match(&self.text, relative);
        }
        relative
            .split('/')
            .any(|segment| glob_match(&self.text, segment))
    }
}

/// The ignore rules in force for one workspace.
#[derive(Debug, Clone)]
pub struct IgnoreRules {
    patterns: Vec<Pattern>,
}

impl Default for IgnoreRules {
    fn default() -> Self {
        Self::builtin()
    }
}

impl IgnoreRules {
    /// Just the built-in defaults, with no ignore file.
    pub fn builtin() -> Self {
        let patterns = DEFAULT_IGNORES
            .iter()
            .filter_map(|raw| Pattern::parse(raw))
            .collect();
        Self { patterns }
    }

    /// Built-in defaults plus the workspace's `.tuntexignore`, if present.
    pub fn load(workspace: &Path) -> Result<Self> {
        let mut rules = Self::builtin();
        let path = workspace.join(IGNORE_FILE_NAME);
        if !path.is_file() {
            return Ok(rules);
        }
        let contents = std::fs::read_to_string(&path).map_err(|error| {
            Error::config(format!("could not read {}: {error}", path.display()))
        })?;
        rules
            .patterns
            .extend(contents.lines().filter_map(Pattern::parse));
        Ok(rules)
    }

    /// True when `relative` should be left out of the upload.
    pub fn is_ignored(&self, relative: &str, is_dir: bool) -> bool {
        self.patterns
            .iter()
            .any(|pattern| pattern.matches(relative, is_dir))
    }

    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

/// Match `text` against a pattern supporting `*` and `?`.
///
/// `*` matches any run of characters (including `/`, so `build/*` covers
/// nested output); `?` matches exactly one character.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();

    // Iterative backtracking: linear in practice, no recursion depth limit.
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Validate a member name used inside an archive.
///
/// Applied when writing *and* when reading: the client must not create an
/// unsafe entry, and must not trust one that arrives.
pub fn validate_member_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::protocol("archive member has an empty name"));
    }
    if name.contains('\0') {
        return Err(Error::protocol("archive member contains a NUL byte"));
    }
    if name.contains('\\') {
        return Err(Error::protocol(format!(
            "archive member {name:?} contains a backslash; protocol paths use '/'"
        )));
    }
    if name.starts_with('/') {
        return Err(Error::protocol(format!(
            "archive member {name:?} is absolute"
        )));
    }
    if is_drive_prefix(name) {
        return Err(Error::protocol(format!(
            "archive member {name:?} contains a Windows drive prefix"
        )));
    }
    for segment in name.split('/') {
        if segment == ".." {
            return Err(Error::protocol(format!(
                "archive member {name:?} escapes its root via '..'"
            )));
        }
    }
    Ok(())
}

fn is_drive_prefix(name: &str) -> bool {
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(letter), Some(':')) => letter.is_ascii_alphabetic(),
        _ => false,
    }
}

/// Convert an archive member name into a safe relative path.
///
/// Returns the path and its forward-slash form.  Rejects absolute names,
/// traversal, and non-Unicode components.
pub fn member_to_relative_path(name: &str) -> Result<(PathBuf, String)> {
    validate_member_name(name)?;

    let mut path = PathBuf::new();
    let mut normalised: Vec<&str> = Vec::new();
    for segment in name.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return Err(Error::protocol(format!(
                "archive member {name:?} escapes its root via '..'"
            )));
        }
        path.push(segment);
        normalised.push(segment);
    }

    if normalised.is_empty() {
        return Err(Error::protocol(format!(
            "archive member {name:?} has no usable path components"
        )));
    }

    // A relative path built from validated segments cannot escape, but assert
    // it anyway: this is the check that protects the user's workspace.
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(Error::protocol(format!(
                "archive member {name:?} is not a plain relative path"
            )));
        }
    }

    Ok((path, normalised.join("/")))
}

/// One file selected for upload.
#[derive(Debug, Clone)]
struct Entry {
    absolute: PathBuf,
    relative: String,
    size: u64,
}

impl Entry {
    /// The name this file gets inside the archive.
    fn member_name(&self) -> String {
        format!("{}/{}", protocol::REQUEST_WORKSPACE_PREFIX, self.relative)
    }
}

/// Walk the workspace, applying ignore rules and the size limits.
///
/// The limits are checked here, before anything is written, so an enormous
/// workspace fails immediately instead of half-uploading.
fn walk_workspace(workspace: &Path, rules: &IgnoreRules, limits: Limits) -> Result<Vec<Entry>> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut total_bytes: u64 = 0;

    let mut stack = vec![(workspace.to_path_buf(), String::new())];
    while let Some((directory, prefix)) = stack.pop() {
        let read_dir = std::fs::read_dir(&directory).map_err(|error| {
            Error::config(format!("could not read {}: {error}", directory.display()))
        })?;

        for entry in read_dir {
            let entry = entry.map_err(|error| {
                Error::config(format!("could not read {}: {error}", directory.display()))
            })?;

            let file_name = entry.file_name();
            let relative = if prefix.is_empty() {
                file_name.to_string_lossy().into_owned()
            } else {
                format!("{prefix}/{}", file_name.to_string_lossy())
            };

            let file_type = entry.file_type().map_err(|error| {
                Error::config(format!(
                    "could not stat {}: {error}",
                    entry.path().display()
                ))
            })?;

            if file_type.is_symlink() {
                return Err(Error::config(format!(
                    "{} is a symbolic link, which cannot be uploaded safely:\n  {}\n\
                     remove the link, or move its target into the workspace",
                    relative,
                    entry.path().display()
                )));
            }

            if file_type.is_dir() {
                if rules.is_ignored(&relative, true) {
                    continue;
                }
                validate_member_name(&relative)?;
                stack.push((entry.path(), relative));
                continue;
            }

            if !file_type.is_file() {
                continue; // sockets, fifos, devices: not uploadable
            }

            if rules.is_ignored(&relative, false) {
                continue;
            }

            // The protocol requires every name to survive as UTF-8.
            if file_name.to_str().is_none() {
                return Err(Error::config(format!(
                    "file name is not valid Unicode and cannot be sent over the protocol: {}",
                    entry.path().display()
                )));
            }
            validate_member_name(&relative)?;

            let size = entry
                .metadata()
                .map_err(|error| {
                    Error::config(format!(
                        "could not stat {}: {error}",
                        entry.path().display()
                    ))
                })?
                .len();

            total_bytes = total_bytes.saturating_add(size);
            if total_bytes > limits.max_upload_size {
                return Err(Error::config(format!(
                    "upload exceeds the configured {} limit (already {total_bytes} bytes at {relative})\n\
                     raise TUNTEX_MAX_UPLOAD_SIZE, or exclude files with {IGNORE_FILE_NAME}",
                    human_size(limits.max_upload_size)
                )));
            }
            if entries.len() + 1 > limits.max_file_count {
                return Err(Error::config(format!(
                    "workspace contains more than the configured {} files\n\
                     raise TUNTEX_MAX_FILE_COUNT, or exclude files with {IGNORE_FILE_NAME}",
                    limits.max_file_count
                )));
            }

            entries.push(Entry {
                absolute: entry.path(),
                relative,
                size,
            });
        }
    }

    entries.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(entries)
}

/// Write the request archive: `meta/request.json` plus the whole workspace.
pub fn create_request_archive(
    destination: &Path,
    workspace: &Path,
    metadata_bytes: &[u8],
    rules: &IgnoreRules,
    limits: Limits,
) -> Result<ArchiveReport> {
    let entries = walk_workspace(workspace, rules, limits)?;

    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::software(format!(
                "could not create the temporary directory {}: {error}",
                parent.display()
            ))
        })?;
    }

    let file = File::create(destination).map_err(|error| {
        Error::software(format!(
            "could not create the request archive {}: {error}",
            destination.display()
        ))
    })?;

    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = Builder::new(encoder);

    append_bytes(&mut builder, protocol::REQUEST_META_NAME, metadata_bytes)?;

    let mut total_bytes = 0u64;
    for entry in &entries {
        append_file(&mut builder, entry)?;
        total_bytes += entry.size;
    }

    let encoder = builder.into_inner().map_err(io_error)?;
    encoder.finish().map_err(io_error)?;

    Ok(ArchiveReport {
        file_count: entries.len(),
        total_bytes,
    })
}

fn io_error(error: std::io::Error) -> Error {
    Error::software(format!("archive I/O failed: {error}"))
}

fn append_bytes<W: Write>(builder: &mut Builder<W>, name: &str, payload: &[u8]) -> Result<()> {
    validate_member_name(name)?;
    let mut header = Header::new_gnu();
    header.set_size(payload.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_data(&mut header, name, payload)
        .map_err(io_error)
}

fn append_file<W: Write>(builder: &mut Builder<W>, entry: &Entry) -> Result<()> {
    let mut source = File::open(&entry.absolute).map_err(|error| {
        Error::config(format!(
            "could not open {}: {error}",
            entry.absolute.display()
        ))
    })?;

    let mut header = Header::new_gnu();
    header.set_size(entry.size);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();

    builder
        .append_data(&mut header, entry.member_name(), &mut source)
        .map_err(io_error)
}

/// Render a byte count the way the debug output does.
pub fn human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KIB {
        format!("{bytes} B")
    } else if value < KIB * KIB {
        format!("{:.1} KiB", value / KIB)
    } else if value < KIB * KIB * KIB {
        format!("{:.1} MiB", value / (KIB * KIB))
    } else {
        format!("{:.2} GiB", value / (KIB * KIB * KIB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Read;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            // Deliberately not the OS temp directory: the tests keep all of
            // their scratch space inside the crate.
            let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            path.push(".runtime-tests");
            path.push(format!("{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, content: &str) -> PathBuf {
            let target = self
                .0
                .join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&target, content).unwrap();
            target
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn generous_limits() -> Limits {
        Limits {
            max_file_count: 1000,
            max_upload_size: 64 * 1024 * 1024,
        }
    }

    fn archive_members(path: &Path) -> Vec<String> {
        let file = File::open(path).unwrap();
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .path()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    fn extract_member(path: &Path, name: &str) -> Option<String> {
        let file = File::open(path).unwrap();
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().to_string_lossy() == name {
                let mut buffer = String::new();
                entry.read_to_string(&mut buffer).unwrap();
                return Some(buffer);
            }
        }
        None
    }

    fn metadata_bytes() -> Vec<u8> {
        let metadata = crate::protocol::RequestMetadata::new(
            "00000000-0000-4000-8000-000000000000",
            "latexmk",
            vec!["/workspace/main.tex".to_string()],
            "/workspace",
            BTreeMap::new(),
            60,
        );
        metadata.to_bytes().unwrap()
    }

    // -- glob ------------------------------------------------------------

    #[test]
    fn glob_matches_literals() {
        assert!(glob_match("build", "build"));
        assert!(!glob_match("build", "builds"));
    }

    #[test]
    fn glob_star_matches_any_run_including_empty() {
        assert!(glob_match("*.log", "main.log"));
        assert!(glob_match("*.log", ".log"));
        assert!(!glob_match("*.log", "main.log.bak"));
    }

    #[test]
    fn glob_star_crosses_directory_separators() {
        assert!(glob_match("build/*", "build/a/b.pdf"));
    }

    #[test]
    fn glob_question_mark_matches_one_character() {
        assert!(glob_match("f?.tex", "f1.tex"));
        assert!(!glob_match("f?.tex", "f12.tex"));
    }

    #[test]
    fn glob_star_at_the_end_matches_a_prefix() {
        assert!(glob_match("node_modules*", "node_modules_old"));
    }

    // -- ignore rules ----------------------------------------------------

    #[test]
    fn defaults_exclude_version_control_directories() {
        let rules = IgnoreRules::builtin();
        assert!(rules.is_ignored(".git", true));
        assert!(rules.is_ignored(".svn", true));
        assert!(rules.is_ignored(".hg", true));
        assert!(rules.is_ignored(".tuntex", true));
    }

    #[test]
    fn build_products_are_not_ignored_by_default() {
        let rules = IgnoreRules::builtin();
        assert!(!rules.is_ignored("main.aux", false));
        assert!(!rules.is_ignored("main.fls", false));
        assert!(!rules.is_ignored("build", true));
        assert!(!rules.is_ignored("build/main.pdf", false));
    }

    #[test]
    fn gitignore_is_not_treated_as_an_upload_filter() {
        let scratch = Scratch::new("gitignore");
        scratch.write(".gitignore", "build/\n*.aux\n");
        let rules = IgnoreRules::load(scratch.path()).unwrap();
        assert!(!rules.is_ignored("build", true));
        assert!(!rules.is_ignored("main.aux", false));
    }

    #[test]
    fn tuntexignore_is_honoured() {
        let scratch = Scratch::new("ignorefile");
        scratch.write(IGNORE_FILE_NAME, "# comment\n\nnode_modules/\n*.tmp\n");
        let rules = IgnoreRules::load(scratch.path()).unwrap();
        assert!(rules.is_ignored("node_modules", true));
        assert!(rules.is_ignored("scratch.tmp", false));
        assert!(!rules.is_ignored("main.tex", false));
    }

    #[test]
    fn a_directory_only_pattern_does_not_match_a_file() {
        let rules = IgnoreRules {
            patterns: vec![Pattern::parse("build/").unwrap()],
        };
        assert!(rules.is_ignored("build", true));
        assert!(!rules.is_ignored("build", false));
    }

    #[test]
    fn a_pattern_with_a_slash_matches_the_relative_path() {
        let rules = IgnoreRules {
            patterns: vec![Pattern::parse("data/large").unwrap()],
        };
        assert!(rules.is_ignored("data/large", true));
        assert!(!rules.is_ignored("other/large", true));
    }

    #[test]
    fn a_short_pattern_matches_any_component() {
        let rules = IgnoreRules {
            patterns: vec![Pattern::parse("node_modules").unwrap()],
        };
        assert!(rules.is_ignored("node_modules", true));
        assert!(rules.is_ignored("packages/app/node_modules", true));
    }

    // -- member name validation ------------------------------------------

    #[test]
    fn ordinary_member_names_are_accepted() {
        assert!(validate_member_name("meta/request.json").is_ok());
        assert!(validate_member_name("workspace/main.tex").is_ok());
        assert!(validate_member_name("workspace/résumé/chapitre-un.tex").is_ok());
        assert!(validate_member_name("workspace/My Paper/main.tex").is_ok());
    }

    #[test]
    fn traversal_member_names_are_rejected() {
        assert!(validate_member_name("../escape").is_err());
        assert!(validate_member_name("workspace/../../escape").is_err());
    }

    #[test]
    fn absolute_member_names_are_rejected() {
        assert!(validate_member_name("/etc/passwd").is_err());
        assert!(validate_member_name("C:/Windows/system32").is_err());
    }

    #[test]
    fn backslash_member_names_are_rejected() {
        assert!(validate_member_name("workspace\\main.tex").is_err());
    }

    #[test]
    fn member_to_relative_path_normalises_dot_segments() {
        let (path, text) = member_to_relative_path("./workspace/./a/b.tex").unwrap();
        assert_eq!(text, "workspace/a/b.tex");
        assert!(path.ends_with("b.tex"));
    }

    #[test]
    fn member_to_relative_path_rejects_escapes() {
        assert!(member_to_relative_path("../../etc/passwd").is_err());
        assert!(member_to_relative_path("").is_err());
    }

    // -- archive creation ------------------------------------------------

    #[test]
    fn the_archive_contains_metadata_and_the_workspace() {
        let scratch = Scratch::new("create");
        scratch.write("main.tex", "hello");
        scratch.write("chapters/intro.tex", "chapter");

        let destination = scratch.path().join("request.tar.gz");
        let report = create_request_archive(
            &destination,
            scratch.path(),
            &metadata_bytes(),
            &IgnoreRules::builtin(),
            generous_limits(),
        )
        .unwrap();

        assert_eq!(report.file_count, 2);
        let members = archive_members(&destination);
        assert!(members.contains(&"meta/request.json".to_string()));
        assert!(members.contains(&"workspace/main.tex".to_string()));
        assert!(members.contains(&"workspace/chapters/intro.tex".to_string()));
    }

    #[test]
    fn file_contents_survive_the_round_trip() {
        let scratch = Scratch::new("contents");
        scratch.write("main.tex", "\\documentclass{article}");
        let destination = scratch.path().join("request.tar.gz");
        create_request_archive(
            &destination,
            scratch.path(),
            &metadata_bytes(),
            &IgnoreRules::builtin(),
            generous_limits(),
        )
        .unwrap();

        assert_eq!(
            extract_member(&destination, "workspace/main.tex").unwrap(),
            "\\documentclass{article}"
        );
    }

    #[test]
    fn empty_files_are_included() {
        let scratch = Scratch::new("empty");
        scratch.write("empty.tex", "");
        let destination = scratch.path().join("request.tar.gz");
        let report = create_request_archive(
            &destination,
            scratch.path(),
            &metadata_bytes(),
            &IgnoreRules::builtin(),
            generous_limits(),
        )
        .unwrap();
        assert_eq!(report.file_count, 1);
        assert_eq!(
            extract_member(&destination, "workspace/empty.tex").unwrap(),
            ""
        );
    }

    #[test]
    fn unicode_and_spaced_names_are_included() {
        let scratch = Scratch::new("unicode");
        scratch.write("résumé draft/chapitre-un.tex", "content");
        let destination = scratch.path().join("request.tar.gz");
        create_request_archive(
            &destination,
            scratch.path(),
            &metadata_bytes(),
            &IgnoreRules::builtin(),
            generous_limits(),
        )
        .unwrap();
        let members = archive_members(&destination);
        assert!(members.iter().any(|name| name.contains("résumé draft")));
    }

    #[test]
    fn ignored_directories_are_left_out() {
        let scratch = Scratch::new("ignored");
        scratch.write("main.tex", "keep");
        scratch.write(".git/config", "should not be uploaded");
        let destination = scratch.path().join("request.tar.gz");
        let report = create_request_archive(
            &destination,
            scratch.path(),
            &metadata_bytes(),
            &IgnoreRules::builtin(),
            generous_limits(),
        )
        .unwrap();

        assert_eq!(report.file_count, 1);
        let members = archive_members(&destination);
        assert!(!members.iter().any(|name| name.contains(".git")));
    }

    #[test]
    fn the_file_count_limit_is_enforced() {
        let scratch = Scratch::new("count");
        for index in 0..10 {
            scratch.write(&format!("f{index}.tex"), "x");
        }
        let error = create_request_archive(
            &scratch.path().join("request.tar.gz"),
            scratch.path(),
            &metadata_bytes(),
            &IgnoreRules::builtin(),
            Limits {
                max_file_count: 3,
                max_upload_size: u64::MAX,
            },
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 64);
        assert!(error.message().contains("TUNTEX_MAX_FILE_COUNT"));
    }

    #[test]
    fn the_upload_size_limit_is_enforced() {
        let scratch = Scratch::new("size");
        scratch.write("big.tex", &"x".repeat(5000));
        let error = create_request_archive(
            &scratch.path().join("request.tar.gz"),
            scratch.path(),
            &metadata_bytes(),
            &IgnoreRules::builtin(),
            Limits {
                max_file_count: 100,
                max_upload_size: 1000,
            },
        )
        .unwrap_err();
        assert!(error.message().contains("TUNTEX_MAX_UPLOAD_SIZE"));
    }

    #[test]
    fn human_size_is_readable() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn a_symlinked_file_is_refused() {
        let scratch = Scratch::new("symlink");
        let target = scratch.write("real.tex", "content");
        let link = scratch.path().join("link.tex");
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            return; // symlink creation needs privileges; nothing to prove here
        }

        let error = create_request_archive(
            &scratch.path().join("request.tar.gz"),
            scratch.path(),
            &metadata_bytes(),
            &IgnoreRules::builtin(),
            generous_limits(),
        )
        .unwrap_err();
        assert!(error.message().contains("symbolic link"));
    }
}
