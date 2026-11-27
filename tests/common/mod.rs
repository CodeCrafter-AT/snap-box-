use std::{fs, sync::Arc};

use snap_box::{
    config::SnapBoxConfig, filesystem::select_backend, metrics::MetricsRegistry,
    pool::WarmPoolManager,
};
use tempfile::TempDir;

pub fn create_seed_dir(root: &std::path::Path) -> std::path::PathBuf {
    let seed = root.join("seed-repo");
    fs::create_dir_all(seed.join("src")).unwrap();
    fs::write(seed.join("README.md"), "# Seed Project\n").unwrap();
    fs::write(seed.join("src/hello.txt"), "immutable base content\n").unwrap();
    seed
}

pub async fn create_test_pool(
    target_warm: usize,
    max_sandboxes: usize,
) -> (TempDir, Arc<WarmPoolManager>) {
    let temp = tempfile::tempdir().unwrap();
    let seed = create_seed_dir(temp.path());

    let mut cfg = SnapBoxConfig::default();
    cfg.storage.data_root = temp.path().join("data");
    cfg.storage.base_source_dir = Some(seed);
    cfg.storage.allow_non_cow_fallback = cfg!(not(target_os = "macos"));
    cfg.pool.min_warm = target_warm.min(1);
    cfg.pool.target_warm = target_warm;
    cfg.pool.max_sandboxes = max_sandboxes;
    cfg.pool.reaper_interval_ms = 50;

    let backend = select_backend(&cfg.storage.data_root, cfg.storage.allow_non_cow_fallback)
        .expect("backend must initialize on test host");
    let metrics = MetricsRegistry::new();
    let pool = WarmPoolManager::new(cfg, backend, metrics).unwrap();
    pool.warm_initial_pool().await.unwrap();
    (temp, pool)
}
