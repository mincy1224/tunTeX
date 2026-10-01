use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::error::{Error, Result};

pub fn discover(root: &Path, limit: usize, entry: Option<&Path>) -> Result<BTreeSet<PathBuf>> {
    let root = root
        .canonicalize()
        .map_err(|e| Error::config(e.to_string()))?;
    let mut selected = BTreeSet::new();
    let rules = crate::archive::IgnoreRules::load(&root)?;
    let mut directories = if let Some(entry) = entry {
        let entry = entry
            .canonicalize()
            .map_err(|e| Error::config(e.to_string()))?;
        if !entry.starts_with(&root) {
            return Err(Error::config("temporary input escapes workspace"));
        }
        selected.insert(entry);
        Vec::new()
    } else {
        vec![root.clone()]
    };
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).map_err(|e| Error::config(e.to_string()))? {
            let entry = entry.map_err(|e| Error::config(e.to_string()))?;
            let kind = entry
                .file_type()
                .map_err(|e| Error::config(e.to_string()))?;
            let relative = entry
                .path()
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rules.is_ignored(&relative, kind.is_dir()) {
                continue;
            }
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if !matches!(entry.file_name().to_str(), Some(".git" | ".hg" | ".svn")) {
                    directories.push(entry.path());
                }
            } else if entry
                .path()
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("tex"))
            {
                selected.insert(entry.path());
                if selected.len() > limit {
                    return Err(Error::config("dependency list exceeds max_file_count"));
                }
            }
        }
    }
    let command = Regex::new(r"\\(includegraphics|includepdf|includesvg|input|include|subfile|bibliography|addbibresource|bibliographystyle|documentclass|usepackage|lstinputlisting|VerbatimInput)\*?(?:\s*\[[^\]]*\])?\s*\{([^{}]*)\}").unwrap();
    let graphics = Regex::new(r"\\graphicspath\s*\{((?:\s*\{[^{}]*\}\s*)+)\}").unwrap();
    let group = Regex::new(r"\{([^{}]*)\}").unwrap();
    let mut graphics_search = BTreeSet::new();
    for source in &selected {
        let text = source_text(source)?;
        for capture in graphics.captures_iter(&text) {
            for path in group.captures_iter(&capture[1]) {
                if path[1].contains(['\\', '#', '$']) {
                    return Err(Error::config("dynamic graphicspath is unsupported"));
                }
                graphics_search.insert(root.join(&path[1]));
            }
        }
    }
    let mut pending: Vec<_> = selected.iter().cloned().collect();
    let mut scanned = BTreeSet::new();
    while let Some(source) = pending.pop() {
        if !scanned.insert(source.clone()) {
            continue;
        }
        let text = source_text(&source)?;
        let search = vec![source.parent().unwrap().to_path_buf(), root.clone()];
        for capture in graphics.captures_iter(&text) {
            for path in group.captures_iter(&capture[1]) {
                if path[1].contains(['\\', '#', '$']) {
                    return Err(Error::config("dynamic graphicspath is unsupported"));
                }
                graphics_search.insert(root.join(&path[1]));
            }
        }
        for capture in command.captures_iter(&text) {
            let name = &capture[1];
            let optional_system =
                matches!(name, "documentclass" | "usepackage" | "bibliographystyle");
            let values: Vec<_> = if matches!(name, "bibliography" | "usepackage") {
                capture[2].split(',').collect()
            } else {
                vec![&capture[2]]
            };
            let mut search = search.clone();
            if matches!(name, "includegraphics" | "includepdf" | "includesvg") {
                search.extend(graphics_search.iter().cloned());
            }
            for value in values.into_iter().map(str::trim) {
                if value.is_empty() || value.contains(['\\', '#', '$']) {
                    return Err(Error::config(format!(
                        "dynamic resource reference in {} is unsupported",
                        source.display()
                    )));
                }
                let extensions: &[&str] = match name {
                    "includegraphics" => &["", "pdf", "png", "jpg", "jpeg", "eps"],
                    "includepdf" => &["", "pdf"],
                    "includesvg" => &["", "svg"],
                    "input" | "include" | "subfile" => &["", "tex"],
                    "bibliography" | "addbibresource" => &["", "bib"],
                    "bibliographystyle" => &["", "bst"],
                    "documentclass" => &["", "cls"],
                    "usepackage" => &["", "sty"],
                    _ => &[""],
                };
                let mut found = None;
                for directory in &search {
                    for extension in extensions {
                        let mut path = directory.join(value);
                        if !extension.is_empty() && path.extension().is_none() {
                            path.set_extension(extension);
                        }
                        if path.is_file() {
                            let path = path
                                .canonicalize()
                                .map_err(|e| Error::config(e.to_string()))?;
                            if !path.starts_with(&root) {
                                return Err(Error::config(format!(
                                    "resource {value:?} escapes workspace"
                                )));
                            }
                            found = Some(path);
                            break;
                        }
                    }
                    if found.is_some() {
                        break;
                    }
                }
                if let Some(path) = found {
                    if selected.insert(path.clone())
                        && matches!(
                            path.extension().and_then(|e| e.to_str()),
                            Some("tex" | "sty" | "cls")
                        )
                    {
                        pending.push(path);
                    }
                } else if !optional_system {
                    return Err(Error::config(format!(
                        "resource {value:?} referenced by {} was not found",
                        source.display()
                    )));
                }
            }
        }
        if selected.len() > limit {
            return Err(Error::config("dependency list exceeds max_file_count"));
        }
    }
    Ok(selected)
}

fn source_text(source: &Path) -> Result<String> {
    use std::io::Read;
    const LIMIT: u64 = 16 * 1024 * 1024;
    let mut raw = String::new();
    std::fs::File::open(source)
        .and_then(|file| file.take(LIMIT + 1).read_to_string(&mut raw))
        .map_err(|e| Error::config(format!("cannot scan {}: {e}", source.display())))?;
    if raw.len() as u64 > LIMIT {
        return Err(Error::config(
            "a TeX source exceeds the 16 MiB scanning limit",
        ));
    }
    Ok(strip_comments(&raw))
}

fn strip_comments(raw: &str) -> String {
    raw.lines()
        .map(|line| {
            let mut slashes = 0;
            for (index, character) in line.char_indices() {
                if character == '%' && slashes % 2 == 0 {
                    return &line[..index];
                }
                slashes = if character == '\\' { slashes + 1 } else { 0 };
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_are_selected_but_unrelated_files_are_not() {
        let root = std::env::temp_dir().join(format!("tuntex-discovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join("main.tex"),
            "\\includegraphics{image}\\bibliography{refs}",
        )
        .unwrap();
        std::fs::write(root.join("image.png"), b"image").unwrap();
        std::fs::write(root.join("refs.bib"), b"refs").unwrap();
        std::fs::write(root.join("unrelated.exe"), b"excluded").unwrap();
        let selected = discover(&root, 100, None).unwrap();
        assert_eq!(selected.len(), 3);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn temporary_mode_does_not_select_neighboring_tex_files() {
        let root = std::env::temp_dir().join(format!("tuntex-discovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("main.tex"), "\\input{part}").unwrap();
        std::fs::write(root.join("part.tex"), "\\input{main}").unwrap();
        std::fs::write(root.join("unrelated.tex"), "\\input{missing}").unwrap();
        let selected = discover(&root, 100, Some(&root.join("main.tex"))).unwrap();
        assert_eq!(selected.len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn escaped_percent_does_not_hide_a_reference() {
        assert_eq!(strip_comments("\\% \\input{a} % hidden"), "\\% \\input{a} ");
        assert_eq!(strip_comments("\\\\% hidden"), "\\\\");
    }

    #[test]
    fn dynamic_references_are_rejected() {
        let root = std::env::temp_dir().join(format!("tuntex-discovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("main.tex"), "\\input{\\filename}").unwrap();
        assert!(discover(&root, 100, None)
            .unwrap_err()
            .message()
            .contains("dynamic"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nested_sources_share_static_graphics_and_bibliography_dependencies() {
        let root = std::env::temp_dir().join(format!("tuntex-discovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("chapters")).unwrap();
        std::fs::create_dir(root.join("figures")).unwrap();
        std::fs::write(root.join("main.tex"),"\\graphicspath{{figures/}}\\input{chapters/part}\\usepackage{local}\\addbibresource{refs.bib}").unwrap();
        std::fs::write(root.join("chapters/part.tex"), "\\includegraphics*{plot}").unwrap();
        std::fs::write(root.join("local.sty"), "\\VerbatimInput{data.csv}").unwrap();
        for path in ["figures/plot.png", "data.csv", "refs.bib"] {
            std::fs::write(root.join(path), b"data").unwrap();
        }
        assert_eq!(discover(&root, 100, None).unwrap().len(), 6);
        std::fs::remove_dir_all(root).unwrap();
    }
}
