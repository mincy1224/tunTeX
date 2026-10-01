# Local benchmark

Measured on 2026-10-02 using the Windows client, `E:/Reading/pre2610/main.tex`, and the existing server at `127.0.0.1:38117`. Arguments matched the LaTeX Workshop recipe. No project sources or installed binaries were edited. All measured requests exited successfully.

| Case | Wall time |
| --- | --- |
| Initial installed-client cached calls | 104–115 ms |
| Explicit `-g` compilation | 3479 ms |
| Installed client, five alternating cached calls | 123, 99, 112, 91, 97 ms; mean 104.4 ms |
| Optimized client, same server | 94, 94, 88, 101, 109 ms; mean 97.2 ms |

The small client improvement overlaps measurement variability. This is not evidence of a large speedup. No historical full-directory-upload binary was available for a controlled comparison.

One optimized cached call reported: discovery/hashing 3 ms, manifest negotiation 22 ms, compile request/transfer 24 ms, result application 29 ms, uploaded sources 0 B, result 98.7 KiB, engine duration 0 ms. These phases omit process startup and other setup.

Optimizations retain full content verification: the client avoids replacing identical products, preserving their modification times; the server checks cached generations before allocating/copying a working workspace. Corrupt or missing stored products still force execution. Server changes require installation before measuring their live effect; the numbers above used the previously installed server.

Run `tools/benchmark.ps1` to reproduce normal requests and one explicit forced build. The first normal call can be cold because the last request may have different arguments. Use `debug: true` in a separate client instance to inspect phase timings. Do not compare a cold or forced build against a cache hit.
