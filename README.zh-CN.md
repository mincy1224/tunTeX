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

服务端没有配置文件，也没有 tunTeX 环境变量覆盖。固定监听 127.0.0.1:38117，通过 PATH 查找 TeX 引擎。启动服务端的账户需能直接调用 latexmk 和所需引擎。

```sh
tuntex-server project register
tuntex-server project list
tuntex-server project delete <key>
tuntex-server start
tuntex-server status
tuntex-server stop
```

注册返回随机 key；列表用表格按注册顺序显示项目 ID、可读的 UTC 注册时间和 key。项目 ID 标识服务端存储，key 填入客户端的 project_key。删除撤销 key 并删除存储。列表包含凭据，请勿公开。

无子命令或 run 为前台运行。日志和 PID 状态位于实际服务端程序同目录的 .state/。系统命令符号链接会解析到实际程序，调用时的当前目录不会影响存储位置。

项目数据固定放在程序同目录的 workspace/，包含 projects.sqlite3 和各项目目录。每个 key 对应独立持久工作区，同项目串行，最多四个不同项目并发。内置限制为请求/结果各 512 MiB、上传单文件 256 MiB、50,000 个文件、单次编译最多 600 秒。源文件清单与工作区版本指针通过 SQLite 事务一起发布。.tmp/ 下的请求临时目录自动清理，编译产物保留给后续 latexmk、BibTeX、Biber。

程序和数据集中放在一个目录，例如 /opt/tuntex，由运行服务端的普通账户拥有写权限，不用 root 启动。迁移时先停止旧服务端，再将完整项目存储目录（包括 SQLite 注册表）复制到新 workspace/，保留现有 key 和文件。

## 客户端实例

每次调用先同步源文件的新增、修改和删除，再判断是否需要编译。源文件和编译条件与上次缓存一致时，服务端直接返回产物、输出和退出码，不启动引擎；本地丢失的产物会恢复。失败结果保留真实错误和非零退出码，超时和取消结果不复用。引擎、参数、工作目录、透传环境、超时以及引擎程序标识参与缓存判断。latexmk 显式强制编译或清理参数绕过缓存；更新 TeX 宏包后用一次 `-g` 刷新结果。

客户端将诊断输出、`.log`、`.fls`、`.fdb_latexmk` 和 SyncTeX（包括 gzip）中的远端项目路径还原为本地工作区路径，系统 TeX 路径和二进制产物保持不变。

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

项目模式递归按扩展名收集，不解析文件内容：`.tex`、`.sty`、`.cls`、`.clo`、`.def`、`.cfg`、`.ltx`、`.fd`、`.bbx`、`.cbx`、`.lbx`、`.bib`、`.bst`、`.png`、`.jpg`、`.jpeg`、`.pdf`、`.eps`、`.ps`、`.svg`、`.mps`、`.csv`、`.tsv`、`.dat`、`.txt`、`.tikz`、`.pgf`、`.otf`、`.ttf`、`.ttc`。无需显式引用，仍遵守忽略规则和上传限制。系统宏包由服务端 TeX 提供。

引用的 .bib 与本地 .bst 会上传，生成的辅助文件保留在服务端。推荐 latexmk 处理多轮编译与参考文献。

每次编译前先同步源文件的新增、修改与删除。变化文件刷新修改时间，未变化文件保留原时间。源文件发生变化时自动让 latexmk 重建，避免旧失败记录阻止重新编译；无变化时保留增量编译，明确的清理命令不受影响。

依据输入文件名、jobname、输出目录和 latexmk 的 `-cd` 排除本次输出 PDF；`.aux`、`.log`、`.fls`、`.fdb_latexmk`、`.synctex.gz` 等中间文件不在白名单中。其他构建命令遗留的输出可通过 `.tuntexignore` 排除。EXE、版本控制目录和符号链接不上传。SHA-256 协商后只传新增或变化文件，同步冲突最多重试一次，补传完整文件集合。

扫描器不再拒绝宏构造的文件名或自定义加载命令；所需资源须已存在于工作区内，且扩展名在白名单中。工作区外资源和本地 latexmk 配置不上传。字体文件会上传，但需按项目文件引用；系统字体名称由服务端解析。扫描阶段不限制源文件编码，也不解析源文件内容。

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

EXE 叫 pdflatex.exe，在应用中指定路径，或将实例目录加入 PATH。临时模式要求恰好一个 .tex 输入，输入文件父目录作为工作区和远程 cwd；收集该目录直接包含的白名单文件，不递归扫描子目录或共享临时目录树。嵌套资源请使用项目模式。

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
