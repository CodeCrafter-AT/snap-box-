# Security Model & Threat Analysis

## Security Philosophy

`snap-box` separates **filesystem Copy-on-Write isolation** from **process/kernel isolation**. A filesystem clone or union mount ensures one workload's disk writes do not corrupt the base image or sibling workspaces, but it does **not** by itself prevent a process from inspecting host processes, network sockets, or kernel interfaces unless paired with OS isolation mechanisms.

---

## Threat Matrix & Mitigations

| Threat | Mitigation in `snap-box` | Platform Caveats |
|---|---|---|
| **Base filesystem tampering** | Base directory sits outside sandbox `workspace` paths; verified with deterministic SHA-256 manifest (`verify_base_integrity`). Linux OverlayFS enforces read-only `lowerdir` at VFS level. | On macOS APFS, `clonefile` gives independent inodes; workloads confined to `workspace` cannot write to `base` unless escaping process confinement. |
| **Directory traversal / symlink escape** | `resolve_confined_cwd` and `validate_workspace_relative_path` reject absolute paths, `..` components, and symlinks resolving outside canonicalized `workspace`. APFS `clonefile` uses `CLONE_NOFOLLOW \| CLONE_NOFOLLOW_ANY`. | Commands spawned inside a workspace without chroot/VM can still pass arbitrary paths to child utilities unless `macos_seatbelt_profile` or Linux namespaces are active. |
| **Host secret leakage** | `build_sanitized_env` clears parent environment (`cmd.env_clear()`) when `strip_host_env = true`, sets sandbox-local `HOME` and `TMPDIR`, and blocks `AWS_*`, `SSH_AUTH_SOCK`, `GITHUB_TOKEN`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`. | Set `security.strip_host_env = true` (default) in production. |
| **Runaway / fork-bombing workloads** | Every spawned command gets its own process group (`setpgid(0, 0)`), `RLIMIT_CPU`, `RLIMIT_FSIZE`, `RLIMIT_NOFILE`, and wall-clock `timeout_ms` triggering `killpg(pgid, SIGKILL)`. | `RLIMIT_NPROC` / cgroups v2 `pids.max` depend on OS user limits / cgroup delegation. |
| **Concurrent double-allocation** | `PoolState` (`records` + `warm_queue`) is guarded by a single mutex with atomic state transitions (`Warm -> Allocated`). | Tested under concurrent burst stress tests. |
| **Incomplete cleanup / zombie leases** | Background reaper reclaims expired leases (`lease_expires_at`), destroys `Failed` sandboxes, and `shutdown_all()` kills active process groups and removes all workspaces on exit. | Unclean `SIGKILL` of the daemon itself requires manual cleanup of `data_root/sandboxes`. |
| **Privilege escalation** | Daemon runs unprivileged on macOS. On Linux, `PR_SET_NO_NEW_PRIVS` is applied in `pre_exec` when `linux_enable_namespaces = true`. | Never run untrusted public multi-tenant code directly on macOS without a VM boundary. |

---

## Platform Isolation Boundaries (Honest Assessment)

### macOS (`MacOsApfsBackend`)
- **Provided:** APFS `clonefile(2)` CoW workspaces, environment scrubbing, `cwd` path/symlink validation, POSIX `rlimit`s, process-group kill on timeout/destroy, and optional `/usr/bin/sandbox-exec` Seatbelt file-write/network restriction (`macos_seatbelt_profile = true`).
- **Not Provided:** macOS has no Linux-style PID/mount/network namespaces. Apple deprecated `sandbox-exec` for third-party App Store apps, though it remains functional on macOS for local tooling. **Do not treat macOS APFS workspaces alone as a multi-tenant kernel boundary for hostile untrusted code**; pair with a lightweight Linux VM (e.g., Apple Virtualization.framework) for untrusted multi-tenant execution.

### Linux (`LinuxOverlayBackend`)
- **Provided:** Kernel OverlayFS (`lowerdir` read-only base, `upperdir` per-sandbox delta), `PR_SET_NO_NEW_PRIVS`, `rlimit`s, process-group reaping, and capability probing for user namespaces (`/proc/self/ns/user`) and cgroups v2 (`/sys/fs/cgroup/cgroup.controllers`).
- **Caveat:** Mounting `overlay` in kernel space requires `CAP_SYS_ADMIN` (or kernel support for unprivileged overlay mounts inside a user namespace). `snap-box` probes mount capability explicitly at startup rather than silently pretending OverlayFS is active.
