# Roadmap

## Completed (v0.1.0)
- Core async daemon (`SnapBoxDaemon`) with PID lifecycle and graceful signal shutdown
- Deterministic finite-state lifecycle machine with audit history
- Common `CoWBackend` trait with macOS APFS `clonefile(2)` backend and Linux `overlayfs` backend + capability probing
- Policy-driven `SandboxAllocator` (`fifo`, `most_recently_reset`, `least_recently_used`)
- `WarmPoolManager` with pre-warming, bounded concurrent replenishment, and lease/failure reaper
- Security containment (environment scrubbing, loader var blocking, path/symlink confinement, `rlimit`s, process-group `killpg`, optional macOS Seatbelt profile)
- Real OS host telemetry (`sysctl` / `host_statistics64` / `/proc/meminfo`, `statvfs`, `getloadavg`)
- Axum 0.8 REST API and Clap CLI with built-in warm-vs-cold benchmark suite

## Near-Term (v0.2.0)
- Linux per-sandbox `unshare` mount/PID/network namespace wrapper + dedicated cgroups v2 slice controller (`memory.max`, `cpu.max`, `pids.max`)
- Seccomp-bpf syscall allowlist profile on Linux
- Multi-base image registry (`base_id` per language toolchain, e.g., `rust-1.85`, `node-22`, `python-3.13`)
- Async background trash deletion queue on macOS so `reset_workspace` returns immediately after `rename`

## Long-Term (v0.3.0+)
- Apple `Virtualization.framework` micro-VM backend for true untrusted multi-tenant isolation on macOS
- Firecracker / Cloud Hypervisor micro-VM backend on Linux
- Prometheus `/metrics` OpenMetrics exporter and OpenTelemetry trace propagation
- SDK clients for Python and TypeScript AI agent frameworks
