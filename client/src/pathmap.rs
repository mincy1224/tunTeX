//! Mapping Windows paths onto the protocol's single virtual root.
//!
//! The server never sees a Windows path, and it never sees the real temporary
//! directory either.  Everything is expressed relative to `/workspace`.
//!
//! Two rules keep this honest:
//!
//! * A path outside the workspace is an **error**, never a guess.  Silently
//!   rewriting it to something that happens to exist remotely would turn a
//!   configuration mistake into a mysteriously wrong build.
//! * Only the path-shaped part of an argument is rewritten.  `-synctex=1` and
//!   `-interaction=nonstopmode` pass through byte-for-byte.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// The one virtual root shared with the server.
pub const VIRTUAL_ROOT: &str = "/workspace";

/// Stands in for `RootDir` while normalising, so `..` never pops past it.
const ROOT_SENTINEL: &str = "\\";

/// Maps paths for a single workspace.
#[derive(Debug, Clone)]
pub struct PathMapper {
    workspace: PathBuf,
    segments: Vec<OsString>,
}

impl PathMapper {
    /// Build a mapper for `workspace`, which must be absolute.
    pub fn new(workspace: &Path) -> Result<Self> {
        if !workspace.is_absolute() {
            return Err(Error::config(format!(
                "workspace must be an absolute path: {}",
                workspace.display()
            )));
        }
        let segments = normalise(workspace);
        if segments.is_empty() {
            return Err(Error::config(format!(
                "workspace could not be resolved: {}",
                workspace.display()
            )));
        }
        Ok(Self {
            workspace: workspace.to_path_buf(),
            segments,
        })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// True when `path` lies inside the workspace (or is the workspace itself).
    pub fn contains(&self, path: &Path) -> bool {
        let candidate = normalise(path);
        candidate.len() >= self.segments.len()
            && self
                .segments
                .iter()
                .zip(candidate.iter())
                .all(|(a, b)| segment_eq(a, b))
    }

    /// Convert an argument, rewriting only the part that is a workspace path.
    ///
    /// Handles plain paths, `-key=value` forms, and values wrapped in a single
    /// pair of double quotes.
    pub fn map_argument(&self, argument: &OsStr) -> Result<String> {
        let text = argument.to_str().ok_or_else(|| {
            Error::config(format!(
                "argument is not valid Unicode and cannot be sent over the protocol: {:?}",
                argument
            ))
        })?;

        if text.is_empty() {
            return Ok(String::new());
        }

        if let Some(rest) = text.strip_prefix('-') {
            // A bare flag has no value to rewrite.
            let Some(separator) = rest.find('=') else {
                return Ok(text.to_string());
            };
            // Rebuild from the original so a key containing '=' is preserved.
            let key_end = 1 + separator;
            let (key, value) = text.split_at(key_end + 1);
            let mapped = self.map_value(value)?;
            return Ok(format!("{key}{mapped}"));
        }

        self.map_value(text)
    }

    /// Convert a whole path that must lie inside the workspace.
    pub fn map_path(&self, path: &Path) -> Result<String> {
        let text = path.to_str().ok_or_else(|| {
            Error::config(format!(
                "path is not valid Unicode and cannot be sent over the protocol: {}",
                path.display()
            ))
        })?;
        self.map_rooted(text, path)
    }

    /// Map the value part of an argument.
    fn map_value(&self, value: &str) -> Result<String> {
        if value.is_empty() {
            return Ok(String::new());
        }

        // A single surrounding pair of quotes is presentation, not content.
        let (prefix, inner, suffix) = split_quotes(value);

        if is_path_shaped(inner) {
            let mapped = self.map_rooted(inner, Path::new(inner))?;
            return Ok(format!("{prefix}{mapped}{suffix}"));
        }

        if has_parent_component(inner) {
            return Err(Error::config(format!(
                "argument contains a relative path with '..', which would escape the \
                 uploaded workspace: {value}\n\
                 paths must stay inside TUNTEX_WORKSPACE"
            )));
        }

        Ok(value.to_string())
    }

    /// Map a rooted path, or explain precisely why it cannot be mapped.
    fn map_rooted(&self, text: &str, path: &Path) -> Result<String> {
        let candidate = normalise(path);

        if !has_root(path) {
            return Err(Error::config(format!("expected an absolute path: {text}")));
        }

        if candidate.len() < self.segments.len()
            || !self
                .segments
                .iter()
                .zip(candidate.iter())
                .all(|(a, b)| segment_eq(a, b))
        {
            return Err(Error::config(format!(
                "argument contains a path outside TUNTEX_WORKSPACE:\n  \
                 path:      {text}\n  \
                 workspace: {}\n\
                 move the file into the workspace, or adjust TUNTEX_WORKSPACE",
                self.workspace.display()
            )));
        }

        let remainder = &candidate[self.segments.len()..];
        if remainder.is_empty() {
            return Ok(VIRTUAL_ROOT.to_string());
        }

        let mut virtual_path = String::from(VIRTUAL_ROOT);
        for segment in remainder {
            let segment = segment.to_str().ok_or_else(|| {
                Error::config(format!(
                    "path component is not valid Unicode and cannot be sent over the \
                     protocol: {}",
                    Path::new(segment).display()
                ))
            })?;
            virtual_path.push('/');
            virtual_path.push_str(segment);
        }
        Ok(virtual_path)
    }
}

/// True when the argument carries an absolute or drive-qualified path.
fn is_path_shaped(value: &str) -> bool {
    let path = Path::new(value);
    has_root(path) || starts_with_prefix(path)
}

fn has_root(path: &Path) -> bool {
    path.is_absolute() || path.has_root()
}

fn starts_with_prefix(path: &Path) -> bool {
    matches!(path.components().next(), Some(Component::Prefix(_)))
}

fn has_parent_component(value: &str) -> bool {
    Path::new(value)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
}

/// Split a value into `(opening quote, body, closing quote)`.
fn split_quotes(value: &str) -> (&str, &str, &str) {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        (
            &value[..1],
            &value[1..value.len() - 1],
            &value[value.len() - 1..],
        )
    } else {
        ("", value, "")
    }
}

/// Reduce a path to comparable segments, resolving `.` and `..` lexically.
///
/// Lexical normalisation is deliberate: the target of `-outdir=` usually does
/// not exist yet, so `canonicalize` is not available.
fn normalise(path: &Path) -> Vec<OsString> {
    let mut segments: Vec<OsString> = Vec::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => segments.push(prefix.as_os_str().to_os_string()),
            Component::RootDir => segments.push(OsString::from(ROOT_SENTINEL)),
            Component::CurDir => {}
            Component::ParentDir => {
                // Never pop the root marker: `C:\..\..\x` is still on C:\.
                if matches!(segments.last(), Some(last) if last != OsStr::new(ROOT_SENTINEL)) {
                    segments.pop();
                }
            }
            Component::Normal(name) => segments.push(name.to_os_string()),
        }
    }

    segments
}

/// Compare two path segments.  Windows paths are case-insensitive.
fn segment_eq(left: &OsString, right: &OsString) -> bool {
    left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapper() -> PathMapper {
        PathMapper::new(Path::new("C:\\repo\\paper")).unwrap()
    }

    fn map(argument: &str) -> Result<String> {
        mapper().map_argument(OsStr::new(argument))
    }

    #[test]
    fn the_workspace_root_maps_to_the_virtual_root() {
        assert_eq!(map("C:\\repo\\paper").unwrap(), "/workspace");
    }

    #[test]
    fn backslash_paths_are_mapped() {
        assert_eq!(
            map("C:\\repo\\paper\\main.tex").unwrap(),
            "/workspace/main.tex"
        );
    }

    #[test]
    fn forward_slash_paths_are_mapped() {
        assert_eq!(
            map("C:/repo/paper/main.tex").unwrap(),
            "/workspace/main.tex"
        );
    }

    #[test]
    fn mapping_is_case_insensitive_like_windows() {
        assert_eq!(
            map("c:\\REPO\\Paper\\Main.TeX").unwrap(),
            "/workspace/Main.TeX",
            "the workspace prefix is matched case-insensitively, the rest keeps its case"
        );
    }

    #[test]
    fn paths_with_spaces_are_mapped() {
        let mapper = PathMapper::new(Path::new("C:\\My Paper")).unwrap();
        assert_eq!(
            mapper
                .map_argument(OsStr::new("C:\\My Paper\\main.tex"))
                .unwrap(),
            "/workspace/main.tex"
        );
    }

    #[test]
    fn quoted_paths_keep_their_quotes() {
        let mapper = PathMapper::new(Path::new("C:\\My Paper")).unwrap();
        assert_eq!(
            mapper
                .map_argument(OsStr::new("\"C:\\My Paper\\main.tex\""))
                .unwrap(),
            "\"/workspace/main.tex\""
        );
    }

    #[test]
    fn nested_paths_are_mapped() {
        assert_eq!(
            map("C:\\repo\\paper\\build\\main.pdf").unwrap(),
            "/workspace/build/main.pdf"
        );
    }

    #[test]
    fn unicode_paths_are_mapped() {
        assert_eq!(
            map("C:\\repo\\paper\\résumé\\chapitre-un.tex").unwrap(),
            "/workspace/résumé/chapitre-un.tex"
        );
    }

    #[test]
    fn relative_paths_are_left_alone() {
        assert_eq!(map("main.tex").unwrap(), "main.tex");
        assert_eq!(map("./main.tex").unwrap(), "./main.tex");
        assert_eq!(map("figures/a.pdf").unwrap(), "figures/a.pdf");
    }

    #[test]
    fn bare_flags_are_left_alone() {
        assert_eq!(map("-pdf").unwrap(), "-pdf");
        assert_eq!(map("-synctex=1").unwrap(), "-synctex=1");
        assert_eq!(
            map("-interaction=nonstopmode").unwrap(),
            "-interaction=nonstopmode"
        );
        assert_eq!(map("-file-line-error").unwrap(), "-file-line-error");
    }

    #[test]
    fn outdir_with_a_windows_path_is_mapped() {
        assert_eq!(
            map("-outdir=C:\\repo\\paper\\build").unwrap(),
            "-outdir=/workspace/build"
        );
        assert_eq!(
            map("-outdir=C:/repo/paper/build").unwrap(),
            "-outdir=/workspace/build"
        );
    }

    #[test]
    fn long_output_directory_flag_is_mapped() {
        assert_eq!(
            map("--output-directory=C:\\repo\\paper\\build").unwrap(),
            "--output-directory=/workspace/build"
        );
    }

    #[test]
    fn relative_outdir_is_left_alone() {
        assert_eq!(map("-outdir=build").unwrap(), "-outdir=build");
        assert_eq!(map("-aux-directory=aux").unwrap(), "-aux-directory=aux");
    }

    #[test]
    fn jobname_is_not_treated_as_a_path() {
        assert_eq!(map("-jobname=main").unwrap(), "-jobname=main");
        assert_eq!(map("-jobname=out:2").unwrap(), "-jobname=out:2");
    }

    #[test]
    fn dot_segments_are_resolved() {
        assert_eq!(
            map("C:\\repo\\paper\\.\\build\\main.aux").unwrap(),
            "/workspace/build/main.aux"
        );
    }

    #[test]
    fn parent_segments_within_the_workspace_are_resolved() {
        assert_eq!(
            map("C:\\repo\\paper\\chapters\\..\\main.tex").unwrap(),
            "/workspace/main.tex"
        );
    }

    #[test]
    fn a_sibling_of_the_workspace_is_rejected() {
        let error = map("C:\\repo\\other\\main.tex").unwrap_err();
        assert_eq!(error.exit_code(), 64);
        assert!(error.message().contains("outside"));
        assert!(error.message().contains("C:\\repo\\other\\main.tex"));
    }

    #[test]
    fn escaping_the_workspace_with_parent_segments_is_rejected() {
        let error = map("C:\\repo\\paper\\..\\other\\main.tex").unwrap_err();
        assert!(error.message().contains("outside"));
    }

    #[test]
    fn another_drive_is_rejected() {
        let error = map("D:\\fonts\\foo.ttf").unwrap_err();
        assert!(error.message().contains("outside"));
    }

    #[test]
    fn an_unc_path_is_rejected() {
        let error = map("\\\\server\\share\\main.tex").unwrap_err();
        assert!(error.message().contains("outside"));
    }

    #[test]
    fn outdir_outside_the_workspace_is_rejected() {
        let error = map("-outdir=C:\\elsewhere\\build").unwrap_err();
        assert!(error.message().contains("outside"));
    }

    #[test]
    fn a_relative_path_with_parent_segments_is_rejected() {
        let error = map("..\\secrets\\main.tex").unwrap_err();
        assert!(error.message().contains(".."));
    }

    #[test]
    fn contains_matches_the_workspace_and_its_children() {
        let mapper = mapper();
        assert!(mapper.contains(Path::new("C:\\repo\\paper")));
        assert!(mapper.contains(Path::new("C:\\repo\\paper\\a\\b.tex")));
        assert!(!mapper.contains(Path::new("C:\\repo\\paper2")));
        assert!(!mapper.contains(Path::new("C:\\repo")));
    }

    #[test]
    fn map_path_requires_an_absolute_path() {
        assert!(mapper().map_path(Path::new("main.tex")).is_err());
    }

    #[test]
    fn map_path_maps_the_cwd() {
        assert_eq!(
            mapper()
                .map_path(Path::new("C:\\repo\\paper\\chapters"))
                .unwrap(),
            "/workspace/chapters"
        );
    }
}
