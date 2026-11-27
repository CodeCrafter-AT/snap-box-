use std::collections::{HashMap, VecDeque};

use crate::{
    config::AllocationPolicy,
    lifecycle::SandboxState,
    sandbox::{SandboxId, SandboxRecord},
};

/// Policy-driven, thread-safe selector for picking an available warm sandbox.
#[derive(Debug, Clone)]
pub struct SandboxAllocator {
    policy: AllocationPolicy,
}

impl SandboxAllocator {
    pub fn new(policy: AllocationPolicy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> AllocationPolicy {
        self.policy
    }

    /// Selects and removes the next eligible `Warm` sandbox from `warm_queue`.
    /// Stale queue entries that are no longer `Warm` are discarded automatically.
    pub fn select_warm(
        &self,
        warm_queue: &mut VecDeque<SandboxId>,
        records: &HashMap<SandboxId, SandboxRecord>,
    ) -> Option<SandboxId> {
        // Purge any non-warm entries from the front/back first.
        warm_queue.retain(|id| {
            records
                .get(id)
                .is_some_and(|rec| rec.state == SandboxState::Warm)
        });

        if warm_queue.is_empty() {
            return None;
        }

        match self.policy {
            AllocationPolicy::Fifo => warm_queue.pop_front(),
            AllocationPolicy::MostRecentlyReset => warm_queue.pop_back(),
            AllocationPolicy::LeastRecentlyUsed => {
                let mut best_idx = 0;
                let mut best_ts = None;
                for (idx, id) in warm_queue.iter().enumerate() {
                    if let Some(rec) = records.get(id) {
                        if best_ts.map_or(true, |cur| rec.last_used_at < cur) {
                            best_ts = Some(rec.last_used_at);
                            best_idx = idx;
                        }
                    }
                }
                warm_queue.remove(best_idx)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration as ChronoDuration;

    use super::*;
    use crate::sandbox::WorkspacePaths;

    fn make_warm_record(age_secs: i64) -> SandboxRecord {
        let paths = WorkspacePaths {
            root: "/tmp/sb".into(),
            workspace: "/tmp/sb/ws".into(),
            upper_dir: None,
            work_dir: None,
            backend: "test".into(),
            mounted: false,
        };
        let mut rec = SandboxRecord::new("default".into(), paths);
        rec.transition(SandboxState::Warm, "ready").unwrap();
        rec.last_used_at -= ChronoDuration::seconds(age_secs);
        rec
    }

    #[test]
    fn fifo_selects_oldest_warm_entry() {
        let alloc = SandboxAllocator::new(AllocationPolicy::Fifo);
        let r1 = make_warm_record(10);
        let r2 = make_warm_record(5);
        let mut q = VecDeque::from([r1.id, r2.id]);
        let mut map = HashMap::new();
        map.insert(r1.id, r1.clone());
        map.insert(r2.id, r2);

        assert_eq!(alloc.select_warm(&mut q, &map), Some(r1.id));
    }

    #[test]
    fn mrr_selects_newest_warm_entry() {
        let alloc = SandboxAllocator::new(AllocationPolicy::MostRecentlyReset);
        let r1 = make_warm_record(10);
        let r2 = make_warm_record(5);
        let mut q = VecDeque::from([r1.id, r2.id]);
        let mut map = HashMap::new();
        map.insert(r1.id, r1);
        map.insert(r2.id, r2.clone());

        assert_eq!(alloc.select_warm(&mut q, &map), Some(r2.id));
    }

    #[test]
    fn lru_selects_least_recently_used() {
        let alloc = SandboxAllocator::new(AllocationPolicy::LeastRecentlyUsed);
        let r1 = make_warm_record(5);
        let r2 = make_warm_record(60);
        let mut q = VecDeque::from([r1.id, r2.id]);
        let mut map = HashMap::new();
        map.insert(r1.id, r1);
        map.insert(r2.id, r2.clone());

        assert_eq!(alloc.select_warm(&mut q, &map), Some(r2.id));
    }
}
