use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::errors::{Result, SnapBoxError};

/// Deterministic finite states for a sandbox managed by snap-box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SandboxState {
    Initializing,
    Warm,
    Allocated,
    Running,
    Releasing,
    Resetting,
    Failed,
    Destroyed,
}

impl fmt::Display for SandboxState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Initializing => "INITIALIZING",
            Self::Warm => "WARM",
            Self::Allocated => "ALLOCATED",
            Self::Running => "RUNNING",
            Self::Releasing => "RELEASING",
            Self::Resetting => "RESETTING",
            Self::Failed => "FAILED",
            Self::Destroyed => "DESTROYED",
        };
        f.pad(label)
    }
}

/// Audit entry recorded whenever a sandbox changes state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateTransitionRecord {
    pub from: SandboxState,
    pub to: SandboxState,
    pub timestamp: DateTime<Utc>,
    pub reason: String,
}

/// Enforces valid sandbox lifecycle transitions.
pub struct LifecycleStateMachine;

impl LifecycleStateMachine {
    /// Returns true if transitioning from `from` to `to` is valid.
    pub fn can_transition(from: SandboxState, to: SandboxState) -> bool {
        use SandboxState::*;
        matches!(
            (from, to),
            (Initializing, Warm)
                | (Initializing, Failed)
                | (Initializing, Destroyed)
                | (Warm, Allocated)
                | (Warm, Releasing)
                | (Warm, Failed)
                | (Warm, Destroyed)
                | (Allocated, Running)
                | (Allocated, Releasing)
                | (Allocated, Resetting)
                | (Allocated, Failed)
                | (Allocated, Destroyed)
                | (Running, Allocated)
                | (Running, Releasing)
                | (Running, Failed)
                | (Running, Destroyed)
                | (Releasing, Resetting)
                | (Releasing, Destroyed)
                | (Releasing, Failed)
                | (Resetting, Warm)
                | (Resetting, Allocated)
                | (Resetting, Failed)
                | (Resetting, Destroyed)
                | (Failed, Destroyed)
        )
    }

    /// Validates a transition and produces an audit record, or returns `InvalidStateTransition`.
    pub fn validate_and_record(
        sandbox_id: &str,
        from: SandboxState,
        to: SandboxState,
        reason: impl Into<String>,
    ) -> Result<StateTransitionRecord> {
        if !Self::can_transition(from, to) {
            return Err(SnapBoxError::InvalidStateTransition {
                sandbox_id: sandbox_id.to_string(),
                from: from.to_string(),
                to: to.to_string(),
            });
        }
        Ok(StateTransitionRecord {
            from,
            to,
            timestamp: Utc::now(),
            reason: reason.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_lifecycle_paths_succeed() {
        use SandboxState::*;
        assert!(LifecycleStateMachine::can_transition(Initializing, Warm));
        assert!(LifecycleStateMachine::can_transition(Warm, Allocated));
        assert!(LifecycleStateMachine::can_transition(Allocated, Running));
        assert!(LifecycleStateMachine::can_transition(Running, Allocated));
        assert!(LifecycleStateMachine::can_transition(Allocated, Releasing));
        assert!(LifecycleStateMachine::can_transition(Releasing, Resetting));
        assert!(LifecycleStateMachine::can_transition(Resetting, Warm));
        assert!(LifecycleStateMachine::can_transition(Failed, Destroyed));
    }

    #[test]
    fn invalid_transitions_are_rejected() {
        use SandboxState::*;
        assert!(!LifecycleStateMachine::can_transition(Warm, Running));
        assert!(!LifecycleStateMachine::can_transition(Destroyed, Warm));
        assert!(!LifecycleStateMachine::can_transition(Destroyed, Allocated));
        assert!(!LifecycleStateMachine::can_transition(
            Initializing,
            Allocated
        ));
        assert!(!LifecycleStateMachine::can_transition(Failed, Warm));

        let err = LifecycleStateMachine::validate_and_record("sb-1", Warm, Running, "bad jump")
            .unwrap_err();
        assert!(matches!(err, SnapBoxError::InvalidStateTransition { .. }));
    }
}
