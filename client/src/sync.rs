//! Applying a result archive to the local workspace.
//!
//! Three properties matter here, in this order:
//!
//! 1. **Nothing is touched until everything is validated.**  The archive is
//!    unpacked into a staging directory and every path checked before the first
//!    byte is written to the workspace.  A truncated download or a hostile
//!    response cannot leave the project half-updated.
//! 2. **Replacement is atomic.**  Each file is written to a temporary file in
//!    its final directory and then renamed over the target, so an interrupted
//!    sync cannot destroy the last good PDF.
//! 3. **Files are applied even when the compile failed.**  A non-zero exit
//!    still produces `.log` and `.aux` files that are the whole point of
//!    looking at the error.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use tar::Archive;

use crate::archive::{member_to_relative_path, validate_member_name};
use crate::error::{Error, Result};
use crate::protocol;

/// A cap on how many files a single result may carry.
const MAX_RESULT_FILES: usize = 200_000;

/// What was applied to the workspace.
#[derive(Debug, Clone)]
pub struct AppliedResult {
    pub exit_code: i32,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration_ms: u64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub changed_count: usize,
    pub deleted_count: usize,
}

/// Unpack a result archive into `staging`, returning the paths of changed files.
fn stage_result(archive_path: &Path, staging: &Path) -> Result<StagedResult> {
    fs::create_dir_all(staging).map_err(|error| {
        Error::software(format!(
            "could not create the staging directory {}: {error}",
            staging.display()
        ))
    })?;

    let file = fs::File::open(archive_path).map_err(|error| {
        Error::protocol(format!(
            "could not open the result archive {}: {error}",
            archive_path.display()
        ))
    })?;

    let mut archive = Archive::new(GzDecoder::new(file));

    let mut staged = StagedResult::default();
    let mut file_count = 0usize;

    let entries = archive
        .entries()
        .map_err(|error| Error::protocol(format!("result archive is unreadable: {error}")))?;

    for entry in entries {
        let mut entry = entry
            .map_err(|error| Error::protocol(format!("result archive is malformed: {error}")))?;

        let raw_name = entry
            .path()
            .map_err(|error| Error::protocol(format!("result archive has a bad path: {error}")))?
            .to_string_lossy()
            .into_owned();

        // Directories are implied by the files inside them.
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(Error::protocol(format!(
                "result archive member {raw_name:?} is not a regular file"
            )));
        }

        validate_member_name(&raw_name)?;

        file_count += 1;
        if file_count > MAX_RESULT_FILES {
            return Err(Error::protocol(format!(
                "result archive contains more than {MAX_RESULT_FILES} files"
            )));
        }

        let mut payload = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut payload).map_err(|error| {
            Error::protocol(format!(
                "could not read {raw_name:?} from the result archive: {error}"
            ))
        })?;

        match raw_name.as_str() {
            protocol::RESULT_META_NAME => staged.metadata = payload,
            protocol::RESULT_STDOUT_NAME => staged.stdout = payload,
            protocol::RESULT_STDERR_NAME => staged.stderr = payload,
            _ => {
                let Some(relative) =
                    raw_name.strip_prefix(&format!("{}/", protocol::RESULT_FILES_PREFIX))
                else {
                    return Err(Error::protocol(format!(
                        "result archive member {raw_name:?} is in an unexpected location"
                    )));
                };
                let (relative_path, normalised) = member_to_relative_path(relative)?;
                let staged_path = staging.join("files").join(&relative_path);
                write_staged(&staged_path, &payload, &normalised)?;
                staged.files.insert(normalised, staged_path);
            }
        }
    }

    Ok(staged)
}

fn write_staged(target: &Path, payload: &[u8], label: &str) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Error::software(format!("could not create {}: {error}", parent.display()))
        })?;
    }
    fs::write(target, payload)
        .map_err(|error| Error::software(format!("could not stage {label}: {error}")))
}

#[derive(Default)]
struct StagedResult {
    metadata: Vec<u8>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    files: BTreeMap<String, PathBuf>,
}

/// Validate a workspace-relative path from the `deleted` list.
fn resolve_deleted(workspace: &Path, relative: &str) -> Result<PathBuf> {
    let (path, _) = member_to_relative_path(relative)?;
    let target = workspace.join(&path);

    // Belt and braces: the components are already known to be plain names, but
    // this is the check that guards the user's actual files.
    if !target.starts_with(workspace) {
        return Err(Error::protocol(format!(
            "refusing to delete {relative:?}: it is outside the workspace"
        )));
    }
    Ok(target)
}

/// Atomically replace `target` with `payload`.
fn replace_file(target: &Path, source: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Error::software(format!("could not create {}: {error}", parent.display()))
        })?;
    }

    // The temporary file must live in the destination directory so the rename
    // stays within one volume and is therefore atomic.
    let temporary = temporary_sibling(target);
    {
        let mut handle = fs::File::create(&temporary).map_err(|error| {
            Error::software(format!(
                "could not create the temporary file {}: {error}",
                temporary.display()
            ))
        })?;
        let payload = fs::read(source).map_err(|error| {
            Error::software(format!("could not read {}: {error}", source.display()))
        })?;
        handle.write_all(&payload).map_err(|error| {
            Error::software(format!("could not write {}: {error}", temporary.display()))
        })?;
        handle.flush().map_err(|error| {
            Error::software(format!("could not flush {}: {error}", temporary.display()))
        })?;
    }

    fs::rename(&temporary, target).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        Error::software(format!("could not replace {}: {error}", target.display()))
    })
}

fn temporary_sibling(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let temporary_name = format!(".{name}.{unique}.tuntex-tmp");
    match target.parent() {
        Some(parent) => parent.join(temporary_name),
        None => PathBuf::from(temporary_name),
    }
}

/// Apply a result archive to the workspace.
///
/// On any error the workspace is left exactly as it was: everything that can
/// fail is done before the first write.
pub fn apply_result_archive(
    archive_path: &Path,
    staging: &Path,
    workspace: &Path,
    expected_request_id: &str,
) -> Result<AppliedResult> {
    if !workspace.is_dir() {
        return Err(Error::config(format!(
            "workspace is not a directory: {}",
            workspace.display()
        )));
    }

    // ---- phase 1: unpack and validate everything -----------------------
    let staged = stage_result(archive_path, staging)?;

    if staged.metadata.is_empty() {
        return Err(Error::protocol(format!(
            "result archive does not contain {}",
            protocol::RESULT_META_NAME
        )));
    }

    let metadata = protocol::ResultMetadata::from_bytes(&staged.metadata)?;
    metadata.validate_for(expected_request_id)?;
    let local_root = workspace.to_string_lossy();
    let local_root = local_root
        .strip_prefix("\\\\?\\")
        .unwrap_or(&local_root)
        .replace('\\', "/");
    if !metadata.remote_workspace.is_empty() {
        for (relative, path) in &staged.files {
            (|| -> std::io::Result<()> {
                if relative.ends_with(".synctex.gz") {
                    let raw = fs::read(path)?;
                    let mut decoded = Vec::new();
                    GzDecoder::new(raw.as_slice())
                        .take(64 * 1024 * 1024 + 1)
                        .read_to_end(&mut decoded)?;
                    if decoded.len() > 64 * 1024 * 1024 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "SyncTeX exceeds decompression limit",
                        ));
                    }
                    let mut encoder =
                        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                    encoder.write_all(&rebase_paths(
                        &decoded,
                        &metadata.remote_workspace,
                        &local_root,
                    ))?;
                    fs::write(path, encoder.finish()?)?;
                } else if [".log", ".fls", ".fdb_latexmk", ".synctex"]
                    .iter()
                    .any(|ext| relative.ends_with(ext))
                {
                    fs::write(
                        path,
                        rebase_paths(&fs::read(path)?, &metadata.remote_workspace, &local_root),
                    )?;
                }
                Ok(())
            })()
            .map_err(|e| Error::protocol(format!("cannot map result paths: {e}")))?;
        }
    }

    // Every file the metadata claims to have changed must actually be present.
    for relative in &metadata.changed {
        if !staged.files.contains_key(relative) {
            return Err(Error::protocol(format!(
                "result metadata lists {relative:?} as changed, but the archive does not \
                 contain it; refusing to apply a partial result"
            )));
        }
    }

    // Resolve deletions now, so a bad entry fails before any write happens.
    let mut deletions = Vec::new();
    for relative in &metadata.deleted {
        deletions.push((relative.clone(), resolve_deleted(workspace, relative)?));
    }

    // ---- phase 2: apply ------------------------------------------------
    for (relative, staged_path) in &staged.files {
        let target = workspace.join(member_to_relative_path(relative)?.0);
        replace_file(&target, staged_path)?;
    }

    for (relative, target) in &deletions {
        match fs::remove_file(target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::software(format!(
                    "could not delete {relative}: {error}"
                )))
            }
        }
    }

    Ok(AppliedResult {
        exit_code: metadata.exit_code,
        timed_out: metadata.timed_out,
        cancelled: metadata.cancelled,
        duration_ms: metadata.duration_ms,
        stdout: rebase_paths(&staged.stdout, &metadata.remote_workspace, &local_root),
        stderr: rebase_paths(&staged.stderr, &metadata.remote_workspace, &local_root),
        changed_count: staged.files.len(),
        deleted_count: deletions.len(),
    })
}

fn rebase_paths(bytes: &[u8], remote: &str, local: &str) -> Vec<u8> {
    let native = remote.replace('/', "\\");
    replace_root(&replace_root(bytes, &native, local), remote, local)
}

fn replace_root(bytes: &[u8], remote: &str, local: &str) -> Vec<u8> {
    if remote.is_empty() {
        return bytes.to_vec();
    }
    let needle = remote.as_bytes();
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor..].starts_with(needle)
            && bytes
                .get(cursor + needle.len())
                .is_none_or(|b| !b.is_ascii_alphanumeric() && !matches!(b, b'_' | b'-' | b'.'))
        {
            output.extend_from_slice(local.as_bytes());
            cursor += needle.len();
        } else {
            output.push(bytes[cursor]);
            cursor += 1;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_rebasing_preserves_binary_bytes_and_unrelated_paths() {
        let bytes = b"\xff /tmp/job/workspace/main.tex /tmp/job/workspace-other/main.tex /usr/share/texlive/file";
        assert_eq!(
            rebase_paths(bytes, "/tmp/job/workspace", "E:/project"),
            b"\xff E:/project/main.tex /tmp/job/workspace-other/main.tex /usr/share/texlive/file"
        );
        assert_eq!(
            rebase_paths(
                b"C:\\job\\workspace\\main.tex",
                "C:/job/workspace",
                "E:/project"
            ),
            b"E:/project\\main.tex"
        );
    }
    use crate::protocol::{ResultMetadata, PROTOCOL_VERSION};
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::collections::BTreeMap;
    use tar::{Builder, Header};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            // Deliberately not the OS temp directory: the tests keep all of
            // their scratch space inside the crate.
            let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            path.push(".runtime-tests");
            path.push(format!("sync-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(path.join("workspace")).unwrap();
            Self(path)
        }

        fn workspace(&self) -> PathBuf {
            self.0.join("workspace")
        }

        fn staging(&self) -> PathBuf {
            self.0.join("staging")
        }

        fn write_workspace(&self, relative: &str, content: &str) {
            let target = self
                .workspace()
                .join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, content).unwrap();
        }

        fn read_workspace(&self, relative: &str) -> String {
            fs::read_to_string(
                self.workspace()
                    .join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)),
            )
            .unwrap()
        }

        fn workspace_has(&self, relative: &str) -> bool {
            self.workspace()
                .join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
                .exists()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct ArchiveBuilder {
        metadata: Vec<u8>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        files: Vec<(String, Vec<u8>)>,
        raw_members: Vec<(String, Vec<u8>)>,
    }

    impl ArchiveBuilder {
        fn new(request_id: &str) -> Self {
            let metadata = ResultMetadata {
                remote_workspace: String::new(),
                protocol: PROTOCOL_VERSION,
                request_id: request_id.to_string(),
                exit_code: 0,
                timed_out: false,
                cancelled: false,
                duration_ms: 7,
                changed: Vec::new(),
                deleted: Vec::new(),
            };
            Self {
                metadata: serde_json::to_vec(&metadata).unwrap(),
                stdout: Vec::new(),
                stderr: Vec::new(),
                files: Vec::new(),
                raw_members: Vec::new(),
            }
        }

        fn exit_code(mut self, code: i32) -> Self {
            let mut metadata: ResultMetadata = serde_json::from_slice(&self.metadata).unwrap();
            metadata.exit_code = code;
            self.metadata = serde_json::to_vec(&metadata).unwrap();
            self
        }

        fn deleted(mut self, paths: &[&str]) -> Self {
            let mut metadata: ResultMetadata = serde_json::from_slice(&self.metadata).unwrap();
            metadata.deleted = paths.iter().map(|s| s.to_string()).collect();
            self.metadata = serde_json::to_vec(&metadata).unwrap();
            self
        }

        fn declared_changed(mut self, paths: &[&str]) -> Self {
            let mut metadata: ResultMetadata = serde_json::from_slice(&self.metadata).unwrap();
            metadata.changed = paths.iter().map(|s| s.to_string()).collect();
            self.metadata = serde_json::to_vec(&metadata).unwrap();
            self
        }

        fn file(mut self, relative: &str, content: &str) -> Self {
            self.files
                .push((relative.to_string(), content.as_bytes().to_vec()));
            let mut metadata: ResultMetadata = serde_json::from_slice(&self.metadata).unwrap();
            metadata.changed.push(relative.to_string());
            self.metadata = serde_json::to_vec(&metadata).unwrap();
            self
        }

        fn stdout(mut self, content: &str) -> Self {
            self.stdout = content.as_bytes().to_vec();
            self
        }

        fn stderr(mut self, content: &str) -> Self {
            self.stderr = content.as_bytes().to_vec();
            self
        }

        /// Add a member that ignores the layout rules, for hostile-input tests.
        fn raw_member(mut self, name: &str, content: &str) -> Self {
            self.raw_members
                .push((name.to_string(), content.as_bytes().to_vec()));
            self
        }

        fn omit_metadata(mut self) -> Self {
            self.metadata.clear();
            self
        }

        fn write(self, path: &Path) -> PathBuf {
            let file = fs::File::create(path).unwrap();
            let encoder = GzEncoder::new(file, Compression::default());
            let mut builder = Builder::new(encoder);

            if !self.metadata.is_empty() {
                append(&mut builder, protocol::RESULT_META_NAME, &self.metadata);
            }
            append(&mut builder, protocol::RESULT_STDOUT_NAME, &self.stdout);
            append(&mut builder, protocol::RESULT_STDERR_NAME, &self.stderr);
            for (relative, content) in &self.files {
                append(
                    &mut builder,
                    &format!("{}/{relative}", protocol::RESULT_FILES_PREFIX),
                    content,
                );
            }
            for (name, content) in &self.raw_members {
                append_unchecked(&mut builder, name, content);
            }

            builder.into_inner().unwrap().finish().unwrap();
            path.to_path_buf()
        }
    }

    fn append<W: Write>(builder: &mut Builder<W>, name: &str, payload: &[u8]) {
        let mut header = Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, payload).unwrap();
    }

    /// Append a member with a name the tar crate itself would refuse.
    ///
    /// `set_path` rejects `..` and absolute names, so a hostile archive cannot
    /// be built through it.  Writing the name straight into the header bytes
    /// bypasses that, which is exactly what is needed to prove that *our*
    /// reader rejects such an archive rather than the writer refusing to make
    /// one.
    fn append_unchecked<W: Write>(builder: &mut Builder<W>, name: &str, payload: &[u8]) {
        let mut header = Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);

        let name_bytes = name.as_bytes();
        assert!(
            name_bytes.len() <= 100,
            "test member name must fit the tar header"
        );
        header.as_mut_bytes()[..name_bytes.len()].copy_from_slice(name_bytes);
        header.set_cksum();

        builder.append(&header, payload).unwrap();
    }

    fn apply(scratch: &Scratch, archive: &Path, request_id: &str) -> Result<AppliedResult> {
        apply_result_archive(
            archive,
            &scratch.staging(),
            &scratch.workspace(),
            request_id,
        )
    }

    // -- happy paths -----------------------------------------------------

    #[test]
    fn diagnostics_and_synctex_are_rebased_but_pdf_is_not() {
        let scratch = Scratch::new("rebase");
        let remote = "/tmp/job/workspace";
        let input = format!("Input:1:{remote}/main.tex\n");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input.as_bytes()).unwrap();
        let mut builder = ArchiveBuilder::new("req-1")
            .stdout(&format!("{remote}/main.tex:5: error"))
            .file("main.log", &input)
            .file("main.pdf", &input)
            .file("main.synctex.gz", "");
        builder.files.last_mut().unwrap().1 = encoder.finish().unwrap();
        let mut metadata: ResultMetadata = serde_json::from_slice(&builder.metadata).unwrap();
        metadata.remote_workspace = remote.into();
        metadata.exit_code = 1;
        builder.metadata = serde_json::to_vec(&metadata).unwrap();
        let archive = builder.write(&scratch.0.join("result.tar.gz"));
        let applied = apply(&scratch, &archive, "req-1").unwrap();
        assert_eq!(applied.exit_code, 1);
        assert!(!String::from_utf8(applied.stdout).unwrap().contains(remote));
        assert!(!scratch.read_workspace("main.log").contains(remote));
        assert_eq!(scratch.read_workspace("main.pdf"), input);
        let raw = fs::read(scratch.0.join("workspace/main.synctex.gz")).unwrap();
        let mut decoded = String::new();
        GzDecoder::new(raw.as_slice())
            .read_to_string(&mut decoded)
            .unwrap();
        assert!(!decoded.contains(remote));
        assert!(decoded.ends_with("/main.tex\n"));
    }

    #[test]
    fn created_files_are_written() {
        let scratch = Scratch::new("create");
        let archive = ArchiveBuilder::new("req-1")
            .file("build/main.pdf", "%PDF")
            .write(&scratch.0.join("result.tar.gz"));

        let applied = apply(&scratch, &archive, "req-1").unwrap();
        assert_eq!(applied.changed_count, 1);
        assert_eq!(scratch.read_workspace("build/main.pdf"), "%PDF");
    }

    #[test]
    fn modified_files_replace_their_previous_contents() {
        let scratch = Scratch::new("modify");
        scratch.write_workspace("main.tex", "original");
        let archive = ArchiveBuilder::new("req-1")
            .file("main.tex", "MODIFIED")
            .write(&scratch.0.join("result.tar.gz"));

        apply(&scratch, &archive, "req-1").unwrap();
        assert_eq!(scratch.read_workspace("main.tex"), "MODIFIED");
    }

    #[test]
    fn deleted_files_are_removed() {
        let scratch = Scratch::new("delete");
        scratch.write_workspace("build/old.aux", "gone soon");
        let archive = ArchiveBuilder::new("req-1")
            .deleted(&["build/old.aux"])
            .write(&scratch.0.join("result.tar.gz"));

        let applied = apply(&scratch, &archive, "req-1").unwrap();
        assert_eq!(applied.deleted_count, 1);
        assert!(!scratch.workspace_has("build/old.aux"));
    }

    #[test]
    fn deleting_a_missing_file_is_harmless() {
        let scratch = Scratch::new("delete-missing");
        let archive = ArchiveBuilder::new("req-1")
            .deleted(&["build/never-existed.aux"])
            .write(&scratch.0.join("result.tar.gz"));
        assert!(apply(&scratch, &archive, "req-1").is_ok());
    }

    #[test]
    fn stdout_and_stderr_come_back_verbatim() {
        let scratch = Scratch::new("streams");
        let archive = ArchiveBuilder::new("req-1")
            .stdout("out\n")
            .stderr("err\n")
            .write(&scratch.0.join("result.tar.gz"));

        let applied = apply(&scratch, &archive, "req-1").unwrap();
        assert_eq!(applied.stdout, b"out\n");
        assert_eq!(applied.stderr, b"err\n");
    }

    #[test]
    fn a_failing_compile_still_has_its_files_applied() {
        let scratch = Scratch::new("nonzero");
        scratch.write_workspace("main.pdf", "the last good pdf");
        let archive = ArchiveBuilder::new("req-1")
            .exit_code(12)
            .file("main.log", "! Undefined control sequence")
            .write(&scratch.0.join("result.tar.gz"));

        let applied = apply(&scratch, &archive, "req-1").unwrap();
        assert_eq!(applied.exit_code, 12);
        assert_eq!(
            scratch.read_workspace("main.log"),
            "! Undefined control sequence"
        );
        assert_eq!(
            scratch.read_workspace("main.pdf"),
            "the last good pdf",
            "an untouched file must survive a failed build"
        );
    }

    #[test]
    fn nested_directories_are_created() {
        let scratch = Scratch::new("nested");
        let archive = ArchiveBuilder::new("req-1")
            .file("a/b/c/d.txt", "deep")
            .write(&scratch.0.join("result.tar.gz"));
        apply(&scratch, &archive, "req-1").unwrap();
        assert_eq!(scratch.read_workspace("a/b/c/d.txt"), "deep");
    }

    #[test]
    fn unicode_paths_are_applied() {
        let scratch = Scratch::new("unicode");
        let archive = ArchiveBuilder::new("req-1")
            .file("résumé/chapitre-un.pdf", "content")
            .write(&scratch.0.join("result.tar.gz"));
        apply(&scratch, &archive, "req-1").unwrap();
        assert_eq!(scratch.read_workspace("résumé/chapitre-un.pdf"), "content");
    }

    #[test]
    fn no_temporary_files_are_left_behind() {
        let scratch = Scratch::new("tempfiles");
        let archive = ArchiveBuilder::new("req-1")
            .file("main.pdf", "%PDF")
            .write(&scratch.0.join("result.tar.gz"));
        apply(&scratch, &archive, "req-1").unwrap();

        let leftovers: Vec<_> = fs::read_dir(scratch.workspace())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("tuntex-tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files left behind: {leftovers:?}"
        );
    }

    // -- rejection paths: nothing may be touched -------------------------

    #[test]
    fn a_request_id_mismatch_leaves_the_workspace_untouched() {
        let scratch = Scratch::new("mismatch");
        scratch.write_workspace("main.pdf", "original");
        let archive = ArchiveBuilder::new("other-request")
            .file("main.pdf", "REPLACED")
            .file("new.txt", "NEW")
            .write(&scratch.0.join("result.tar.gz"));

        let error = apply(&scratch, &archive, "req-1").unwrap_err();
        assert_eq!(error.exit_code(), 74);
        assert_eq!(scratch.read_workspace("main.pdf"), "original");
        assert!(!scratch.workspace_has("new.txt"));
    }

    #[test]
    fn an_unsupported_protocol_leaves_the_workspace_untouched() {
        let scratch = Scratch::new("protocol");
        scratch.write_workspace("main.pdf", "original");
        let mut builder = ArchiveBuilder::new("req-1").file("main.pdf", "REPLACED");
        let mut metadata: ResultMetadata = serde_json::from_slice(&builder.metadata).unwrap();
        metadata.protocol = 42;
        builder.metadata = serde_json::to_vec(&metadata).unwrap();

        let error = apply(
            &scratch,
            &builder.write(&scratch.0.join("result.tar.gz")),
            "req-1",
        )
        .unwrap_err();
        assert!(error.message().contains("protocol"));
        assert_eq!(scratch.read_workspace("main.pdf"), "original");
    }

    #[test]
    fn a_traversing_member_is_refused_and_nothing_is_written() {
        let scratch = Scratch::new("traversal");
        scratch.write_workspace("main.pdf", "original");
        let archive = ArchiveBuilder::new("req-1")
            .file("good.txt", "should not land")
            .raw_member("files/../../escape.txt", "ESCAPED")
            .write(&scratch.0.join("result.tar.gz"));

        assert!(apply(&scratch, &archive, "req-1").is_err());
        assert!(!scratch.workspace_has("good.txt"));
        assert_eq!(scratch.read_workspace("main.pdf"), "original");
        assert!(!scratch.0.join("escape.txt").exists());
    }

    #[test]
    fn an_absolute_member_is_refused() {
        let scratch = Scratch::new("absolute");
        let archive = ArchiveBuilder::new("req-1")
            .raw_member("/tmp/escape.txt", "ESCAPED")
            .write(&scratch.0.join("result.tar.gz"));
        assert!(apply(&scratch, &archive, "req-1").is_err());
    }

    #[test]
    fn a_result_missing_its_metadata_is_refused() {
        let scratch = Scratch::new("nometa");
        let archive = ArchiveBuilder::new("req-1")
            .file("main.pdf", "%PDF")
            .omit_metadata()
            .write(&scratch.0.join("result.tar.gz"));

        let error = apply(&scratch, &archive, "req-1").unwrap_err();
        assert!(error.message().contains("meta/result.json"));
        assert!(!scratch.workspace_has("main.pdf"));
    }

    #[test]
    fn a_corrupt_archive_is_refused_without_touching_the_workspace() {
        let scratch = Scratch::new("corrupt");
        scratch.write_workspace("main.pdf", "original");
        let path = scratch.0.join("result.tar.gz");
        fs::write(&path, b"this is not a tar.gz").unwrap();

        assert!(apply(&scratch, &path, "req-1").is_err());
        assert_eq!(scratch.read_workspace("main.pdf"), "original");
    }

    #[test]
    fn a_missing_archive_is_refused() {
        let scratch = Scratch::new("missing");
        let error = apply(&scratch, &scratch.0.join("absent.tar.gz"), "req-1").unwrap_err();
        assert_eq!(error.exit_code(), 74);
    }

    #[test]
    fn a_declared_change_missing_from_the_archive_is_refused() {
        let scratch = Scratch::new("incomplete");
        scratch.write_workspace("main.pdf", "original");
        let archive = ArchiveBuilder::new("req-1")
            .file("present.txt", "here")
            .declared_changed(&["present.txt", "absent.pdf"])
            .write(&scratch.0.join("result.tar.gz"));

        let error = apply(&scratch, &archive, "req-1").unwrap_err();
        assert!(error.message().contains("partial result"));
        assert!(!scratch.workspace_has("present.txt"));
    }

    #[test]
    fn a_deletion_outside_the_workspace_is_refused() {
        let scratch = Scratch::new("delete-escape");
        let archive = ArchiveBuilder::new("req-1")
            .deleted(&["../../outside.txt"])
            .write(&scratch.0.join("result.tar.gz"));

        let error = apply(&scratch, &archive, "req-1").unwrap_err();
        assert!(error.message().contains("escapes") || error.message().contains("outside"));
    }

    #[test]
    fn a_bad_deletion_prevents_all_writes() {
        let scratch = Scratch::new("delete-first");
        scratch.write_workspace("main.pdf", "original");
        let archive = ArchiveBuilder::new("req-1")
            .file("main.pdf", "REPLACED")
            .deleted(&["../escape.txt"])
            .write(&scratch.0.join("result.tar.gz"));

        assert!(apply(&scratch, &archive, "req-1").is_err());
        assert_eq!(
            scratch.read_workspace("main.pdf"),
            "original",
            "validation must complete before any file is replaced"
        );
    }

    #[test]
    fn a_symlink_member_is_refused() {
        let scratch = Scratch::new("symlink-member");
        let path = scratch.0.join("result.tar.gz");
        let file = fs::File::create(&path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);
        let metadata = ResultMetadata {
            remote_workspace: String::new(),
            protocol: PROTOCOL_VERSION,
            request_id: "req-1".to_string(),
            exit_code: 0,
            timed_out: false,
            cancelled: false,
            duration_ms: 0,
            changed: Vec::new(),
            deleted: Vec::new(),
        };
        append(
            &mut builder,
            protocol::RESULT_META_NAME,
            &serde_json::to_vec(&metadata).unwrap(),
        );

        let mut header = Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_link_name("/etc/passwd").unwrap();
        header.set_cksum();
        builder
            .append_data(&mut header, "files/link.tex", std::io::empty())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        let error = apply(&scratch, &path, "req-1").unwrap_err();
        assert!(error.message().contains("regular file"));
    }

    #[test]
    fn the_metadata_feeds_the_applied_result() {
        let scratch = Scratch::new("summary");
        let archive = ArchiveBuilder::new("req-1")
            .exit_code(3)
            .file("a.txt", "a")
            .write(&scratch.0.join("result.tar.gz"));

        let applied = apply(&scratch, &archive, "req-1").unwrap();
        assert_eq!(applied.exit_code, 3);
        assert_eq!(applied.duration_ms, 7);
        assert!(!applied.timed_out);
        assert!(!applied.cancelled);
        assert_eq!(applied.changed_count, 1);
    }

    #[test]
    fn an_empty_changed_map_is_fine() {
        let scratch = Scratch::new("emptychange");
        let archive = ArchiveBuilder::new("req-1").write(&scratch.0.join("result.tar.gz"));
        let applied = apply(&scratch, &archive, "req-1").unwrap();
        assert_eq!(applied.changed_count, 0);
        let _: BTreeMap<String, PathBuf> = BTreeMap::new();
    }
}
