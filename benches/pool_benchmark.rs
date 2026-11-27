use std::{fs, sync::Arc};

use criterion::{criterion_group, criterion_main, Criterion};
use snap_box::{
    config::SnapBoxConfig,
    filesystem::{select_backend, CoWBackend, PortableCopyBackend},
    metrics::MetricsRegistry,
    pool::WarmPoolManager,
    sandbox::SandboxId,
};

fn build_seed_tree(
    root: &std::path::Path,
    file_count: usize,
    file_size_kb: usize,
) -> std::path::PathBuf {
    let seed = root.join("seed");
    fs::create_dir_all(&seed).unwrap();
    let payload = vec![b'X'; file_size_kb * 1024];
    for i in 0..file_count {
        let sub = seed.join(format!("mod_{}", i % 8));
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join(format!("file_{i}.bin")), &payload).unwrap();
    }
    seed
}

fn bench_pool_and_cow(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let seed = build_seed_tree(temp.path(), 100, 16);

    let mut cfg = SnapBoxConfig::default();
    cfg.storage.data_root = temp.path().join("data");
    cfg.storage.base_source_dir = Some(seed);
    cfg.storage.allow_non_cow_fallback = cfg!(not(target_os = "macos"));
    cfg.pool.target_warm = 6;
    cfg.pool.min_warm = 2;
    cfg.pool.max_sandboxes = 16;

    let backend =
        select_backend(&cfg.storage.data_root, cfg.storage.allow_non_cow_fallback).unwrap();
    let pool = WarmPoolManager::new(cfg, Arc::clone(&backend), MetricsRegistry::new()).unwrap();
    rt.block_on(pool.warm_initial_pool()).unwrap();

    c.bench_function("warm_allocate_and_release_100x16k", |b| {
        b.to_async(&rt).iter(|| async {
            let sb = pool.allocate(Some("crit".into()), Some(60)).await.unwrap();
            pool.release(sb.id).await.unwrap();
        });
    });

    let clone_dir = temp.path().join("clone_bench");
    fs::create_dir_all(&clone_dir).unwrap();
    c.bench_function("cow_clone_and_destroy_100x16k", |b| {
        b.iter(|| {
            let id = SandboxId::new();
            let mut ws = backend
                .create_workspace(id, pool.base_image(), &clone_dir)
                .unwrap();
            backend.destroy_workspace(id, &mut ws).unwrap();
        });
    });

    let cold_backend = PortableCopyBackend::new();
    let cold_dir = temp.path().join("cold_bench");
    fs::create_dir_all(&cold_dir).unwrap();
    c.bench_function("cold_full_copy_baseline_100x16k", |b| {
        b.iter(|| {
            let id = SandboxId::new();
            let mut ws = cold_backend
                .create_workspace(id, pool.base_image(), &cold_dir)
                .unwrap();
            cold_backend.destroy_workspace(id, &mut ws).unwrap();
        });
    });

    rt.block_on(pool.shutdown_all());
}

criterion_group!(benches, bench_pool_and_cow);
criterion_main!(benches);
