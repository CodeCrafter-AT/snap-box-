//! Linux-specific OverlayFS capability & workspace tests.
//! On macOS hosts these tests compile cleanly and stay inactive via `#[cfg(target_os = "linux")]`.

#[cfg(target_os = "linux")]
mod linux_only {
    use snap_box::{
        filesystem::{linux::LinuxOverlayBackend, prepare_base_tree, CoWBackend},
        sandbox::SandboxId,
    };

    #[test]
    fn overlayfs_probe_and_lifecycle_when_permitted() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = tmp.path().join("overlay-data");

        let backend = match LinuxOverlayBackend::probe(&data_root) {
            Ok(b) => b,
            Err(err) => {
                // Report honestly if running inside an unprivileged container without CAP_SYS_ADMIN.
                eprintln!(
                    "Skipping Linux OverlayFS mount test (unsupported in current container): {err}"
                );
                return;
            }
        };

        let bases_dir = data_root.join("bases");
        let sandboxes_dir = data_root.join("sandboxes");
        std::fs::create_dir_all(&sandboxes_dir).unwrap();

        let base = prepare_base_tree("linux-test", None, &bases_dir).unwrap();
        let id = SandboxId::new();
        let mut ws = backend.create_workspace(id, &base, &sandboxes_dir).unwrap();

        assert!(ws.workspace.join("README.md").exists());
        std::fs::write(ws.workspace.join("overlay_dirty.txt"), "dirty").unwrap();
        assert!(!base.root_dir.join("overlay_dirty.txt").exists());

        backend.reset_workspace(id, &mut ws, &base).unwrap();
        assert!(!ws.workspace.join("overlay_dirty.txt").exists());
        assert!(ws.workspace.join("README.md").exists());

        backend.destroy_workspace(id, &mut ws).unwrap();
        assert!(!ws.root.exists());
    }
}
