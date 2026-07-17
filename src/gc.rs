//! Implicit GC within a single project (spec §8.1), used by the `sync`
//! slow path and by `prune`.
//!
//! Reclamation is expressed as a mostly-pure function: liveness (`git
//! worktree list`), directory existence, and port usage are all injected
//! so this module needs no I/O of its own.

use crate::registry::ProjectEntry;
use std::path::{Path, PathBuf};

/// Reclaims every worktree entry in `project` that satisfies all three
/// conditions of spec §8.1:
///
/// 1. `pinned == false`
/// 2. the worktree path is absent from `live_worktrees` *and* `dir_exists`
///    reports it as gone
/// 3. `block_unused` reports every port in the entry's block as unused
///
/// Matching entries are removed from `project.worktrees` and returned as
/// `(worktree_path, block)` pairs.
pub fn collect(
    project: &mut ProjectEntry,
    live_worktrees: &[PathBuf],
    dir_exists: &dyn Fn(&Path) -> bool,
    block_unused: &dyn Fn((u16, u16)) -> bool,
) -> Vec<(String, (u16, u16))> {
    let candidates: Vec<String> = project
        .worktrees
        .iter()
        .filter(|(path, entry)| {
            if entry.pinned {
                return false;
            }
            let is_live = live_worktrees
                .iter()
                .any(|p| p.as_path() == Path::new(path.as_str()));
            if is_live || dir_exists(Path::new(path.as_str())) {
                return false;
            }
            // A pending block (interrupted two-phase move) is part of the
            // entry's footprint: reclaim only when it, too, is unused.
            block_unused(entry.block) && entry.pending_block.map(block_unused).unwrap_or(true)
        })
        .map(|(path, _)| path.clone())
        .collect();

    let mut reclaimed = Vec::with_capacity(candidates.len());
    for path in candidates {
        if let Some(entry) = project.worktrees.remove(&path) {
            reclaimed.push((path, entry.block));
        }
    }
    reclaimed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::WorktreeEntry;
    use chrono::{FixedOffset, TimeZone};
    use std::collections::BTreeMap;

    fn entry(block: (u16, u16), pinned: bool) -> WorktreeEntry {
        let tz = FixedOffset::east_opt(0).unwrap();
        let now = tz.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        WorktreeEntry {
            block,
            generation: 1,
            pending_block: None,
            branch: None,
            manifest_hash: None,
            pinned,
            label: None,
            allocated_at: now,
            last_seen_at: now,
        }
    }

    fn project_with(worktrees: BTreeMap<String, WorktreeEntry>) -> ProjectEntry {
        ProjectEntry {
            name: "p".to_string(),
            worktrees,
        }
    }

    #[test]
    fn collects_when_all_three_conditions_hold() {
        let mut worktrees = BTreeMap::new();
        worktrees.insert("/gone".to_string(), entry((3000, 3004), false));
        let mut project = project_with(worktrees);

        let reclaimed = collect(&mut project, &[], &|_| false, &|_| true);

        assert_eq!(reclaimed, vec![("/gone".to_string(), (3000, 3004))]);
        assert!(project.worktrees.is_empty());
    }

    #[test]
    fn pinned_entry_is_never_collected() {
        let mut worktrees = BTreeMap::new();
        worktrees.insert("/gone".to_string(), entry((3000, 3004), true));
        let mut project = project_with(worktrees);

        let reclaimed = collect(&mut project, &[], &|_| false, &|_| true);

        assert!(reclaimed.is_empty());
        assert_eq!(project.worktrees.len(), 1);
    }

    #[test]
    fn entry_whose_directory_still_exists_is_kept() {
        let mut worktrees = BTreeMap::new();
        worktrees.insert("/exists".to_string(), entry((3000, 3004), false));
        let mut project = project_with(worktrees);

        // Not in the live worktree list, but the directory check reports
        // it as still present.
        let reclaimed = collect(&mut project, &[], &|_| true, &|_| true);

        assert!(reclaimed.is_empty());
    }

    #[test]
    fn entry_present_in_live_worktree_list_is_kept() {
        let mut worktrees = BTreeMap::new();
        worktrees.insert("/still/live".to_string(), entry((3000, 3004), false));
        let mut project = project_with(worktrees);

        let live = vec![PathBuf::from("/still/live")];
        let reclaimed = collect(&mut project, &live, &|_| false, &|_| true);

        assert!(reclaimed.is_empty());
    }

    #[test]
    fn entry_with_ports_still_in_use_is_kept() {
        let mut worktrees = BTreeMap::new();
        worktrees.insert("/gone".to_string(), entry((3000, 3004), false));
        let mut project = project_with(worktrees);

        let reclaimed = collect(&mut project, &[], &|_| false, &|_| false);

        assert!(reclaimed.is_empty());
        assert_eq!(project.worktrees.len(), 1);
    }

    #[test]
    fn only_matching_entries_are_reclaimed_among_several() {
        let mut worktrees = BTreeMap::new();
        worktrees.insert("/gone".to_string(), entry((3000, 3004), false));
        worktrees.insert("/pinned".to_string(), entry((3005, 3009), true));
        worktrees.insert("/live".to_string(), entry((3010, 3014), false));
        worktrees.insert("/port-in-use".to_string(), entry((3015, 3019), false));
        let mut project = project_with(worktrees);

        let live = vec![PathBuf::from("/live")];
        let reclaimed = collect(&mut project, &live, &|_| false, &|block| {
            block != (3015, 3019)
        });

        assert_eq!(reclaimed, vec![("/gone".to_string(), (3000, 3004))]);
        assert_eq!(project.worktrees.len(), 3);
        assert!(project.worktrees.contains_key("/pinned"));
        assert!(project.worktrees.contains_key("/live"));
        assert!(project.worktrees.contains_key("/port-in-use"));
    }
}
