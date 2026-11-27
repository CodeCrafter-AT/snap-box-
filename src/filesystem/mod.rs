pub mod linux;
pub mod macos;

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::{
    errors::{Result, SnapBoxError},
    sandbox::{SandboxId, WorkspacePaths},
};

/// Security isolation boundary actually provided by a filesystem/runtime backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationLevel {
    /// No CoW isolation (full-copy baseline only).
    NoneBaselineCopy,
    /// Filesystem-level Copy-on-Write + userspace env/rlimit/path containment (macOS APFS).
    FilesystemCowAndProcessBounds,
    /// Kernel OverlayFS + mount/PID/IPC/UTS namespaces & cgroups v2 (Linux).
    KernelOverlayAndNamespaces,
}

/// Capabilities reported truthfully by a storage backend after runtime probing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendCapabilities {
    pub backend_name: String,
    pub true_kernel_cow: bool,
    pub supports_namespaces: bool,
    pub supports_cgroups: bool,
    pub isolation_level: IsolationLevel,
    pub notes: String,
}

/// Prepared read-only base environment image.
#[derive(Debug, Clone, Serialize)]
pub struct BaseImage {
    pub base_id: String,
    pub root_dir: PathBuf,
    pub manifest_sha256: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

/// Common Copy-on-Write filesystem backend abstraction.
pub trait CoWBackend: Send + Sync {
    fn capabilities(&self) -> BackendCapabilities;

    fn prepare_base(
        &self,
        base_id: &str,
        source_dir: Option<&Path>,
        bases_dir: &Path,
    ) -> Result<BaseImage>;

    fn create_workspace(
        &self,
        sandbox_id: SandboxId,
        base: &BaseImage,
        sandboxes_dir: &Path,
    ) -> Result<WorkspacePaths>;

    fn reset_workspace(
        &self,
        sandbox_id: SandboxId,
        paths: &mut WorkspacePaths,
        base: &BaseImage,
    ) -> Result<()>;

    fn destroy_workspace(&self, sandbox_id: SandboxId, paths: &mut WorkspacePaths) -> Result<()>;

    fn workspace_disk_usage(&self, paths: &WorkspacePaths) -> Result<u64>;

    fn verify_base_integrity(&self, base: &BaseImage) -> Result<bool> {
        let (digest, _, _) = compute_tree_manifest_sha256(&base.root_dir)?;
        Ok(digest == base.manifest_sha256)
    }
}

/// Select the native CoW backend for the current host OS and verify support.
pub fn select_backend(
    data_root: &Path,
    allow_non_cow_fallback: bool,
) -> Result<Arc<dyn CoWBackend>> {
    fs::create_dir_all(data_root).map_err(|e| SnapBoxError::Filesystem {
        operation: "create_dir_all".into(),
        path: data_root.display().to_string(),
        source: e,
    })?;

    #[cfg(target_os = "macos")]
    {
        match macos::MacOsApfsBackend::probe(data_root) {
            Ok(backend) => Ok(Arc::new(backend)),
            Err(err) => {
                if allow_non_cow_fallback {
                    tracing::warn!(
                        error = %err,
                        "APFS clonefile unavailable; falling back to PortableCopyBackend because allow_non_cow_fallback=true"
                    );
                    return Ok(Arc::new(PortableCopyBackend::new()));
                }
                Err(err)
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        match linux::LinuxOverlayBackend::probe(data_root) {
            Ok(backend) => Ok(Arc::new(backend)),
            Err(err) => {
                if allow_non_cow_fallback {
                    tracing::warn!(
                        error = %err,
                        "Linux OverlayFS unavailable; falling back to PortableCopyBackend because allow_non_cow_fallback=true"
                    );
                    return Ok(Arc::new(PortableCopyBackend::new()));
                }
                Err(err)
            }
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        if allow_non_cow_fallback {
            Ok(Arc::new(PortableCopyBackend::new()))
        } else {
            Err(SnapBoxError::UnsupportedPlatform(
                "no native CoW backend implemented for this operating system".into(),
            ))
        }
    }
}

/// Non-CoW full-copy backend used strictly for explicit cold-provisioning benchmarks
/// or when `allow_non_cow_fallback = true` on unsupported filesystems.
#[derive(Debug, Default)]
pub struct PortableCopyBackend;

impl PortableCopyBackend {
    pub fn new() -> Self {
        Self
    }
}

impl CoWBackend for PortableCopyBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_name: "portable-full-copy".into(),
            true_kernel_cow: false,
            supports_namespaces: false,
            supports_cgroups: false,
            isolation_level: IsolationLevel::NoneBaselineCopy,
            notes: "Full recursive byte copy (non-CoW baseline/fallback; not a kernel sandbox)"
                .into(),
        }
    }

    fn prepare_base(
        &self,
        base_id: &str,
        source_dir: Option<&Path>,
        bases_dir: &Path,
    ) -> Result<BaseImage> {
        prepare_base_tree(base_id, source_dir, bases_dir)
    }

    fn create_workspace(
        &self,
        sandbox_id: SandboxId,
        base: &BaseImage,
        sandboxes_dir: &Path,
    ) -> Result<WorkspacePaths> {
        let root = sandboxes_dir.join(sandbox_id.to_string());
        let workspace = root.join("workspace");
        fs::create_dir_all(&root).map_err(|e| SnapBoxError::Filesystem {
            operation: "create_dir_all".into(),
            path: root.display().to_string(),
            source: e,
        })?;
        copy_tree_recursive(&base.root_dir, &workspace)?;
        Ok(WorkspacePaths {
            root,
            workspace,
            upper_dir: None,
            work_dir: None,
            backend: "portable-full-copy".into(),
            mounted: false,
        })
    }

    fn reset_workspace(
        &self,
        _sandbox_id: SandboxId,
        paths: &mut WorkspacePaths,
        base: &BaseImage,
    ) -> Result<()> {
        remove_dir_if_exists(&paths.workspace)?;
        copy_tree_recursive(&base.root_dir, &paths.workspace)?;
        Ok(())
    }

    fn destroy_workspace(&self, _sandbox_id: SandboxId, paths: &mut WorkspacePaths) -> Result<()> {
        remove_dir_if_exists(&paths.root)
    }

    fn workspace_disk_usage(&self, paths: &WorkspacePaths) -> Result<u64> {
        directory_apparent_bytes(&paths.workspace)
    }
}

/// Prepares a base image directory on disk and computes its SHA-256 integrity manifest.
pub fn prepare_base_tree(
    base_id: &str,
    source_dir: Option<&Path>,
    bases_dir: &Path,
) -> Result<BaseImage> {
    fs::create_dir_all(bases_dir).map_err(|e| SnapBoxError::Filesystem {
        operation: "create_dir_all".into(),
        path: bases_dir.display().to_string(),
        source: e,
    })?;

    let target = bases_dir.join(base_id);
    if target.exists() {
        remove_dir_if_exists(&target)?;
    }

    match source_dir {
        Some(src) => {
            if !src.is_dir() {
                return Err(SnapBoxError::Validation(format!(
                    "base_source_dir '{}' does not exist or is not a directory",
                    src.display()
                )));
            }
            copy_tree_recursive(src, &target)?;
        }
        None => {
            seed_default_base_tree(&target)?;
        }
    }

    let (manifest_sha256, file_count, total_bytes) = compute_tree_manifest_sha256(&target)?;
    Ok(BaseImage {
        base_id: base_id.to_string(),
        root_dir: target,
        manifest_sha256,
        file_count,
        total_bytes,
    })
}

/// Seeds a deterministic default workspace tree when no custom source directory is configured.
pub fn seed_default_base_tree(target: &Path) -> Result<()> {
    let src_dir = target.join("src");
    let tests_dir = target.join("tests");
    fs::create_dir_all(&src_dir).map_err(|e| SnapBoxError::Filesystem {
        operation: "create_dir_all".into(),
        path: src_dir.display().to_string(),
        source: e,
    })?;
    fs::create_dir_all(&tests_dir).map_err(|e| SnapBoxError::Filesystem {
        operation: "create_dir_all".into(),
        path: tests_dir.display().to_string(),
        source: e,
    })?;

    let files = [
        (
            target.join("README.md"),
            "# snap-box Default Sandbox Environment\n\nIsolated Copy-on-Write workspace.\n",
        ),
        (
            src_dir.join("main.sh"),
            "#!/bin/sh\necho \"hello from snap-box workspace\"\n",
        ),
        (
            tests_dir.join("smoke.sh"),
            "#!/bin/sh\nset -eu\ntest -f README.md\n",
        ),
    ];

    for (path, content) in files {
        fs::write(&path, content).map_err(|e| SnapBoxError::Filesystem {
            operation: "write".into(),
            path: path.display().to_string(),
            source: e,
        })?;
    }
    Ok(())
}

/// Recursively copies a directory tree without following symlinks into host paths.
pub fn copy_tree_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).map_err(|e| SnapBoxError::Filesystem {
        operation: "create_dir_all".into(),
        path: dst.display().to_string(),
        source: e,
    })?;

    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry.map_err(|e| SnapBoxError::Filesystem {
            operation: "walkdir".into(),
            path: src.display().to_string(),
            source: std::io::Error::other(e.to_string()),
        })?;

        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|e| SnapBoxError::Internal(format!("strip_prefix failed: {e}")))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        let ftype = entry.file_type();
        if ftype.is_dir() {
            fs::create_dir_all(&target).map_err(|e| SnapBoxError::Filesystem {
                operation: "create_dir_all".into(),
                path: target.display().to_string(),
                source: e,
            })?;
        } else if ftype.is_file() {
            fs::copy(entry.path(), &target).map_err(|e| SnapBoxError::Filesystem {
                operation: "copy".into(),
                path: target.display().to_string(),
                source: e,
            })?;
        } else if ftype.is_symlink() {
            // Skip external symlinks during base ingestion to prevent host escape vectors.
            tracing::debug!(
                symlink = %entry.path().display(),
                "skipping symlink during base copy"
            );
        }
    }
    Ok(())
}

pub fn remove_dir_if_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_dir_all(path).map_err(|e| SnapBoxError::Filesystem {
        operation: "remove_dir_all".into(),
        path: path.display().to_string(),
        source: e,
    })
}

pub fn directory_apparent_bytes(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(|e| SnapBoxError::Filesystem {
            operation: "walkdir".into(),
            path: path.display().to_string(),
            source: std::io::Error::other(e.to_string()),
        })?;
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

/// Computes a deterministic SHA-256 manifest digest across relative paths and file contents.
pub fn compute_tree_manifest_sha256(root: &Path) -> Result<(String, usize, u64)> {
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|e| SnapBoxError::Filesystem {
            operation: "walkdir".into(),
            path: root.display().to_string(),
            source: std::io::Error::other(e.to_string()),
        })?;
        entries.push(entry.into_path());
    }
    entries.sort();

    let mut hasher = Sha256::new();
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    let mut buf = [0u8; 8192];

    for path in entries {
        let rel = path
            .strip_prefix(root)
            .map_err(|e| SnapBoxError::Internal(format!("strip_prefix failed: {e}")))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let meta = fs::symlink_metadata(&path).map_err(|e| SnapBoxError::Filesystem {
            operation: "symlink_metadata".into(),
            path: path.display().to_string(),
            source: e,
        })?;

        hasher.update(rel.to_string_lossy().as_bytes());
        if meta.is_dir() {
            hasher.update(b":D:");
        } else if meta.is_file() {
            file_count += 1;
            let len = meta.len();
            total_bytes = total_bytes.saturating_add(len);
            hasher.update(b":F:");
            hasher.update(len.to_le_bytes());
            let mut file = fs::File::open(&path).map_err(|e| SnapBoxError::Filesystem {
                operation: "open".into(),
                path: path.display().to_string(),
                source: e,
            })?;
            loop {
                let n = file.read(&mut buf).map_err(|e| SnapBoxError::Filesystem {
                    operation: "read".into(),
                    path: path.display().to_string(),
                    source: e,
                })?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
        }
    }

    Ok((format!("{:x}", hasher.finalize()), file_count, total_bytes))
}
