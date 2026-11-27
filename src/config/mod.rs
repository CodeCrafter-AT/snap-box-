use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::errors::{Result, SnapBoxError};

/// Allocation selection policy for choosing a warm sandbox from the ready queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AllocationPolicy {
    /// Oldest warm sandbox first (even wear and predictable ordering).
    #[default]
    Fifo,
    /// Most recently prepared/reset sandbox first (hotter page cache).
    MostRecentlyReset,
    /// Least recently used sandbox by last-used timestamp.
    LeastRecentlyUsed,
}

/// Top-level configuration for the snap-box daemon and pool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapBoxConfig {
    pub daemon: DaemonConfig,
    pub pool: PoolConfig,
    pub storage: StorageConfig,
    pub security: SecurityConfig,
    pub resources: ResourceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub bind_addr: SocketAddr,
    pub pid_file: PathBuf,
    pub shutdown_grace_ms: u64,
    pub json_logs: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolConfig {
    pub min_warm: usize,
    pub target_warm: usize,
    pub max_sandboxes: usize,
    pub max_concurrent_provisions: usize,
    pub allocation_policy: AllocationPolicy,
    pub default_lease_ttl_secs: u64,
    pub idle_timeout_secs: u64,
    pub reaper_interval_ms: u64,
    pub max_reuses_per_sandbox: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub base_source_dir: Option<PathBuf>,
    pub data_root: PathBuf,
    pub base_id: String,
    pub allow_non_cow_fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    pub strip_host_env: bool,
    pub allow_network: bool,
    pub enforce_path_confinement: bool,
    pub enforce_rlimits: bool,
    pub macos_seatbelt_profile: bool,
    pub linux_enable_namespaces: bool,
    pub linux_enable_cgroups: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceConfig {
    pub max_memory_bytes: u64,
    pub max_cpu_seconds: u64,
    pub max_disk_bytes: u64,
    pub max_open_files: u64,
    pub max_processes: u64,
    pub execution_timeout_ms: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        let base_runtime = std::env::temp_dir().join("snap-box");
        Self {
            bind_addr: "127.0.0.1:7432"
                .parse()
                .expect("static loopback socket addr must parse"),
            pid_file: base_runtime.join("snap-box.pid"),
            shutdown_grace_ms: 2_000,
            json_logs: false,
        }
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min_warm: 2,
            target_warm: 4,
            max_sandboxes: 16,
            max_concurrent_provisions: 4,
            allocation_policy: AllocationPolicy::Fifo,
            default_lease_ttl_secs: 300,
            idle_timeout_secs: 180,
            reaper_interval_ms: 1_000,
            max_reuses_per_sandbox: 50,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            base_source_dir: None,
            data_root: std::env::temp_dir().join("snap-box-data"),
            base_id: "default-env".to_string(),
            allow_non_cow_fallback: false,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            strip_host_env: true,
            allow_network: false,
            enforce_path_confinement: true,
            enforce_rlimits: true,
            macos_seatbelt_profile: false,
            linux_enable_namespaces: true,
            linux_enable_cgroups: true,
        }
    }
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            // 512 MiB per sandbox command by default (friendly to 8GB M1 hosts)
            max_memory_bytes: 512 * 1024 * 1024,
            max_cpu_seconds: 30,
            max_disk_bytes: 512 * 1024 * 1024,
            max_open_files: 256,
            max_processes: 64,
            execution_timeout_ms: 30_000,
        }
    }
}

impl SnapBoxConfig {
    /// Load configuration from a TOML file, then validate constraints.
    pub fn from_toml_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            SnapBoxError::Config(format!("failed to read config '{}': {}", path.display(), e))
        })?;
        let cfg: Self = toml::from_str(&raw)
            .map_err(|e| SnapBoxError::Config(format!("invalid TOML config: {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate all invariants across pool, storage, and resource bounds.
    pub fn validate(&self) -> Result<()> {
        if self.pool.max_sandboxes == 0 {
            return Err(SnapBoxError::Config(
                "pool.max_sandboxes must be at least 1".into(),
            ));
        }
        if self.pool.min_warm > self.pool.target_warm {
            return Err(SnapBoxError::Config(format!(
                "pool.min_warm ({}) cannot exceed pool.target_warm ({})",
                self.pool.min_warm, self.pool.target_warm
            )));
        }
        if self.pool.target_warm > self.pool.max_sandboxes {
            return Err(SnapBoxError::Config(format!(
                "pool.target_warm ({}) cannot exceed pool.max_sandboxes ({})",
                self.pool.target_warm, self.pool.max_sandboxes
            )));
        }
        if self.pool.max_concurrent_provisions == 0 {
            return Err(SnapBoxError::Config(
                "pool.max_concurrent_provisions must be at least 1".into(),
            ));
        }
        if self.pool.default_lease_ttl_secs == 0 {
            return Err(SnapBoxError::Config(
                "pool.default_lease_ttl_secs must be greater than 0".into(),
            ));
        }
        if self.pool.reaper_interval_ms < 50 {
            return Err(SnapBoxError::Config(
                "pool.reaper_interval_ms must be at least 50ms".into(),
            ));
        }
        if self.storage.base_id.trim().is_empty()
            || self
                .storage
                .base_id
                .chars()
                .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        {
            return Err(SnapBoxError::Config(
                "storage.base_id must be non-empty alphanumeric/dash/underscore".into(),
            ));
        }
        if self.resources.execution_timeout_ms < 50 {
            return Err(SnapBoxError::Config(
                "resources.execution_timeout_ms must be at least 50ms".into(),
            ));
        }
        if self.resources.max_open_files < 16 {
            return Err(SnapBoxError::Config(
                "resources.max_open_files must be at least 16".into(),
            ));
        }
        Ok(())
    }

    pub fn bases_dir(&self) -> PathBuf {
        self.storage.data_root.join("bases")
    }

    pub fn sandboxes_dir(&self) -> PathBuf {
        self.storage.data_root.join("sandboxes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = SnapBoxConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn invalid_pool_bounds_are_rejected() {
        let mut cfg = SnapBoxConfig::default();
        cfg.pool.min_warm = 10;
        cfg.pool.target_warm = 5;
        assert!(cfg.validate().is_err());

        let mut cfg2 = SnapBoxConfig::default();
        cfg2.pool.target_warm = 32;
        cfg2.pool.max_sandboxes = 16;
        assert!(cfg2.validate().is_err());
    }

    #[test]
    fn invalid_base_id_is_rejected() {
        let mut cfg = SnapBoxConfig::default();
        cfg.storage.base_id = "../escape".into();
        assert!(cfg.validate().is_err());
    }
}
