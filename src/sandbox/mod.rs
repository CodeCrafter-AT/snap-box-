use std::{collections::HashMap, fmt, path::PathBuf, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::lifecycle::{LifecycleStateMachine, SandboxState, StateTransitionRecord};

/// Strongly-typed sandbox identifier wrapping UUID v4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SandboxId(pub Uuid);

impl SandboxId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn short(&self) -> String {
        let s = self.0.to_string();
        s[..8].to_string()
    }
}

impl Default for SandboxId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SandboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for SandboxId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// Filesystem paths managed by a CoW backend for a single sandbox instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspacePaths {
    /// Root container directory for this sandbox's artifacts.
    pub root: PathBuf,
    /// Writable workspace directory exposed to workloads.
    pub workspace: PathBuf,
    /// Optional OverlayFS upper layer directory (Linux).
    pub upper_dir: Option<PathBuf>,
    /// Optional OverlayFS work directory (Linux).
    pub work_dir: Option<PathBuf>,
    /// Name of the CoW backend that created this workspace.
    pub backend: String,
    /// Whether the workspace currently has an active kernel mount (e.g., Linux overlay).
    pub mounted: bool,
}

/// Full runtime record for a sandbox managed by the WarmPoolManager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRecord {
    pub id: SandboxId,
    pub base_id: String,
    pub state: SandboxState,
    pub paths: WorkspacePaths,
    pub owner: Option<String>,
    pub created_at: DateTime<Utc>,
    pub allocated_at: Option<DateTime<Utc>>,
    pub last_used_at: DateTime<Utc>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub reuse_count: u64,
    pub execution_count: u64,
    pub total_exec_ms: u64,
    pub active_pid: Option<u32>,
    pub last_error: Option<String>,
    pub history: Vec<StateTransitionRecord>,
}

impl SandboxRecord {
    pub fn new(base_id: String, paths: WorkspacePaths) -> Self {
        let now = Utc::now();
        Self {
            id: SandboxId::new(),
            base_id,
            state: SandboxState::Initializing,
            paths,
            owner: None,
            created_at: now,
            allocated_at: None,
            last_used_at: now,
            lease_expires_at: None,
            reuse_count: 0,
            execution_count: 0,
            total_exec_ms: 0,
            active_pid: None,
            last_error: None,
            history: Vec::new(),
        }
    }

    pub fn transition(
        &mut self,
        to: SandboxState,
        reason: impl Into<String>,
    ) -> crate::errors::Result<()> {
        let record = LifecycleStateMachine::validate_and_record(
            &self.id.to_string(),
            self.state,
            to,
            reason,
        )?;
        self.state = to;
        self.last_used_at = record.timestamp;
        self.history.push(record);
        if self.history.len() > 64 {
            let excess = self.history.len() - 64;
            self.history.drain(0..excess);
        }
        Ok(())
    }
}

/// Request to execute a command inside an allocated sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRequest {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd_rel: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Captured output and telemetry from executing a command inside a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}
