# tunTeX

[English](README.md)

Rust 编写的远程 LaTeX 代理。编辑器调用本地 EXE，服务端运行真实引擎，传回生成文件、标准输出、标准错误及退出码。仅服务端需要 TeX Live。

## 构建与升级

```sh
cargo build --release --bin tuntex-client
cargo build --release --bin tuntex-server
```

产物位于 target/release。客户端安装在桌面端，服务端安装在提供 TeX Live 的环境中。

## 服务端

将 server/tuntex-server.example.yaml 复制为工作目录下的 tuntex-server.yaml，并配置引擎路径。TUNTEX_CONFIG 可指定其他服务端配置路径。

```sh
tuntex-server project register
tuntex-server project list
tuntex-server project delete <key>
tuntex-server start
tuntex-server status
tuntex-server stop
```

注册返回随机 key；列表按注册顺序显示 ID、Unix 时间戳和 key；删除撤销 key 并删除存储。列表包含凭据，请勿公开。

默认监听 127.0.0.1:38117。无子命令或 run 为前台运行。Linux 后台日志位于 ~/.local/state/tuntex/server.log，Windows 位于 %LOCALAPPDATA%/tuntex/server.log。

server.projects_root 指定 SQLite 注册表与项目存储目录；null 使用平台状态目录下的 projects。每个 key 对应独立持久工作区，同项目串行，不同项目按 max_concurrent_builds 并发。源文件清单与工作区版本指针通过 SQLite 事务一起发布。请求临时目录自动清理，编译产物保留给后续 latexmk、BibTeX、Biber。

## 客户端实例

一个 EXE 与同目录的 tun-tex-cfg.yaml 构成一个实例。客户端自身配置只用 YAML，不接受 tunTeX 环境变量覆盖。

```yaml
socket: 127.0.0.1:38117
tex: latexmk
project_key: '注册命令返回的KEY'
input_mode: project
workspace: 'E:/Documents/paper'
cwd: null
timeout_seconds: 120
forward_env: []
max_upload_size: 536870912
max_file_count: 50000
debug: false
```

项目模式必须配置存在的绝对 workspace。cwd 为 null 表示项目根目录；指定值必须位于工作区内。不会退回调用程序当前目录，拒绝磁盘根目录与 Windows 系统目录。

支持 latexmk、xelatex、pdflatex、lualatex、bibtex、biber、makeindex。EXE 可叫 tuntex-client.exe 或与 tex 完全一致，例如 pdflatex.exe。不同项目或引擎分开放在不同实例目录；其他名字被拒绝。

socket 接受 host:port 或 HTTP(S) URL。未知字段会报错。forward_env 只透传明确允许的引擎环境变量，不是 tunTeX 配置。旧 token、keep_temp 字段移除。

## 文件选择

项目模式纳入所有 .tex，再收集 input、include、subfile、includegraphics、bibliography、addbibresource、bibliographystyle、documentclass、usepackage、lstinputlisting、VerbatimInput 中的静态引用。请使用花括号文件名与正斜杠。本地 .sty/.cls 继续扫描，支持常见图片扩展名与静态 graphicspath。系统宏包、文档类、样式由服务端 TeX 提供。

引用的 .bib 与本地 .bst 会上传，生成的辅助文件保留在服务端。推荐 latexmk 处理多轮编译与参考文献。

无关 EXE、未引用图片/PDF、本地构建产物不会上传。跳过版本控制目录和符号链接；.tuntexignore 过滤源文件发现。SHA-256 协商后只传新增或变化文件。并发使缓存变化时最多重试一次，补传完整依赖集合。

不支持动态文件访问：不要用宏、变量、外部命令或运行时计算生成文件名。自定义加载命令、不带花括号输入、工作区外资源也不支持。静态扫描不是完整 TeX 解释器，不会兜底上传整个目录。文本源文件须为 UTF-8，单个不超过 16 MiB。本地 latexmk 配置、自定义字体和不支持命令加载的文件不会自动纳入。支持 includepdf/includesvg 引用。

单参数 --version、-version、-v、--help、-help、-h 查询远程引擎，不扫描、不上传工作区文件；查询产物在本地丢弃。

## VS Code 与 Inkscape

LaTeX Workshop 的 command 指定实例 EXE 绝对路径，保留正常引擎参数与 %DOC%，YAML 配置项目绝对 workspace。

生成临时 .tex 的应用使用单独实例：

```yaml
socket: 127.0.0.1:38117
tex: pdflatex
project_key: '单独注册的KEY'
input_mode: temporary
```

EXE 叫 pdflatex.exe，在应用中指定路径，或将实例目录加入 PATH。临时模式要求恰好一个 .tex 输入，只选择它与静态依赖，不递归扫描共享临时目录。输入文件父目录作为工作区和远程 cwd；资源必须位于其中。

查询仍需有效 YAML 与 key，但无需输入文件。部分 Inkscape 扩展需要额外工具或特定发现机制，不能保证所有扩展兼容。支持范围内 Windows 无需 TeX Live。

## 安全与验证

key 隔离存储，不隔离 TeX 对宿主机的访问。只运行可信文档，使用低权限账户。保持本地监听；跨主机需 TLS、防火墙和可信网络。不要提交包含 key 的配置。

服务端存储持久存在，请监控磁盘并删除不用的项目。不可信文档需要额外操作系统级隔离。

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

协议见 [protocol.md](protocol.md)，许可证为 [MIT](LICENSE)。
