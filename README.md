# tunTeX

[中文说明](README.zh-CN.md)

tunTeX is a remote LaTeX compilation proxy written in Rust. An editor or build tool invokes the local client, the client uploads the workspace, and the server runs the real TeX engine. Generated files, stdout, stderr, and the exit code are then synchronized back to the local machine.

Both the client and server are standalone native executables with no Python dependency.

## How it works

```text
Editor / CLI
    │  xelatex main.tex
    ▼
tuntex-client
    │  HTTP + tar.gz
    ▼
tuntex-server
    │  xelatex main.tex
    ▼
PDF, logs, stdout, stderr, and exit code return to the client
```

## Features

- Preserves LaTeX arguments, output streams, and exit codes.
- Supports `latexmk`, `xelatex`, `pdflatex`, `lualatex`, `bibtex`, `biber`, and `makeindex`.
- Terminates timed-out or cancelled process trees on Windows and Linux.
- Enforces request-size, expanded-size, per-file, file-count, concurrency, and timeout limits.
- Rejects path traversal, archive links, special files, dangerous environment variables, and unknown engines.
- Validates complete result archives before atomically updating local files.
- Supports bearer-token authentication and explicit job cancellation.

## Build

A stable Rust toolchain is required.

```bash
cargo build --release --bin tuntex-client
cargo build --release --bin tuntex-server
```

The executables are written to:

```text
target/release/tuntex-client[.exe]
target/release/tuntex-server[.exe]
```

## Client configuration

The client requires `tun-tex-cfg.yaml` beside its executable. Start from the example:

```powershell
Copy-Item client/tun-tex-cfg.example.yaml target/release/tun-tex-cfg.yaml
```

On Linux:

```bash
cp client/tun-tex-cfg.example.yaml target/release/tun-tex-cfg.yaml
```

Configuration format:

```yaml
socket: 127.0.0.1:xxxxx
tex: xelatex
workspace: null
cwd: null
token: ""
timeout_seconds: 120
forward_env: []
max_upload_size: 536870912
max_file_count: 50000
debug: false
keep_temp: false
```

- `socket` accepts `host:port`, `http://host:port`, or `https://host:port`.
- `tex` must name one of the supported engines.
- `workspace` and `cwd` may be `null` to use the invocation's current directory.
- `forward_env` lists environment-variable names whose values may be forwarded.
- A missing file, unknown field, invalid YAML document, or unsupported engine causes an immediate configuration error before the workspace is read or the network is accessed.

Invoke the client directly:

```powershell
./tuntex-client.exe -interaction=nonstopmode main.tex
```

The executable may also be named after a standard tool, such as `xelatex.exe`, for transparent editor integration. In that case, the executable name must match `tex`. Because the configuration is tied to the executable directory, place different engine installations in separate directories with their own `tun-tex-cfg.yaml` files.

### Client configuration fields

All client settings belong in `tun-tex-cfg.yaml`. The file is loaded from the executable directory and is required for every invocation. `socket` selects the server, `tex` selects the engine, `workspace` and `cwd` define the uploaded project and remote working directory, and `token` authenticates the request. `timeout_seconds`, `forward_env`, `max_upload_size`, `max_file_count`, `debug`, and `keep_temp` control compilation, environment forwarding, limits, diagnostics, and temporary-file handling.

Place `.tuntexignore` in the workspace root to exclude files from uploads. Version-control directories are excluded by default.

## Server configuration

Copy the example configuration:

```powershell
Copy-Item server/tuntex-server.example.yaml tuntex-server.yaml
```

Configure `engines` for the TeX distribution installed on the server:

```yaml
server:
  host: 127.0.0.1
  port: xxxxx
  token: ""
  max_request_size: 536870912
  max_result_size: 536870912
  max_file_size: 268435456
  max_file_count: 50000
  max_concurrent_builds: 4
  max_timeout_seconds: 600
  keep_temp: false

engines:
  xelatex:
    command: xelatex
    args: []
  latexmk:
    command: latexmk
    args: []
```

The server reads `tuntex-server.yaml` from its current directory by default. Set `TUNTEX_CONFIG` to use another path:

```powershell
$env:TUNTEX_CONFIG = "C:\tuntex\tuntex-server.yaml"
./tuntex-server.exe
```

```bash
TUNTEX_CONFIG=/etc/tuntex/server.yaml ./tuntex-server
```

Run the server in the background and manage it with:

```bash
tuntex-server start
tuntex-server ls
tuntex-server stop
```

`run` keeps the server in the foreground. Background state and logs are stored under `~/.local/state/tuntex` on Linux and `%LOCALAPPDATA%\tuntex` on Windows.

The server executes only engines that are both declared in the YAML file and present in its built-in allowlist. A client cannot select an arbitrary executable.

## Editor integration

Set the editor's LaTeX tool command to the absolute path of `tuntex-client.exe`, then pass the same arguments that the real engine would receive. For example, LaTeX Workshop can use the client path as `command` and retain `%DOC%` and the normal compiler flags in `args`.

The client uses the invocation's current directory when `workspace` and `cwd` are `null`. Set those fields in `tun-tex-cfg.yaml` when the editor launches the tool from outside the project.

## Security

The server listens on `127.0.0.1` by default. For access across hosts:

- Configure a sufficiently long random token.
- Use a trusted network or a TLS-enabled reverse proxy.
- Restrict source addresses with a firewall.
- Run the server under a dedicated low-privilege account.

LaTeX documents may invoke external programs or consume significant resources. tunTeX validates its transport protocol and filesystem paths, but it is not a TeX sandbox and must not be exposed to untrusted users.

## Development and verification

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

The test suite covers client configuration, path mapping, archive safety, the HTTP protocol, file synchronization, authentication, timeouts, cancellation, and server configuration validation.

See [protocol.md](protocol.md) for protocol details.

## License

This project is licensed under the [MIT License](LICENSE).

## Project layout

```text
client/      Rust client, unit tests, and integration tests
server/      Rust server and YAML example
protocol.md  Wire protocol v1
```

