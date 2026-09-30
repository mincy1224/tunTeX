# tunTeX

[English README](README.md)

tunTeX 是一个使用 Rust 编写的远程 LaTeX 编译代理。编辑器或构建工具调用本地客户端，客户端上传工作区，服务端运行真正的 TeX 引擎，再将生成文件、标准输出、标准错误和退出码同步回本地。

客户端和服务端都是独立的原生可执行文件，不依赖 Python。

## 工作方式

```text
编辑器 / 命令行
    │  xelatex main.tex
    ▼
tuntex-client
    │  HTTP + tar.gz
    ▼
tuntex-server
    │  xelatex main.tex
    ▼
PDF、日志、输出和退出码返回客户端
```

## 特性

- 保留 LaTeX 参数、输出流和退出码。
- 支持 `latexmk`、`xelatex`、`pdflatex`、`lualatex`、`bibtex`、`biber` 和 `makeindex`。
- 支持 Windows 和 Linux 上的超时及取消，并终止完整的进程树。
- 限制请求大小、展开后大小、单文件大小、文件数量、并发数和编译超时。
- 拒绝路径穿越、归档链接、特殊文件、危险环境变量和未知引擎。
- 完整校验结果归档后，再原子更新本地文件。
- 支持 Bearer Token 认证和显式取消任务。

## 构建

需要稳定版 Rust 工具链：

```bash
cargo build --release --bin tuntex-client
cargo build --release --bin tuntex-server
```

生成文件位于：

```text
target/release/tuntex-client[.exe]
target/release/tuntex-server[.exe]
```

## 客户端配置

客户端要求可执行文件同目录存在 `tun-tex-cfg.yaml`。可以复制示例配置：

```powershell
Copy-Item client/tun-tex-cfg.example.yaml target/release/tun-tex-cfg.yaml
```

Linux：

```bash
cp client/tun-tex-cfg.example.yaml target/release/tun-tex-cfg.yaml
```

配置示例：

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

- `socket` 支持 `host:port`、`http://host:port` 和 `https://host:port`。
- `tex` 必须是支持的引擎名称。
- `workspace` 和 `cwd` 为 `null` 时使用当前工作目录。
- `forward_env` 只列出允许透传给远端 TeX 进程的环境变量名称，不是 tunTeX 自身配置方式。
- 配置文件缺失、存在未知字段、YAML 无效或引擎不支持时，程序会在读取工作区和访问网络前立即退出。

客户端可以像普通 LaTeX 引擎一样调用：

```powershell
./tuntex-client.exe -interaction=nonstopmode main.tex
```

也可以将可执行文件命名为标准引擎名称，例如 `xelatex.exe`，以便编辑器无感调用。此时文件名必须与配置中的 `tex` 一致。不同引擎应放在不同目录，并分别放置自己的 `tun-tex-cfg.yaml`。

将 `.tuntexignore` 放在工作区根目录可以排除上传文件；版本控制目录默认排除。

## 服务端配置

复制服务端示例配置：

```powershell
Copy-Item server/tuntex-server.example.yaml tuntex-server.yaml
```

根据服务端安装的 TeX 发行版配置 `engines`。服务端默认从当前目录读取 `tuntex-server.yaml`。也可以使用 `TUNTEX_CONFIG` 指定另一个服务端配置文件路径。

后台启动并管理服务端：

```bash
tuntex-server start
tuntex-server ls
tuntex-server stop
```

`run` 用于前台运行。Linux 的后台状态和日志保存在 `~/.local/state/tuntex`，Windows 则保存在 `%LOCALAPPDATA%\tuntex`。

服务端只会执行同时满足以下条件的引擎：已在 YAML 中声明，并且属于内置允许列表。客户端不能选择任意可执行文件。

## 编辑器集成

将编辑器的 LaTeX 工具命令设置为 `tuntex-client.exe` 的绝对路径，并传入真实引擎所需的相同参数。例如 LaTeX Workshop 可以将客户端路径设置为 `command`，继续使用 `%DOC%` 和普通编译参数。

当编辑器从项目目录外启动客户端时，将 `workspace` 和 `cwd` 写入 `tun-tex-cfg.yaml`。

## 安全说明

服务端默认监听 `127.0.0.1`。跨主机使用时，请配置足够长的随机 Token，使用可信网络或启用 TLS 的反向代理，限制防火墙来源地址，并使用低权限专用账户运行服务端。

LaTeX 文档可能执行外部程序或消耗大量资源。tunTeX 会校验传输协议和文件路径，但不是 TeX 沙箱，不应暴露给不受信任的用户。

## 开发与验证

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

测试覆盖客户端配置、路径映射、归档安全、HTTP 协议、文件同步、认证、超时、取消以及服务端配置校验。

协议细节参见 [protocol.md](protocol.md)。

## 许可证

本项目使用 [MIT License](LICENSE)。
