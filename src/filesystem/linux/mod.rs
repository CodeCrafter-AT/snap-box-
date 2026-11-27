#[cfg(target_os = "linux")]
mod imp {
    use std::{ffi::CString, fs, os::unix::ffi::OsStrExt, path::Path, ptr};

    use crate::{
        errors::{Result, SnapBoxError},
        filesystem::{
            directory_apparent_bytes, prepare_base_tree, remove_dir_if_exists, BackendCapabilities,
            BaseImage, CoWBackend, IsolationLevel,
        },
        sandbox::{SandboxId, WorkspacePaths},
    };

    /// Linux OverlayFS Copy-on-Write backend (`lowerdir` + `upperdir` + `workdir` -> `merged`).
    #[derive(Debug)]
    pub struct LinuxOverlayBackend {
        user_namespaces_available: bool,
    }

    impl LinuxOverlayBackend {
        /// Probes whether kernel OverlayFS is present and whether this process can mount overlayfs.
        pub fn probe(data_root: &Path) -> Result<Self> {
            let filesystems =
                fs::read_to_string("/proc/filesystems").map_err(|e| SnapBoxError::Filesystem {
                    operation: "read".into(),
                    path: "/proc/filesystems".into(),
                    source: e,
                })?;

            if !filesystems
                .lines()
                .any(|l| l.trim_end().ends_with("overlay"))
            {
                return Err(SnapBoxError::UnsupportedPlatform(
                    "Linux kernel does not advertise 'overlay' in /proc/filesystems".into(),
                ));
            }

            fs::create_dir_all(data_root).map_err(|e| SnapBoxError::Filesystem {
                operation: "create_dir_all".into(),
                path: data_root.display().to_string(),
                source: e,
            })?;

            // Perform a real minimal overlay mount probe so we never assume privileges silently.
            let probe_dir =
                tempfile::tempdir_in(data_root).map_err(|e| SnapBoxError::Filesystem {
                    operation: "tempdir_in".into(),
                    path: data_root.display().to_string(),
                    source: e,
                })?;
            let lower = probe_dir.path().join("lower");
            let upper = probe_dir.path().join("upper");
            let work = probe_dir.path().join("work");
            let merged = probe_dir.path().join("merged");
            for d in [&lower, &upper, &work, &merged] {
                fs::create_dir_all(d).map_err(|e| SnapBoxError::Filesystem {
                    operation: "create_dir_all".into(),
                    path: d.display().to_string(),
                    source: e,
                })?;
            }

            mount_overlay(&lower, &upper, &work, &merged).map_err(|err| {
                SnapBoxError::UnsupportedPlatform(format!(
                    "OverlayFS mount probe failed ({err}); ensure CAP_SYS_ADMIN or root/unprivileged overlay mount support is available"
                ))
            })?;
            let _ = umount_overlay(&merged);

            let user_namespaces_available = Path::new("/proc/self/ns/user").exists();
            Ok(Self {
                user_namespaces_available,
            })
        }
    }

    impl CoWBackend for LinuxOverlayBackend {
        fn capabilities(&self) -> BackendCapabilities {
            let cgroups_v2 = Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
            BackendCapabilities {
                backend_name: "linux-overlayfs".into(),
                true_kernel_cow: true,
                supports_namespaces: self.user_namespaces_available,
                supports_cgroups: cgroups_v2,
                isolation_level: IsolationLevel::KernelOverlayAndNamespaces,
                notes: "Uses Linux OverlayFS (read-only lowerdir + writable upperdir) with namespace/cgroup support where kernel permissions allow.".into(),
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
            let upper = root.join("upper");
            let work = root.join("work");
            let workspace = root.join("merged");

            for d in [&upper, &work, &workspace] {
                fs::create_dir_all(d).map_err(|e| SnapBoxError::Filesystem {
                    operation: "create_dir_all".into(),
                    path: d.display().to_string(),
                    source: e,
                })?;
            }

            mount_overlay(&base.root_dir, &upper, &work, &workspace)?;

            Ok(WorkspacePaths {
                root,
                workspace,
                upper_dir: Some(upper),
                work_dir: Some(work),
                backend: "linux-overlayfs".into(),
                mounted: true,
            })
        }

        fn reset_workspace(
            &self,
            _sandbox_id: SandboxId,
            paths: &mut WorkspacePaths,
            base: &BaseImage,
        ) -> Result<()> {
            if paths.mounted {
                umount_overlay(&paths.workspace)?;
                paths.mounted = false;
            }
            let upper = paths
                .upper_dir
                .as_ref()
                .ok_or_else(|| SnapBoxError::Internal("missing overlay upper_dir".into()))?;
            let work = paths
                .work_dir
                .as_ref()
                .ok_or_else(|| SnapBoxError::Internal("missing overlay work_dir".into()))?;

            remove_dir_if_exists(upper)?;
            remove_dir_if_exists(work)?;
            fs::create_dir_all(upper).map_err(|e| SnapBoxError::Filesystem {
                operation: "create_dir_all".into(),
                path: upper.display().to_string(),
                source: e,
            })?;
            fs::create_dir_all(work).map_err(|e| SnapBoxError::Filesystem {
                operation: "create_dir_all".into(),
                path: work.display().to_string(),
                source: e,
            })?;

            mount_overlay(&base.root_dir, upper, work, &paths.workspace)?;
            paths.mounted = true;
            Ok(())
        }

        fn destroy_workspace(
            &self,
            _sandbox_id: SandboxId,
            paths: &mut WorkspacePaths,
        ) -> Result<()> {
            if paths.mounted {
                umount_overlay(&paths.workspace)?;
                paths.mounted = false;
            }
            remove_dir_if_exists(&paths.root)
        }

        fn workspace_disk_usage(&self, paths: &WorkspacePaths) -> Result<u64> {
            // Only upper_dir consumes newly written dirty blocks in OverlayFS.
            match &paths.upper_dir {
                Some(upper) => directory_apparent_bytes(upper),
                None => directory_apparent_bytes(&paths.workspace),
            }
        }
    }

    fn mount_overlay(lower: &Path, upper: &Path, work: &Path, merged: &Path) -> Result<()> {
        let source = CString::new("snapbox-overlay").expect("static string");
        let target = CString::new(merged.as_os_str().as_bytes())
            .map_err(|_| SnapBoxError::Validation("merged path contains NUL byte".into()))?;
        let fstype = CString::new("overlay").expect("static string");
        let options_str = format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        );
        let options = CString::new(options_str).map_err(|_| {
            SnapBoxError::Validation("overlay mount options contain NUL byte".into())
        })?;

        // SAFETY: Valid NUL-terminated C strings passed to Linux mount(2).
        let rc = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                0,
                options.as_ptr().cast(),
            )
        };
        if rc != 0 {
            return Err(SnapBoxError::Filesystem {
                operation: "mount(overlay)".into(),
                path: merged.display().to_string(),
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(())
    }

    fn umount_overlay(merged: &Path) -> Result<()> {
        let target = CString::new(merged.as_os_str().as_bytes())
            .map_err(|_| SnapBoxError::Validation("merged path contains NUL byte".into()))?;
        // SAFETY: Valid NUL-terminated path passed to umount2(2).
        let rc = unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINVAL) && err.raw_os_error() != Some(libc::ENOENT)
            {
                return Err(SnapBoxError::Filesystem {
                    operation: "umount2(MNT_DETACH)".into(),
                    path: merged.display().to_string(),
                    source: err,
                });
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub use imp::LinuxOverlayBackend;
