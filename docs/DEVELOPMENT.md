# Development Guide

## Prerequisites

- **Rust**: Stable toolchain `1.78+` (tested with `rustc 1.100.0` via `rustup`) with `clippy` and `rustfmt` components.
- **macOS**: Apple Silicon or Intel Mac with an **APFS** volume (standard on macOS 10.13+). Xcode Command Line Tools (`clang`) for building `libc` bindings.
- **Linux**: Kernel with `overlay` filesystem support (`/proc/filesystems`) and either `CAP_SYS_ADMIN` / root or unprivileged user+mount namespaces configured for OverlayFS.

## Build & Run

```bash
# Debug build
cargo build

# Release build (optimized for latency benchmarks)
cargo build --release

# Start daemon on default 127.0.0.1:7432
cargo run -- start --pool-size 4

# In another terminal, inspect pool and run a sandboxed command
cargo run -- status
cargo run -- sandbox create --owner dev-test
cargo run -- sandbox exec <SANDBOX_ID> -- /bin/sh -c "ls -la && echo hello > test.txt"
cargo run -- sandbox release <SANDBOX_ID>
cargo run -- stop
```

## Testing

```bash
# Unit tests
cargo test --lib

# Integration & security tests (macOS and Linux)
cargo test --test lifecycle_integration --test security_tests

# Linux OverlayFS test (runs mount lifecycle when kernel/privileges allow)
cargo test --test linux_overlayfs_tests
```

## Linting & Formatting

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

## Benchmarking

```bash
# Built-in reproducible warm-pool vs. cold-copy benchmark
cargo run --release -- benchmark --iterations 30 --pool-size 8 --file-count 200 --file-size-kb 16

# Criterion microbenchmarks
cargo bench --bench pool_benchmark
```

## Developing on 8 GB RAM (MacBook Air M1)

Default settings in `SnapBoxConfig` are tuned for an 8 GB RAM host:
- `pool.target_warm = 4`, `pool.max_sandboxes = 16`, `pool.max_concurrent_provisions = 4`
- `resources.max_memory_bytes = 512 MiB` per sandbox command
- Release builds use `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`.
