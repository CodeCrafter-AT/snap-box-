# Performance Engineering & Benchmark Results

## Target Objective

**Sub-200ms sandbox allocation and rollback** under documented benchmark conditions, backed by reproducible measurements against a cold full-copy baseline.

---

## Test Environment

- **Hardware:** Apple MacBook Air M1 (`MacBookAir10,1`), 8-core ARM64, 8 GiB unified RAM
- **OS & Filesystem:** macOS Darwin 27.2.0 (`arm64`), APFS
- **Rust Toolchain:** `rustc 1.100.0` (`--release`, `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`)
- **Backend:** `macos-apfs-clonefile` (`clonefile(2)` with `CLONE_NOFOLLOW | CLONE_NOFOLLOW_ANY`)
- **Baseline:** `PortableCopyBackend` (recursive file-by-file byte copy)

---

## Measured Results (`snap-box benchmark`)

### Workload A — 200 Files, 3.12 MiB Base Tree, Pool Size 8, 50 Iterations

```text
snap-box Benchmark Report
Backend: macos-apfs-clonefile (true_cow=true) | Base: 200 files (3.12 MiB) | Pool: 8
OPERATION                     MIN(ms)  AVG(ms)  P50(ms)  P95(ms)  P99(ms)    OPS/SEC
warm_allocation                 0.002    0.008    0.002    0.006    0.256       62.4
release_and_rollback           15.522   15.886   15.841   16.383   16.467       62.4
cow_clone                       1.955    2.249    2.256    2.500    2.632       65.1
concurrent_burst_alloc          0.004    0.020    0.011    0.059    0.059      158.3
cold_full_copy_baseline        43.765   45.475   44.381   50.501   50.501       17.1
```

*Note on `OPS/SEC`: in `snap-box benchmark`, `warm_allocation` and `release_and_rollback` share the sequential allocate-mutate-release cycle wall clock (`62.4` full cycles/sec), and `cow_clone` shares the clone+destroy loop wall clock.*

### Workload B — 500 Files, 7.81 MiB Base Tree, Pool Size 16, 30 Iterations, 16 Concurrent Workers

| Operation | Min (ms) | Avg (ms) | Median / P50 (ms) | P95 (ms) | P99 (ms) | Max (ms) | Failure Rate |
|---|---:|---:|---:|---:|---:|---:|---:|
| `warm_allocation` | 0.002 | 0.003 | 0.002 | 0.008 | 0.010 | 0.010 | 0.0% |
| `release_and_rollback` | 34.547 | 36.048 | 35.936 | 37.507 | 42.308 | 42.308 | 0.0% |
| `cow_clone` (APFS) | 4.216 | 4.595 | 4.614 | 5.004 | 5.069 | 5.069 | 0.0% |
| `concurrent_burst_allocation` (16x) | 0.003 | 0.020 | 0.010 | 0.051 | 0.071 | 0.071 | 0.0% |
| `cold_full_copy_baseline` | 107.917 | 118.956 | 109.084 | 201.460 | 201.460 | 201.460 | 0.0% |

### Criterion Microbenchmarks (`cargo bench --bench pool_benchmark`)

Base tree: 100 files × 16 KiB (1.56 MiB):

| Benchmark | Mean Time | 95% Confidence Interval |
|---|---|---|
| `warm_allocate_and_release_100x16k` | **8.59 ms** | `[8.5493 ms, 8.6322 ms]` |
| `cow_clone_and_destroy_100x16k` | **8.61 ms** | `[8.5563 ms, 8.6758 ms]` |
| `cold_full_copy_baseline_100x16k` | **29.75 ms** | `[29.658 ms, 29.853 ms]` |

---

## Key Observations & Bottleneck Analysis

1. **Warm allocation is O(1) memory/state handoff (~2–20 µs):** Allocating from the pre-warmed queue hits no disk I/O path, beating the cold copy baseline (`44–119 ms`) by four orders of magnitude.
2. **APFS `clonefile(2)` scales with metadata entry count, not file byte size:** Cloning 500 files (7.81 MiB) takes `4.60 ms` avg because extent blocks are shared CoW without byte copying, whereas `PortableCopyBackend` degrades linearly with bytes (`118.96 ms` avg, `201.46 ms` p95).
3. **Rollback cost is dominated by recursive `unlink` (`remove_dir_all`), not cloning:** In Workload B, `cow_clone` takes `4.60 ms` while `release_and_rollback` (`clone + rename + remove_dir_all`) takes `36.05 ms` — roughly 85% of rollback latency is deleting the old dirty tree synchronously.
4. **Target status:** Both warm allocation (`< 0.3 ms` P99) and full CoW rollback (`16.5–42.3 ms` P99) are well below the **200 ms** objective on Apple Silicon M1.

## Reproducing Benchmarks

```bash
cargo build --release
./target/release/snap-box benchmark --iterations 50 --concurrency 8 --pool-size 8 --file-count 200 --file-size-kb 16
./target/release/snap-box benchmark --iterations 30 --concurrency 16 --pool-size 16 --file-count 500 --file-size-kb 16 --json
cargo bench --bench pool_benchmark
```
