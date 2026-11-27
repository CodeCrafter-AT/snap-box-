use std::{fs, path::Path, sync::Arc};

use tokio::sync::watch;

use crate::{
    api,
    config::SnapBoxConfig,
    errors::{Result, SnapBoxError},
    filesystem::select_backend,
    metrics::MetricsRegistry,
    pool::WarmPoolManager,
};

/// Core background service orchestrating config, filesystem backend, warm pool, HTTP API, and shutdown.
pub struct SnapBoxDaemon {
    config: SnapBoxConfig,
    pool: Arc<WarmPoolManager>,
    shutdown_tx: watch::Sender<bool>,
}

impl SnapBoxDaemon {
    /// Initializes the CoW backend, prepares the base image, and pre-warms the sandbox pool.
    pub async fn initialize(config: SnapBoxConfig) -> Result<Self> {
        config.validate()?;
        let backend = select_backend(
            &config.storage.data_root,
            config.storage.allow_non_cow_fallback,
        )?;

        let caps = backend.capabilities();
        tracing::info!(
            backend = %caps.backend_name,
            true_cow = caps.true_kernel_cow,
            isolation_level = ?caps.isolation_level,
            "initialized Copy-on-Write filesystem backend"
        );

        let metrics = MetricsRegistry::new();
        let pool = WarmPoolManager::new(config.clone(), backend, metrics)?;
        pool.warm_initial_pool().await?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        pool.start_background_tasks(shutdown_rx);

        Ok(Self {
            config,
            pool,
            shutdown_tx,
        })
    }

    pub fn pool(&self) -> Arc<WarmPoolManager> {
        Arc::clone(&self.pool)
    }

    pub fn trigger_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Runs the Axum HTTP API server until `SIGINT`, `SIGTERM`, or `POST /api/v1/daemon/shutdown`.
    pub async fn serve(self) -> Result<()> {
        write_pid_file(&self.config.daemon.pid_file)?;
        let pid_path = self.config.daemon.pid_file.clone();

        let listener = tokio::net::TcpListener::bind(self.config.daemon.bind_addr)
            .await
            .map_err(|e| {
                SnapBoxError::Daemon(format!(
                    "failed to bind HTTP server on {}: {}",
                    self.config.daemon.bind_addr, e
                ))
            })?;

        tracing::info!(
            bind_addr = %self.config.daemon.bind_addr,
            base_id = %self.config.storage.base_id,
            target_warm = self.config.pool.target_warm,
            "snap-box daemon listening"
        );

        let shutdown_tx = self.shutdown_tx.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let router = api::build_router(Arc::clone(&self.pool), Some(shutdown_tx.clone()));

        let server = axum::serve(listener, router).with_graceful_shutdown(async move {
            tokio::select! {
                _ = wait_for_termination_signal() => {
                    let _ = shutdown_tx.send(true);
                }
                _ = shutdown_rx.changed() => {}
            }
        });

        let serve_res = server
            .await
            .map_err(|e| SnapBoxError::Daemon(format!("HTTP server exited with error: {e}")));

        tracing::info!("shutting down snap-box daemon and cleaning up sandboxes");
        self.pool.shutdown_all().await;
        let _ = fs::remove_file(&pid_path);
        serve_res
    }
}

fn write_pid_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| SnapBoxError::Filesystem {
            operation: "create_dir_all".into(),
            path: parent.display().to_string(),
            source: e,
        })?;
    }
    let pid = std::process::id();
    fs::write(path, format!("{pid}\n")).map_err(|e| SnapBoxError::Filesystem {
        operation: "write_pid_file".into(),
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(())
}

/// Sends SIGTERM to the PID recorded in `pid_file` if it exists.
pub fn stop_daemon_by_pid(pid_file: &Path) -> Result<u32> {
    let raw = fs::read_to_string(pid_file).map_err(|e| {
        SnapBoxError::Daemon(format!(
            "cannot read PID file '{}': {}",
            pid_file.display(),
            e
        ))
    })?;
    let pid: u32 = raw.trim().parse().map_err(|e| {
        SnapBoxError::Daemon(format!("invalid PID in '{}': {e}", pid_file.display()))
    })?;
    // SAFETY: Standard POSIX signal delivery to recorded daemon PID.
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if rc != 0 {
        return Err(SnapBoxError::Daemon(format!(
            "failed to signal PID {pid}: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(pid)
}

async fn wait_for_termination_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
