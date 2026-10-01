# tunTeX server

The Rust HTTP server has no configuration file or tunTeX environment overrides. It listens on `127.0.0.1:38117` and uses TeX commands available on the launching account's PATH.

Project storage is always in `workspace/` beside the actual executable, with logs and PID state in `.state/` and request scratch directories in `.tmp/`. Calling directories and system command symlinks do not change this layout. The account running the server must have write access to the installation directory; do not run it as root.

Use `project register`, `project list`, and `project delete <key>` to manage projects, and `start`, `status`, and `stop` to manage the background server. Migration requires copying the old SQLite registry and project directories into the new `workspace/` while the old server is stopped.

The server limits request size, expanded size, individual file size, file count, concurrency, and execution time. It rejects archive links, path traversal, arbitrary executable names, and dangerous environment variables.

LaTeX can perform high-risk operations by design. Run the server only for trusted clients and see the repository [README](../README.md) for deployment guidance.

