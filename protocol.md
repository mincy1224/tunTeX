# tunTeX wire protocol v2

The client communicates with the server over HTTP.

## Request

`POST /compile` carries a `tar.gz` body and the following headers:

```text
Content-Type: application/gzip
X-TunTeX-Protocol: 2
X-TunTeX-Request-Id: <UUID>
Authorization: Bearer <project_key>
```

The archive accepts regular files only and has this layout:

```text
meta/request.json
workspace/**
```

Request metadata contains `protocol`, `request_id`, `engine`, `argv`, `cwd`, `env`, `timeout_seconds`, and `source_manifest`. The manifest maps workspace-relative filenames to SHA-256 hex digests. Absolute workspace paths use the virtual `/workspace/...` namespace.

Before compilation, `POST /manifest` sends the complete source manifest as JSON with protocol and bearer headers. The response is a JSON array of missing or changed filenames. Compile archives contain only those source files. The server restores the last committed workspace, overlays uploads, removes previously tracked sources absent from the manifest, and verifies every manifest hash before execution.

Single-argument engine information queries have an empty manifest and no workspace entries. They do not change project storage. A stale source cache is rejected before execution; the client retries once with all selected sources.

Each registered key identifies one persistent project. Builds for that project are serialized. Workspace generations and source manifests are published with one SQLite transaction; revoked projects cannot publish. The protocol is not compatible with v1 clients or servers.

## Response

A completed compilation returns HTTP 200 with a `tar.gz` body:

```text
meta/result.json
stdout.bin
stderr.bin
files/**
```

Result metadata contains `exit_code`, `timed_out`, `cancelled`, `duration_ms`, `changed`, and `deleted`. A non-zero LaTeX exit code is compilation data rather than an HTTP error, so the response remains HTTP 200 and the client returns that code unchanged.

`DELETE /jobs/{request_id}` cancels an active job. Protocol and request-ID headers must identify the same request, and the bearer key must own the job.

`GET /health` reports liveness. `GET /info` reports the protocol version and configured engines.

The server rejects path traversal, links, special files, unknown fields, unconfigured engines, dangerous environment variables, and resource-limit violations before execution.

