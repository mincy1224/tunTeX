# tunTeX server

The Rust HTTP server reads `tuntex-server.yaml` from its current directory by default. Set `TUNTEX_CONFIG` to select another configuration file.

The server limits request size, expanded size, individual file size, file count, concurrency, and execution time. It rejects archive links, path traversal, arbitrary executable names, and dangerous environment variables.

LaTeX can perform high-risk operations by design. Run the server only for trusted clients and see the repository [README](../README.md) for deployment guidance.

