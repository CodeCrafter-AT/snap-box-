# REST API Reference

Base URL (default): `http://127.0.0.1:7432`  
All error responses follow a structured JSON envelope:

```json
{
  "error": {
    "code": "SANDBOX_NOT_FOUND",
    "message": "sandbox '...' not found"
  }
}
```

---

## Endpoints

### `GET /health`
Returns daemon liveness, version, uptime, active CoW backend, and warm count.

**Response `200 OK`:**
```json
{
  "status": "ok",
  "version": "0.1.0",
  "uptime_secs": 14,
  "backend": "macos-apfs-clonefile",
  "warm_sandboxes": 4,
  "total_sandboxes": 4
}
```

### `GET /api/v1/pool`
Returns full pool capacity, state breakdown, base image SHA-256 manifest, backend capabilities, and real OS telemetry.

### `GET /api/v1/metrics`
Returns per-operation latency statistics (`count`, `failures`, `failure_rate`, `min_ms`, `avg_ms`, `median_ms`, `p95_ms`, `p99_ms`, `max_ms`, `throughput_ops_sec`).

### `GET /api/v1/sandboxes`
Lists all tracked `SandboxRecord` entries ordered by creation timestamp.

### `POST /api/v1/sandboxes/allocate`
Allocates an available `WARM` sandbox (or provisions on demand if `total < max_sandboxes`).

**Request body (optional JSON):**
```json
{
  "owner": "coding-agent-1",
  "ttl_secs": 300
}
```

**Status codes:** `201 Created`, `400 Bad Request` (invalid TTL), `503 Service Unavailable` (`POOL_EXHAUSTED`).

### `GET /api/v1/sandboxes/{id}`
Returns the `SandboxRecord` and measured `workspace_apparent_bytes`.

**Status codes:** `200 OK`, `400 Bad Request` (invalid UUID), `404 Not Found`.

### `POST /api/v1/sandboxes/{id}/exec`
Executes a command inside an `ALLOCATED` sandbox workspace.

**Request body:**
```json
{
  "program": "/bin/sh",
  "args": ["-c", "cargo test"],
  "cwd_rel": "src",
  "env": { "RUST_LOG": "info" },
  "timeout_ms": 10000
}
```

**Status codes:** `200 OK`, `403 Forbidden` (`SECURITY_VIOLATION`), `404 Not Found`, `409 Conflict` (`INVALID_STATE_TRANSITION`).

### `POST /api/v1/sandboxes/{id}/reset`
Resets an `ALLOCATED` sandbox's CoW workspace back to the clean base image while keeping the client's lease intact.

### `POST /api/v1/sandboxes/{id}/release`
Releases an `ALLOCATED` sandbox, discards temporary workspace changes, and returns it to the `WARM` pool (or destroys it if the pool is already at `target_warm`).

### `DELETE /api/v1/sandboxes/{id}`
Immediately terminates any running process group, unmounts/deletes the workspace, and removes the sandbox.

### `POST /api/v1/daemon/shutdown`
Triggers graceful daemon shutdown and full workspace cleanup.
