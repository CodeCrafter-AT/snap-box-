#[cfg(target_os = "macos")]
mod imp {
    use std::{
        ffi::{CStr, CString},
        fs,
        os::unix::ffi::OsStrExt,
        path::Path,
    };

    use crate::{
        errors::{Result, SnapBoxError},
        filesystem::{
            directory_apparent_bytes, prepare_base_tree, remove_dir_if_exists, BackendCapabilities,
            BaseImage, CoWBackend, IsolationLevel,
        },
        sandbox::{SandboxId, WorkspacePaths},
    };

    const CLONE_NOFOLLOW: u32 = 0x0001;
    const CLONE_NOFOLLOW_ANY: u32 = 0x0008;

    extern "C" {
        fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32)
            -> libc::c_int;
    }

    /// macOS APFS Copy-on-Write backend powered by `clonefile(2)`.
    ///
    /// Note: APFS `clonefile(2)` provides filesystem-level Copy-on-Write workspace isolation
    /// (O(1) extent-sharing clones). It does NOT provide Linux-style kernel namespaces or VM isolation.
    #[derive(Debug)]
    pub struct MacOsApfsBackend;

    impl MacOsApfsBackend {
        /// Verifies that `data_root` resides on APFS and supports `clonefile(2)`.
        pub fn probe(data_root: &Path) -> Result<Self> {
            fs::create_dir_all(data_root).map_err(|e| SnapBoxError::Filesystem {
                operation: "create_dir_all".into(),
                path: data_root.display().to_string(),
                source: e,
            })?;

            let fstype = statfs_fstypename(data_root)?;
            if fstype != "apfs" {
                return Err(SnapBoxError::UnsupportedPlatform(format!(
                    "data_root '{}' is on filesystem '{fstype}', but macOS CoW requires APFS",
                    data_root.display()
                )));
            }

            let probe_dir =
                tempfile::tempdir_in(data_root).map_err(|e| SnapBoxError::Filesystem {
                    operation: "tempdir_in".into(),
                    path: data_root.display().to_string(),
                    source: e,
                })?;
            let probe_src = probe_dir.path().join("probe_src");
            let probe_dst = probe_dir.path().join("probe_dst");
            fs::write(&probe_src, b"apfs-probe").map_err(|e| SnapBoxError::Filesystem {
                operation: "write".into(),
                path: probe_src.display().to_string(),
                source: e,
            })?;
            apfs_clonefile(&probe_src, &probe_dst)?;

            Ok(Self)
        }
    }

    impl CoWBackend for MacOsApfsBackend {
        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                backend_name: "macos-apfs-clonefile".into(),
                true_kernel_cow: true,
                supports_namespaces: false,
                supports_cgroups: false,
                isolation_level: IsolationLevel::FilesystemCowAndProcessBounds,
                notes: "Uses APFS clonefile(2) with CLONE_NOFOLLOW|CLONE_NOFOLLOW_ANY for instant Copy-on-Write clones. Provides filesystem CoW isolation + userspace env/rlimit/pgid bounds, NOT kernel/VM execution isolation.".into(),
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
            apfs_clonefile(&base.root_dir, &workspace)?;

            Ok(WorkspacePaths {
                root,
                workspace,
                upper_dir: None,
                work_dir: None,
                backend: "macos-apfs-clonefile".into(),
                mounted: false,
            })
        }

        fn reset_workspace(
            &self,
            _sandbox_id: SandboxId,
            paths: &mut WorkspacePaths,
            base: &BaseImage,
        ) -> Result<()> {
            // Fast atomic swap pattern: clone fresh into staging path, swap, remove dirty dir.
            let staging = paths.root.join("workspace.next");
            let trash = paths.root.join("workspace.trash");
            remove_dir_if_exists(&staging)?;
            remove_dir_if_exists(&trash)?;

            apfs_clonefile(&base.root_dir, &staging)?;

            if paths.workspace.exists() {
                fs::rename(&paths.workspace, &trash).map_err(|e| SnapBoxError::Filesystem {
                    operation: "rename_old_workspace".into(),
                    path: paths.workspace.display().to_string(),
                    source: e,
                })?;
            }
            if let Err(e) = fs::rename(&staging, &paths.workspace) {
                // Attempt rollback to preserve valid state if rename fails.
                let _ = fs::rename(&trash, &paths.workspace);
                return Err(SnapBoxError::Filesystem {
                    operation: "rename_staging_workspace".into(),
                    path: paths.workspace.display().to_string(),
                    source: e,
                });
            }
            remove_dir_if_exists(&trash)?;
            Ok(())
        }

        fn destroy_workspace(
            &self,
            _sandbox_id: SandboxId,
            paths: &mut WorkspacePaths,
        ) -> Result<()> {
            remove_dir_if_exists(&paths.root)
        }

        fn workspace_disk_usage(&self, paths: &WorkspacePaths) -> Result<u64> {
            directory_apparent_bytes(&paths.workspace)
        }
    }

    fn apfs_clonefile(src: &Path, dst: &Path) -> Result<()> {
        // Canonicalize parent/source so macOS firmlinks/symlinks like /var -> /private/var
        // or /tmp -> /private/tmp do not trigger ELOOP under CLONE_NOFOLLOW_ANY.
        let canon_src = fs::canonicalize(src).map_err(|e| SnapBoxError::Filesystem {
            operation: "canonicalize".into(),
            path: src.display().to_string(),
            source: e,
        })?;
        let dst_parent = dst.parent().ok_or_else(|| {
            SnapBoxError::Validation(format!("destination '{}' has no parent", dst.display()))
        })?;
        let dst_name = dst.file_name().ok_or_else(|| {
            SnapBoxError::Validation(format!("destination '{}' has no file name", dst.display()))
        })?;
        let canon_dst_parent =
            fs::canonicalize(dst_parent).map_err(|e| SnapBoxError::Filesystem {
                operation: "canonicalize".into(),
                path: dst_parent.display().to_string(),
                source: e,
            })?;
        let resolved_dst = canon_dst_parent.join(dst_name);

        let c_src = CString::new(canon_src.as_os_str().as_bytes()).map_err(|_| {
            SnapBoxError::Validation(format!("source path contains NUL byte: {}", src.display()))
        })?;
        let c_dst = CString::new(resolved_dst.as_os_str().as_bytes()).map_err(|_| {
            SnapBoxError::Validation(format!(
                "destination path contains NUL byte: {}",
                dst.display()
            ))
        })?;

        // SAFETY: Both pointers are valid NUL-terminated CStrings and flags are valid APFS clone flags.
        let rc = unsafe {
            clonefile(
                c_src.as_ptr(),
                c_dst.as_ptr(),
                CLONE_NOFOLLOW | CLONE_NOFOLLOW_ANY,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(SnapBoxError::Filesystem {
                operation: "clonefile".into(),
                path: format!("{} -> {}", src.display(), dst.display()),
                source: err,
            });
        }
        Ok(())
    }

    fn statfs_fstypename(path: &Path) -> Result<String> {
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            SnapBoxError::Validation(format!("path contains NUL byte: {}", path.display()))
        })?;
        let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
        if rc != 0 {
            return Err(SnapBoxError::Filesystem {
                operation: "statfs".into(),
                path: path.display().to_string(),
                source: std::io::Error::last_os_error(),
            });
        }
        let name = unsafe { CStr::from_ptr(buf.f_fstypename.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        Ok(name)
    }
}

#[cfg(target_os = "macos")]
pub use imp::MacOsApfsBackend;
