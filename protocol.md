# tunTeX wire protocol v1

The client communicates with the server over HTTP.

## Request

`POST /compile` carries a `tar.gz` body and the following headers:

```text
Content-Type: application/gzip
X-TunTeX-Protocol: 1
X-TunTeX-Request-Id: <UUID>
Authorization: Bearer <token>   # Required when the server configures a token
```

The archive accepts regular files only and has this layout:

```text
meta/request.json
workspace/**
```

Request metadata contains `protocol`, `request_id`, `engine`, `argv`, `cwd`, `env`, and `timeout_seconds`. Absolute workspace paths use the virtual `/workspace/...` namespace.

## Response

A completed compilation returns HTTP 200 with a `tar.gz` body:

```text
meta/result.json
stdout.bin
stderr.bin
files/**
```

Result metadata contains `exit_code`, `timed_out`, `cancelled`, `duration_ms`, `changed`, and `deleted`. A non-zero LaTeX exit code is compilation data rather than an HTTP error, so the response remains HTTP 200 and the client returns that code unchanged.

`DELETE /jobs/{request_id}` cancels an active job. The protocol and request-ID headers must identify the same request.

`GET /health` reports liveness. `GET /info` reports the protocol version and configured engines.

The server rejects path traversal, links, special files, unknown fields, unconfigured engines, dangerous environment variables, and resource-limit violations before execution.

