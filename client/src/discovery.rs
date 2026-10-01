use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const EXTENSIONS: &[&str] = &[
    "tex", "sty", "cls", "clo", "def", "cfg", "ltx", "fd", "bbx", "cbx", "lbx", "bib", "bst",
    "png", "jpg", "jpeg", "pdf", "eps", "ps", "svg", "mps", "csv", "tsv", "dat", "txt", "tikz",
    "pgf", "otf", "ttf", "ttc",
];

pub fn discover(
    root: &Path,
    limit: usize,
    entry: Option<&Path>,
    excluded: &BTreeSet<PathBuf>,
) -> Result<BTreeSet<PathBuf>> {
    let root = root
        .canonicalize()
        .map_err(|e| Error::config(e.to_string()))?;
    if let Some(entry) = entry {
        let entry = entry
            .canonicalize()
            .map_err(|e| Error::config(e.to_string()))?;
        if !entry.starts_with(&root) {
            return Err(Error::config("temporary input escapes workspace"));
        }
    }
    let rules = crate::archive::IgnoreRules::load(&root)?;
    let recursive = entry.is_none();
    let mut selected = BTreeSet::new();
    let mut directories = vec![root.clone()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).map_err(|e| Error::config(e.to_string()))? {
            let entry = entry.map_err(|e| Error::config(e.to_string()))?;
            let kind = entry
                .file_type()
                .map_err(|e| Error::config(e.to_string()))?;
            let path = entry.path();
            let relative = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if kind.is_symlink() || rules.is_ignored(&relative, kind.is_dir()) {
                continue;
            }
            if kind.is_dir() {
                if recursive {
                    directories.push(path);
                }
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| EXTENSIONS.iter().any(|name| ext.eq_ignore_ascii_case(name)))
                && !is_excluded(&path, excluded)
            {
                selected.insert(path);
                if selected.len() > limit {
                    return Err(Error::config("file list exceeds max_file_count"));
                }
            }
        }
    }
    Ok(selected)
}

pub fn output_files(root: &Path, cwd: &str, engine: &str, argv: &[String]) -> BTreeSet<PathBuf> {
    if !matches!(engine, "latexmk" | "pdflatex" | "xelatex" | "lualatex") {
        return BTreeSet::new();
    }
    let cwd = virtual_local(root, cwd, root);
    let mut output_dir = None;
    let mut jobname = None;
    let mut inputs = Vec::new();
    let mut index = 0;
    while index < argv.len() {
        let arg = &argv[index];
        let (option, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(key, value)| (key, Some(value)));
        if matches!(
            option,
            "-outdir"
                | "--outdir"
                | "-output-directory"
                | "--output-directory"
                | "-jobname"
                | "--jobname"
        ) {
            let value = inline.or_else(|| {
                index += 1;
                argv.get(index).map(String::as_str)
            });
            if let Some(value) = value {
                if option.ends_with("jobname") {
                    jobname = Some(value.to_string());
                } else {
                    output_dir = Some(value.to_string());
                }
            }
        } else if !arg.starts_with('-') {
            let mut input = virtual_local(root, arg, &cwd);
            if input.extension().is_none() {
                input.set_extension("tex");
            }
            if input
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("tex"))
            {
                inputs.push(input);
            }
        }
        index += 1;
    }
    let cd = engine == "latexmk" && argv.iter().any(|arg| arg == "-cd");
    if inputs.is_empty() && engine == "latexmk" {
        if let Ok(entries) = std::fs::read_dir(&cwd) {
            inputs.extend(
                entries
                    .filter_map(std::result::Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|e| e.eq_ignore_ascii_case("tex"))
                    }),
            );
        }
    }
    let mut excluded = BTreeSet::new();
    for input in inputs {
        let base = if cd {
            input.parent().unwrap_or(&cwd)
        } else {
            &cwd
        };
        let name = jobname
            .as_deref()
            .map(Path::new)
            .or_else(|| input.file_stem().map(Path::new));
        if let Some(name) = name {
            let directory = output_dir.as_deref().map_or_else(
                || base.to_path_buf(),
                |value| virtual_local(root, value, base),
            );
            let mut filename = name.as_os_str().to_os_string();
            filename.push(".pdf");
            excluded.insert(directory.join(filename));
        }
    }
    excluded
}

fn is_excluded(path: &Path, excluded: &BTreeSet<PathBuf>) -> bool {
    #[cfg(windows)]
    {
        excluded.iter().any(|candidate| {
            candidate
                .to_string_lossy()
                .eq_ignore_ascii_case(&path.to_string_lossy())
        })
    }
    #[cfg(not(windows))]
    {
        excluded.contains(path)
    }
}

fn virtual_local(root: &Path, value: &str, base: &Path) -> PathBuf {
    let path = if value == "/workspace" {
        root.to_path_buf()
    } else if let Some(relative) = value.strip_prefix("/workspace/") {
        root.join(relative)
    } else {
        base.join(value)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn temporary_scanning_stays_in_the_input_directory() {
        let root = std::env::temp_dir().join(format!("tuntex-discovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("neighbor")).unwrap();
        std::fs::write(root.join("input.tex"), b"\\input{\\macro}").unwrap();
        std::fs::write(root.join("image.png"), b"image").unwrap();
        std::fs::write(root.join("neighbor/secret.tex"), b"not selected").unwrap();
        let selected =
            discover(&root, 100, Some(&root.join("input.tex")), &BTreeSet::new()).unwrap();
        assert_eq!(selected.len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn scans_resources_without_parsing_and_excludes_products_and_ignored_files() {
        let root = std::env::temp_dir().join(format!("tuntex-discovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("theme")).unwrap();
        std::fs::write(root.join("main.tex"), b"\\input{\\dynamic}").unwrap();
        std::fs::write(
            root.join("theme/custom.STY"),
            b"\\includegraphics{\\logo}\xff",
        )
        .unwrap();
        for name in [
            "photo.png",
            "paper.pdf",
            "font.otf",
            "refs.bib",
            "data.csv",
            "main.pdf",
            "main.log",
            "unrelated.exe",
            "ignored.jpg",
        ] {
            std::fs::write(root.join(name), b"data").unwrap();
        }
        std::fs::write(root.join(".tuntexignore"), "ignored.jpg\n").unwrap();
        let root = root.canonicalize().unwrap();
        let excluded = output_files(&root, "/workspace", "latexmk", &["main.tex".into()]);
        let selected = discover(&root, 100, None, &excluded).unwrap();
        assert_eq!(selected.len(), 7);
        assert!(selected.contains(&root.join("theme/custom.STY")));
        assert!(!selected.contains(&root.join("main.pdf")));
        assert!(discover(&root, 2, None, &excluded).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn output_paths_follow_jobname_output_directory_and_cd() {
        let root = Path::new("project");
        assert!(output_files(
            root,
            "/workspace",
            "latexmk",
            &[
                "-cd".into(),
                "-outdir=../build".into(),
                "-jobname".into(),
                "slides".into(),
                "/workspace/sub/main.tex".into()
            ]
        )
        .contains(&root.join("build/slides.pdf")));
        assert!(output_files(
            root,
            "/workspace",
            "xelatex",
            &[
                "--output-directory".into(),
                "build".into(),
                "main.tex".into()
            ]
        )
        .contains(&root.join("build/main.pdf")));
        assert!(output_files(
            root,
            "/workspace",
            "pdflatex",
            &["-jobname=slides.final".into(), "main".into()]
        )
        .contains(&root.join("slides.final.pdf")));
    }
}
