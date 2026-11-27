use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Instant,
};

use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify, Semaphore};

use crate::{
    allocator::SandboxAllocator,
    config::SnapBoxConfig,
    errors::{Result, SnapBoxError},
    filesystem::{BackendCapabilities, BaseImage, CoWBackend},
    lifecycle::SandboxState,
    metrics::{MetricsRegistry, OperationKind},
    resources::{collect_host_telemetry, HostTelemetry},
    sandbox::{CommandRequest, CommandResult, SandboxId, SandboxRecord, WorkspacePaths},
    security,
};

#[derive(Debug, Default)]
struct PoolState {
    records: HashMap<SandboxId, SandboxRecord>,
    warm_queue: VecDeque<SandboxId>,
}

/// Point-in-time status summary of the warm pool and underlying host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolStatus {
    pub base_id: String,
    pub base_manifest_sha256: String,
    pub base_file_count: usize,
    pub base_bytes: u64,
    pub backend: BackendCapabilities,
    pub min_warm: usize,
    pub target_warm: usize,
    pub max_sandboxes: usize,
    pub total_sandboxes: usize,
    pub warm_count: usize,
    pub allocated_count: usize,
    pub running_count: usize,
    pub initializing_count: usize,
    pub resetting_count: usize,
    pub releasing_count: usize,
    pub failed_count: usize,
    pub host_telemetry: HostTelemetry,
}

/// Coordinates pre-warmed CoW sandboxes, concurrent allocations, resets, replenishment, and reaping.
pub struct WarmPoolManager {
    config: SnapBoxConfig,
    backend: Arc<dyn CoWBackend>,
    base: BaseImage,
    allocator: SandboxAllocator,
    metrics: MetricsRegistry,
    state: Mutex<PoolState>,
    provision_slots: Arc<Semaphore>,
    replenish_notify: Arc<Notify>,
}

impl WarmPoolManager {
    /// Creates a new pool manager and prepares the immutable base environment.
    pub fn new(
        config: SnapBoxConfig,
        backend: Arc<dyn CoWBackend>,
        metrics: MetricsRegistry,
    ) -> Result<Arc<Self>> {
        config.validate()?;
        std::fs::create_dir_all(config.sandboxes_dir()).map_err(|e| SnapBoxError::Filesystem {
            operation: "create_dir_all".into(),
            path: config.sandboxes_dir().display().to_string(),
            source: e,
        })?;

        let base = backend.prepare_base(
            &config.storage.base_id,
            config.storage.base_source_dir.as_deref(),
            &config.bases_dir(),
        )?;

        Ok(Arc::new(Self {
            provision_slots: Arc::new(Semaphore::new(config.pool.max_concurrent_provisions)),
            allocator: SandboxAllocator::new(config.pool.allocation_policy),
            config,
            backend,
            base,
            metrics,
            state: Mutex::new(PoolState::default()),
            replenish_notify: Arc::new(Notify::new()),
        }))
    }

    pub fn config(&self) -> &SnapBoxConfig {
        &self.config
    }

    pub fn base_image(&self) -> &BaseImage {
        &self.base
    }

    pub fn backend(&self) -> &Arc<dyn CoWBackend> {
        &self.backend
    }

    pub fn metrics(&self) -> &MetricsRegistry {
        &self.metrics
    }

    /// Pre-warms up to `target_warm` sandboxes concurrently during daemon startup.
    pub async fn warm_initial_pool(self: &Arc<Self>) -> Result<()> {
        let target = self.config.pool.target_warm;
        let mut handles = Vec::with_capacity(target);
        for _ in 0..target {
            let pool = Arc::clone(self);
            handles.push(tokio::spawn(async move { pool.provision_warm_one().await }));
        }
        for handle in handles {
            match handle.await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => return Err(err),
                Err(join_err) => {
                    return Err(SnapBoxError::Internal(format!(
                        "initial warm task panicked: {join_err}"
                    )))
                }
            }
        }
        Ok(())
    }

    /// Spawns background replenishment and lease-reaper tasks bound to `shutdown_rx`.
    pub fn start_background_tasks(self: &Arc<Self>, shutdown_rx: watch::Receiver<bool>) {
        let pool_rep = Arc::clone(self);
        let mut rx_rep = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = pool_rep.replenish_notify.notified() => {
                        pool_rep.replenish_to_target().await;
                    }
                    _ = rx_rep.changed() => {
                        if *rx_rep.borrow() {
                            break;
                        }
                    }
                }
            }
        });

        let pool_reap = Arc::clone(self);
        let mut rx_reap = shutdown_rx;
        let interval =
            std::time::Duration::from_millis(self.config.pool.reaper_interval_ms.max(50));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        pool_reap.reap_tick().await;
                    }
                    _ = rx_reap.changed() => {
                        if *rx_reap.borrow() {
                            break;
                        }
                    }
                }
            }
        });
    }

    /// Allocates a sandbox atomically from the warm pool (or provisions on demand if pool allows).
    pub async fn allocate(
        self: &Arc<Self>,
        owner: Option<String>,
        ttl_secs: Option<u64>,
    ) -> Result<SandboxRecord> {
        let started = Instant::now();
        let ttl = ChronoDuration::seconds(
            ttl_secs
                .unwrap_or(self.config.pool.default_lease_ttl_secs)
                .max(1) as i64,
        );

        let fast_selected = {
            let mut guard = self.state.lock().expect("pool mutex poisoned");
            let PoolState {
                records,
                warm_queue,
            } = &mut *guard;

            if let Some(id) = self.allocator.select_warm(warm_queue, records) {
                if let Some(rec) = records.get_mut(&id) {
                    rec.transition(SandboxState::Allocated, "allocated from warm pool")?;
                    let now = Utc::now();
                    rec.owner = owner.clone();
                    rec.allocated_at = Some(now);
                    rec.lease_expires_at = Some(now + ttl);
                    Some(rec.clone())
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some(rec) = fast_selected {
            self.replenish_notify.notify_one();
            self.metrics
                .record_success(OperationKind::Allocation, started.elapsed());
            return Ok(rec);
        }

        // No warm sandbox currently ready; try bounded on-demand CoW provision if below max_sandboxes.
        match self.provision_on_demand(owner, ttl).await {
            Ok(rec) => {
                self.replenish_notify.notify_one();
                self.metrics
                    .record_success(OperationKind::Allocation, started.elapsed());
                Ok(rec)
            }
            Err(err) => {
                self.metrics
                    .record_failure(OperationKind::Allocation, started.elapsed());
                Err(err)
            }
        }
    }

    /// Executes a command inside an allocated sandbox workspace with security & resource bounds.
    pub async fn execute(
        self: &Arc<Self>,
        id: SandboxId,
        req: CommandRequest,
    ) -> Result<CommandResult> {
        let started = Instant::now();
        let workspace = {
            let mut guard = self.state.lock().expect("pool mutex poisoned");
            let rec = guard
                .records
                .get_mut(&id)
                .ok_or_else(|| SnapBoxError::SandboxNotFound(id.to_string()))?;
            rec.transition(
                SandboxState::Running,
                format!("executing '{}'", req.program),
            )?;
            rec.paths.workspace.clone()
        };

        let exec_res = security::execute_sandboxed_command(
            &workspace,
            &req,
            &self.config.security,
            &self.config.resources,
        )
        .await;

        let mut guard = self.state.lock().expect("pool mutex poisoned");
        if let Some(rec) = guard.records.get_mut(&id) {
            if rec.state == SandboxState::Running {
                match &exec_res {
                    Ok(res) => {
                        rec.execution_count += 1;
                        rec.total_exec_ms = rec.total_exec_ms.saturating_add(res.duration_ms);
                        let _ = rec.transition(SandboxState::Allocated, "command completed");
                    }
                    Err(err) => {
                        rec.last_error = Some(err.to_string());
                        let _ = rec.transition(SandboxState::Allocated, "command errored");
                    }
                }
            }
        }

        match &exec_res {
            Ok(_) => self
                .metrics
                .record_success(OperationKind::CommandExec, started.elapsed()),
            Err(_) => self
                .metrics
                .record_failure(OperationKind::CommandExec, started.elapsed()),
        }

        exec_res
    }

    /// Releases an allocated sandbox, discarding temporary changes and returning it to the warm pool
    /// (or destroying it if the pool is already at target capacity / max reuses reached).
    pub async fn release(self: &Arc<Self>, id: SandboxId) -> Result<SandboxRecord> {
        let started = Instant::now();
        let (mut paths, should_recycle, pid_to_kill) = {
            let mut guard = self.state.lock().expect("pool mutex poisoned");
            let warm_plus_resetting = guard
                .records
                .values()
                .filter(|r| {
                    matches!(
                        r.state,
                        SandboxState::Warm | SandboxState::Resetting | SandboxState::Initializing
                    )
                })
                .count();

            let rec = guard
                .records
                .get_mut(&id)
                .ok_or_else(|| SnapBoxError::SandboxNotFound(id.to_string()))?;

            if rec.state == SandboxState::Warm {
                return Err(SnapBoxError::InvalidStateTransition {
                    sandbox_id: id.to_string(),
                    from: rec.state.to_string(),
                    to: SandboxState::Releasing.to_string(),
                });
            }

            let active_pid = rec.active_pid.take();
            rec.transition(SandboxState::Releasing, "client released sandbox")?;

            let can_recycle = rec.reuse_count < self.config.pool.max_reuses_per_sandbox
                && warm_plus_resetting < self.config.pool.target_warm;

            if can_recycle {
                rec.transition(SandboxState::Resetting, "resetting workspace for warm pool")?;
            }

            (rec.paths.clone(), can_recycle, active_pid)
        };

        if let Some(pid) = pid_to_kill {
            security::kill_process_group(pid);
        }

        if should_recycle {
            let reset_start = Instant::now();
            let backend = Arc::clone(&self.backend);
            let base = self.base.clone();
            let res = tokio::task::spawn_blocking(move || {
                backend.reset_workspace(id, &mut paths, &base)?;
                Ok::<WorkspacePaths, SnapBoxError>(paths)
            })
            .await
            .map_err(|e| SnapBoxError::Internal(format!("reset worker panicked: {e}")))?;

            match res {
                Ok(updated_paths) => {
                    self.metrics
                        .record_success(OperationKind::Reset, reset_start.elapsed());
                    let mut guard = self.state.lock().expect("pool mutex poisoned");
                    if let Some(rec) = guard.records.get_mut(&id) {
                        rec.paths = updated_paths;
                        rec.owner = None;
                        rec.allocated_at = None;
                        rec.lease_expires_at = None;
                        rec.reuse_count += 1;
                        rec.transition(SandboxState::Warm, "returned to warm pool")?;
                        let snapshot = rec.clone();
                        guard.warm_queue.push_back(id);
                        self.metrics
                            .record_success(OperationKind::Release, started.elapsed());
                        return Ok(snapshot);
                    }
                    return Err(SnapBoxError::SandboxNotFound(id.to_string()));
                }
                Err(err) => {
                    self.metrics
                        .record_failure(OperationKind::Reset, reset_start.elapsed());
                    self.mark_failed(id, err.to_string());
                    self.metrics
                        .record_failure(OperationKind::Release, started.elapsed());
                    return Err(err);
                }
            }
        }

        // Pool already at target warm capacity or max reuse limit reached: destroy workspace cleanly.
        let snapshot = self.finalize_destroy(id, paths).await?;
        self.metrics
            .record_success(OperationKind::Release, started.elapsed());
        self.replenish_notify.notify_one();
        Ok(snapshot)
    }

    /// Resets an allocated sandbox's workspace in-place while retaining the client's lease.
    pub async fn reset_in_place(self: &Arc<Self>, id: SandboxId) -> Result<SandboxRecord> {
        let started = Instant::now();
        let mut paths = {
            let mut guard = self.state.lock().expect("pool mutex poisoned");
            let rec = guard
                .records
                .get_mut(&id)
                .ok_or_else(|| SnapBoxError::SandboxNotFound(id.to_string()))?;
            if rec.state != SandboxState::Allocated {
                return Err(SnapBoxError::InvalidStateTransition {
                    sandbox_id: id.to_string(),
                    from: rec.state.to_string(),
                    to: SandboxState::Resetting.to_string(),
                });
            }
            rec.transition(SandboxState::Resetting, "in-place reset requested")?;
            rec.paths.clone()
        };

        let backend = Arc::clone(&self.backend);
        let base = self.base.clone();
        let res = tokio::task::spawn_blocking(move || {
            backend.reset_workspace(id, &mut paths, &base)?;
            Ok::<WorkspacePaths, SnapBoxError>(paths)
        })
        .await
        .map_err(|e| SnapBoxError::Internal(format!("reset worker panicked: {e}")))?;

        match res {
            Ok(updated_paths) => {
                let mut guard = self.state.lock().expect("pool mutex poisoned");
                let rec = guard
                    .records
                    .get_mut(&id)
                    .ok_or_else(|| SnapBoxError::SandboxNotFound(id.to_string()))?;
                rec.paths = updated_paths;
                rec.reuse_count += 1;
                rec.transition(SandboxState::Allocated, "in-place reset completed")?;
                let snap = rec.clone();
                self.metrics
                    .record_success(OperationKind::Reset, started.elapsed());
                Ok(snap)
            }
            Err(err) => {
                self.mark_failed(id, err.to_string());
                self.metrics
                    .record_failure(OperationKind::Reset, started.elapsed());
                Err(err)
            }
        }
    }

    /// Destroys a sandbox immediately and removes its workspace from disk.
    pub async fn destroy(self: &Arc<Self>, id: SandboxId) -> Result<SandboxRecord> {
        let (paths, pid_to_kill) = {
            let mut guard = self.state.lock().expect("pool mutex poisoned");
            let rec = guard
                .records
                .get_mut(&id)
                .ok_or_else(|| SnapBoxError::SandboxNotFound(id.to_string()))?;
            let pid = rec.active_pid.take();
            let p = rec.paths.clone();
            guard.warm_queue.retain(|q_id| *q_id != id);
            (p, pid)
        };

        if let Some(pid) = pid_to_kill {
            security::kill_process_group(pid);
        }

        let snap = self.finalize_destroy(id, paths).await?;
        self.replenish_notify.notify_one();
        Ok(snap)
    }

    pub fn get(&self, id: SandboxId) -> Result<SandboxRecord> {
        let guard = self.state.lock().expect("pool mutex poisoned");
        guard
            .records
            .get(&id)
            .cloned()
            .ok_or_else(|| SnapBoxError::SandboxNotFound(id.to_string()))
    }

    pub fn list(&self) -> Vec<SandboxRecord> {
        let guard = self.state.lock().expect("pool mutex poisoned");
        let mut items: Vec<SandboxRecord> = guard.records.values().cloned().collect();
        items.sort_by_key(|r| r.created_at);
        items
    }

    pub fn status(&self) -> PoolStatus {
        let guard = self.state.lock().expect("pool mutex poisoned");
        let mut warm_count = 0;
        let mut allocated_count = 0;
        let mut running_count = 0;
        let mut initializing_count = 0;
        let mut resetting_count = 0;
        let mut releasing_count = 0;
        let mut failed_count = 0;

        for rec in guard.records.values() {
            match rec.state {
                SandboxState::Warm => warm_count += 1,
                SandboxState::Allocated => allocated_count += 1,
                SandboxState::Running => running_count += 1,
                SandboxState::Initializing => initializing_count += 1,
                SandboxState::Resetting => resetting_count += 1,
                SandboxState::Releasing => releasing_count += 1,
                SandboxState::Failed => failed_count += 1,
                SandboxState::Destroyed => {}
            }
        }

        PoolStatus {
            base_id: self.base.base_id.clone(),
            base_manifest_sha256: self.base.manifest_sha256.clone(),
            base_file_count: self.base.file_count,
            base_bytes: self.base.total_bytes,
            backend: self.backend.capabilities(),
            min_warm: self.config.pool.min_warm,
            target_warm: self.config.pool.target_warm,
            max_sandboxes: self.config.pool.max_sandboxes,
            total_sandboxes: guard.records.len(),
            warm_count,
            allocated_count,
            running_count,
            initializing_count,
            resetting_count,
            releasing_count,
            failed_count,
            host_telemetry: collect_host_telemetry(&self.config.storage.data_root),
        }
    }

    pub fn workspace_disk_usage(&self, id: SandboxId) -> Result<u64> {
        let paths = {
            let guard = self.state.lock().expect("pool mutex poisoned");
            guard
                .records
                .get(&id)
                .ok_or_else(|| SnapBoxError::SandboxNotFound(id.to_string()))?
                .paths
                .clone()
        };
        self.backend.workspace_disk_usage(&paths)
    }

    /// Gracefully shuts down and destroys all tracked sandboxes.
    pub async fn shutdown_all(self: &Arc<Self>) {
        let ids: Vec<SandboxId> = {
            let guard = self.state.lock().expect("pool mutex poisoned");
            guard.records.keys().copied().collect()
        };
        for id in ids {
            if let Err(err) = self.destroy(id).await {
                tracing::warn!(sandbox_id = %id, error = %err, "failed to destroy sandbox during shutdown");
            }
        }
    }

    async fn provision_warm_one(self: &Arc<Self>) -> Result<SandboxId> {
        let started = Instant::now();
        let _permit = self
            .provision_slots
            .acquire()
            .await
            .map_err(|_| SnapBoxError::Internal("provision semaphore closed".into()))?;

        let placeholder_id = SandboxId::new();
        {
            let mut guard = self.state.lock().expect("pool mutex poisoned");
            if guard.records.len() >= self.config.pool.max_sandboxes {
                return Err(SnapBoxError::PoolExhausted {
                    max_sandboxes: self.config.pool.max_sandboxes,
                    allocated: guard.records.len(),
                });
            }
            let placeholder_paths = WorkspacePaths {
                root: self.config.sandboxes_dir().join(placeholder_id.to_string()),
                workspace: self
                    .config
                    .sandboxes_dir()
                    .join(placeholder_id.to_string())
                    .join("workspace"),
                upper_dir: None,
                work_dir: None,
                backend: self.backend.capabilities().backend_name,
                mounted: false,
            };
            let mut rec = SandboxRecord::new(self.base.base_id.clone(), placeholder_paths);
            rec.id = placeholder_id;
            guard.records.insert(placeholder_id, rec);
        }

        let clone_started = Instant::now();
        let backend = Arc::clone(&self.backend);
        let base = self.base.clone();
        let sandboxes_dir = self.config.sandboxes_dir();
        let clone_res = tokio::task::spawn_blocking(move || {
            backend.create_workspace(placeholder_id, &base, &sandboxes_dir)
        })
        .await
        .map_err(|e| SnapBoxError::Internal(format!("clone worker panicked: {e}")))?;

        match clone_res {
            Ok(paths) => {
                self.metrics
                    .record_success(OperationKind::WorkspaceClone, clone_started.elapsed());
                let mut guard = self.state.lock().expect("pool mutex poisoned");
                if let Some(rec) = guard.records.get_mut(&placeholder_id) {
                    rec.paths = paths;
                    rec.transition(SandboxState::Warm, "workspace cloned and ready")?;
                    guard.warm_queue.push_back(placeholder_id);
                }
                self.metrics
                    .record_success(OperationKind::Replenish, started.elapsed());
                Ok(placeholder_id)
            }
            Err(err) => {
                self.metrics
                    .record_failure(OperationKind::WorkspaceClone, clone_started.elapsed());
                self.metrics
                    .record_failure(OperationKind::Replenish, started.elapsed());
                self.mark_failed(placeholder_id, err.to_string());
                Err(err)
            }
        }
    }

    async fn provision_on_demand(
        self: &Arc<Self>,
        owner: Option<String>,
        ttl: ChronoDuration,
    ) -> Result<SandboxRecord> {
        let id = self.provision_warm_one().await?;
        let mut guard = self.state.lock().expect("pool mutex poisoned");
        guard.warm_queue.retain(|q| *q != id);
        let rec = guard
            .records
            .get_mut(&id)
            .ok_or_else(|| SnapBoxError::SandboxNotFound(id.to_string()))?;
        rec.transition(SandboxState::Allocated, "on-demand allocation")?;
        let now = Utc::now();
        rec.owner = owner;
        rec.allocated_at = Some(now);
        rec.lease_expires_at = Some(now + ttl);
        Ok(rec.clone())
    }

    async fn replenish_to_target(self: &Arc<Self>) {
        loop {
            let deficit = {
                let guard = self.state.lock().expect("pool mutex poisoned");
                let ready_or_pending = guard
                    .records
                    .values()
                    .filter(|r| {
                        matches!(
                            r.state,
                            SandboxState::Warm
                                | SandboxState::Initializing
                                | SandboxState::Resetting
                        )
                    })
                    .count();
                let remaining_capacity = self
                    .config
                    .pool
                    .max_sandboxes
                    .saturating_sub(guard.records.len());
                self.config
                    .pool
                    .target_warm
                    .saturating_sub(ready_or_pending)
                    .min(remaining_capacity)
            };

            if deficit == 0 {
                break;
            }

            let batch = deficit.min(self.config.pool.max_concurrent_provisions);
            let mut handles = Vec::with_capacity(batch);
            for _ in 0..batch {
                let pool = Arc::clone(self);
                handles.push(tokio::spawn(async move { pool.provision_warm_one().await }));
            }
            for handle in handles {
                if let Ok(Err(err)) = handle.await {
                    tracing::warn!(error = %err, "replenish provision failed");
                    break;
                }
            }
        }
    }

    async fn reap_tick(self: &Arc<Self>) {
        let now = Utc::now();
        let idle_cutoff = now - ChronoDuration::seconds(self.config.pool.idle_timeout_secs as i64);

        let (expired_allocated, failed_ids, excess_idle_warm, need_replenish) = {
            let guard = self.state.lock().expect("pool mutex poisoned");
            let mut expired_allocated = Vec::new();
            let mut failed_ids = Vec::new();
            let mut warm_candidates = Vec::new();

            for rec in guard.records.values() {
                match rec.state {
                    SandboxState::Allocated => {
                        if rec.lease_expires_at.is_some_and(|exp| exp <= now) {
                            expired_allocated.push(rec.id);
                        }
                    }
                    SandboxState::Failed => {
                        failed_ids.push(rec.id);
                    }
                    SandboxState::Warm if rec.last_used_at <= idle_cutoff => {
                        warm_candidates.push(rec.id);
                    }
                    _ => {}
                }
            }

            let total_warm = guard
                .records
                .values()
                .filter(|r| r.state == SandboxState::Warm)
                .count();
            let trim_limit = total_warm.saturating_sub(self.config.pool.target_warm);
            warm_candidates.truncate(trim_limit);

            let ready_or_pending = guard
                .records
                .values()
                .filter(|r| {
                    matches!(
                        r.state,
                        SandboxState::Warm | SandboxState::Initializing | SandboxState::Resetting
                    )
                })
                .count();
            let need_replenish = ready_or_pending < self.config.pool.min_warm
                && guard.records.len() < self.config.pool.max_sandboxes;

            (
                expired_allocated,
                failed_ids,
                warm_candidates,
                need_replenish,
            )
        };

        for id in expired_allocated {
            tracing::info!(sandbox_id = %id, "reaping expired sandbox lease");
            if let Err(err) = self.release(id).await {
                tracing::warn!(sandbox_id = %id, error = %err, "failed to release expired sandbox");
            }
        }
        for id in failed_ids {
            tracing::warn!(sandbox_id = %id, "cleaning up failed sandbox");
            let _ = self.destroy(id).await;
        }
        for id in excess_idle_warm {
            tracing::debug!(sandbox_id = %id, "trimming excess idle warm sandbox");
            let _ = self.destroy(id).await;
        }
        if need_replenish {
            self.replenish_notify.notify_one();
        }
    }

    fn mark_failed(&self, id: SandboxId, reason: String) {
        let mut guard = self.state.lock().expect("pool mutex poisoned");
        guard.warm_queue.retain(|q| *q != id);
        if let Some(rec) = guard.records.get_mut(&id) {
            rec.last_error = Some(reason.clone());
            let _ = rec.transition(SandboxState::Failed, reason);
        }
    }

    async fn finalize_destroy(
        &self,
        id: SandboxId,
        mut paths: WorkspacePaths,
    ) -> Result<SandboxRecord> {
        let backend = Arc::clone(&self.backend);
        let destroy_res = tokio::task::spawn_blocking(move || {
            backend.destroy_workspace(id, &mut paths)?;
            Ok::<WorkspacePaths, SnapBoxError>(paths)
        })
        .await
        .map_err(|e| SnapBoxError::Internal(format!("destroy worker panicked: {e}")))?;

        let updated_paths = match destroy_res {
            Ok(p) => p,
            Err(err) => {
                self.mark_failed(id, err.to_string());
                return Err(err);
            }
        };

        let mut guard = self.state.lock().expect("pool mutex poisoned");
        guard.warm_queue.retain(|q| *q != id);
        let mut rec = guard
            .records
            .remove(&id)
            .ok_or_else(|| SnapBoxError::SandboxNotFound(id.to_string()))?;
        rec.paths = updated_paths;
        let _ = rec.transition(SandboxState::Destroyed, "workspace destroyed");
        Ok(rec)
    }
}
