# tunTeX

[中文说明](README.zh-CN.md)

A Rust remote LaTeX proxy. Editors invoke the local executable; the server runs the real engine and returns generated files, stdout, stderr, and the exit code. Only the server needs TeX Live.

## Build

```sh
cargo build --release --bin tuntex-client
cargo build --release --bin tuntex-server
```

Binaries are in target/release. Install the client on your desktop and the server alongside TeX Live.

## Server

The server has no configuration file or tunTeX environment overrides. It listens on 127.0.0.1:38117 and finds TeX engines on PATH. Make sure latexmk and your required engines are available to the account launching it.

```sh
tuntex-server project register
tuntex-server project list
tuntex-server project delete <key>
tuntex-server start
tuntex-server status
tuntex-server stop
```

Register prints a random project key. List displays a table of project IDs, readable UTC registration times, and keys in registration order. Project IDs identify server storage; project keys belong in the client's project_key field. Delete revokes the key and removes its stored workspace. List output contains credentials; keep it private.

Run without a subcommand, or use run, for foreground operation. Background logs and PID state are in .state/ beside the actual server executable. System command symlinks resolve to that executable; the calling directory does not affect storage.

Project data always lives in workspace/ beside the executable, including projects.sqlite3 and project-specific directories. Each key owns an isolated persistent workspace. Same-project builds are serialized; up to four different projects run concurrently. Built-in limits are 512 MiB per request/result, 256 MiB per uploaded file, 50,000 files, and 600 seconds per compilation. Source manifests and workspace generation pointers are published together in SQLite. Request scratch directories under .tmp/ are removed; generated products persist for subsequent latexmk, BibTeX, and Biber calls.

Keep the executable and its data in one directory, such as /opt/tuntex, writable by the ordinary account running the server. Do not run it as root. When migrating an existing installation, stop the old server first and copy its entire project storage directory (including the SQLite registry) to the new workspace/ to preserve keys and data.

## Client instance

One executable and its adjacent tun-tex-cfg.yaml form one instance. Client settings are YAML-only; tunTeX environment overrides are not used.

```yaml
socket: 127.0.0.1:38117
tex: latexmk
project_key: 'KEY_FROM_PROJECT_REGISTER'
input_mode: project
workspace: 'E:/Documents/paper'
cwd: null
timeout_seconds: 120
forward_env: []
max_upload_size: 536870912
max_file_count: 50000
debug: false
```

Project mode requires an explicit existing absolute workspace. Null cwd means the workspace root; explicit cwd must be inside it. There is no fallback to the caller's current directory. Filesystem roots and Windows system directories are refused.

Supported engines: latexmk, xelatex, pdflatex, lualatex, bibtex, biber, makeindex. Name the executable tuntex-client.exe or exactly the selected engine name (such as pdflatex.exe). Different engines/projects need separate instance directories. Other names are rejected.

Socket accepts host:port or an HTTP(S) URL. Unknown YAML fields are errors. forward_env optionally forwards explicitly allowed engine environment variables, not tunTeX configuration. Old token and keep_temp fields are removed.

## File selection

Project mode includes all .tex files below the workspace and literal resources referenced by input, include, subfile, includegraphics, bibliography, addbibresource, bibliographystyle, documentclass, usepackage, lstinputlisting, and VerbatimInput. Use braced filenames and forward slashes. Local .sty/.cls inputs are scanned recursively. Common image extensions and literal graphicspath entries are supported. Missing system packages/classes/styles are supplied by server TeX.

Referenced .bib and local .bst files are included. Generated auxiliary files stay on the server. Prefer latexmk to orchestrate bibliography tools and multiple passes.

Before each compile, source additions, edits, and deletions are synchronized. Changed sources receive fresh modification times; unchanged sources keep theirs. Source changes automatically trigger a latexmk rebuild so stale failure records cannot suppress recompilation. Unchanged requests retain incremental behavior, and explicit cleaning commands are preserved.

Unreferenced PDFs/images, executables, local build products, and unrelated files are not uploaded. Version-control directories and symlinks are skipped. .tuntexignore filters source discovery. SHA-256 negotiation uploads only missing or changed selected files. If another call changes the cache, the client retries once with the complete selected dependency set.

Dynamic file access is unsupported: do not construct filenames using macros, variables, external programs, or runtime computation. Custom loading commands, unbraced inputs, and resources outside the workspace are unsupported. Static scanning is not a full TeX interpreter; there is no fallback that uploads the entire directory. Text sources must be UTF-8 and at most 16 MiB each. Local latexmk configuration, custom fonts, and files loaded through unsupported commands are not automatically included. PDF/SVG inclusions through includepdf/includesvg are supported.

Single-argument --version, -version, -v, --help, -help, and -h calls query the remote engine without scanning/uploading workspace files. Query artifacts are discarded locally.

## VS Code and Inkscape

For LaTeX Workshop, set command to the absolute instance executable path and retain normal compiler arguments and %DOC%. Set the project's absolute workspace in YAML.

For applications that generate temporary .tex inputs, create a separate instance:

```yaml
socket: 127.0.0.1:38117
tex: pdflatex
project_key: 'A_SEPARATE_REGISTERED_KEY'
input_mode: temporary
```

Name the executable pdflatex.exe and configure the application engine path, or add that instance directory to PATH. Temporary mode requires exactly one .tex argument. Only that entry and its static dependencies are selected; the shared temp directory is never recursively scanned. Its parent becomes the workspace and remote cwd. Resources must stay inside it.

Queries still require a valid instance YAML and key, but no input file. Individual Inkscape extensions may need additional tools or discovery mechanisms; compatibility with every extension is not guaranteed. Windows needs no TeX Live for supported calls.

## Security

Project keys isolate storage, not TeX execution. Only run trusted documents under a low-privilege account. TeX/latexmk can execute programs and access host resources. The listener is loopback-only. Any deliberately configured external proxy needs TLS, a firewall, and a trusted network. Never publish instance YAML containing keys.

Transport checks reject traversal, archive links/special files, invalid hashes, unknown engines, and oversized requests/results. Persistent storage needs disk monitoring and deletion of unused projects. Untrusted workloads require OS-level isolation.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

See [protocol.md](protocol.md). Licensed under [MIT](LICENSE).
