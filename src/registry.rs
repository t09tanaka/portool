//! The `registry.json` ledger schema (spec §5) and pure query helpers.

use crate::error::{Error, Result};
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The full ledger. Keyed collections use `BTreeMap` so that (de)serialized
/// output has a deterministic key order. `deny_unknown_fields` throughout
/// means a *downgrade* (an older binary reading a newer ledger) fails loudly
/// rather than silently dropping fields it doesn't understand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub version: u32,
    /// The pool recorded at ledger-creation time (informational only; live
    /// allocation always consults the current `Config`, and a block outside
    /// this recorded range is a `doctor` advisory, never a corruption).
    pub range: (u16, u16),
    /// A single monotonic counter for the whole ledger, incremented by every
    /// [`crate::store::save`] (schema v4, external review v0.10 P0-2). Mirrored
    /// into each `.env.portool` header, it lets a restore-from-stale-backup be
    /// detected: a live worktree whose env records a *higher* sequence than the
    /// ledger proves the ledger was rolled back, so allocation is quarantined
    /// until `doctor --repair` reconciles.
    #[serde(default)]
    pub sequence: u64,
    /// Keyed by `realpath(git rev-parse --git-common-dir)`.
    pub projects: BTreeMap<String, ProjectEntry>,
    pub reservations: Vec<Reservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectEntry {
    /// Display name, inferred from the common-dir's parent directory name.
    pub name: String,
    /// Keyed by `realpath(worktree root)`.
    pub worktrees: BTreeMap<String, WorktreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeEntry {
    pub block: (u16, u16),
    /// Monotonic counter, bumped every time `block` changes (schema v3).
    /// Mirrored into the `.env.portool` header, and re-checked by the sync
    /// fast path's snapshot revalidation to detect a concurrent move.
    pub generation: u64,
    /// A target block reserved mid-move (schema v3, two-phase update): the
    /// worktree still owns `block`, but `pending_block` is also excluded
    /// from allocation until the move is finalized or rolled back -- so a
    /// crash between the ledger write and the `.env.portool` write can
    /// never leave either block up for grabs. A pending block may overlap
    /// this entry's *own* `block` (a grow-in-place move), never anything
    /// else.
    pub pending_block: Option<(u16, u16)>,
    /// `None` for detached HEAD.
    pub branch: Option<String>,
    pub manifest_hash: Option<String>,
    pub pinned: bool,
    pub label: Option<String>,
    pub allocated_at: DateTime<FixedOffset>,
    pub last_seen_at: DateTime<FixedOffset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub block: (u16, u16),
    pub label: Option<String>,
    pub pinned: bool,
}

impl Registry {
    /// The schema version this build reads and writes. v2 dropped the
    /// per-project `subranges` field (hardening batch C); v3 added
    /// per-worktree `generation` and `pending_block` for the two-phase
    /// ledger/env update; v4 added the top-level monotonic `sequence`
    /// (external review v0.10 P0-2).
    pub const CURRENT_VERSION: u32 = 4;

    /// A freshly created, empty ledger recording `range` for informational
    /// purposes.
    pub fn empty(range: (u16, u16)) -> Registry {
        Registry {
            version: Self::CURRENT_VERSION,
            range,
            sequence: 0,
            projects: BTreeMap::new(),
            reservations: Vec::new(),
        }
    }

    /// Parses a ledger from JSON, migrating older schema versions to the
    /// current one **in memory** (hardening batch C). A v1 ledger drops its
    /// per-project `subranges`; a v1/v2 ledger gains `generation = 1` and
    /// no pending block. Every worktree `block` is preserved verbatim --
    /// blocks are absolute, so no ports move on upgrade. Callers persist
    /// the migrated form only on a locked write; a read-only caller sees
    /// the current schema in memory and leaves the old file on disk.
    ///
    /// An unparseable body is a general error, which callers treat as
    /// corruption; an unrecognized version is the distinct
    /// [`Error::UnsupportedRegistryVersion`], so a ledger written by a
    /// newer portool is never mistaken for a corrupt one.
    pub fn from_json(s: &str) -> Result<Registry> {
        #[derive(Deserialize)]
        struct VersionPeek {
            version: u32,
        }
        let peek: VersionPeek = serde_json::from_str(s)?;
        match peek.version {
            4 => Ok(serde_json::from_str::<Registry>(s)?),
            3 => {
                // v3 is byte-compatible with v4 minus `sequence` (which
                // `#[serde(default)]` supplies as 0); just stamp the version.
                let mut registry = serde_json::from_str::<Registry>(s)?;
                registry.version = Self::CURRENT_VERSION;
                registry.sequence = 0;
                Ok(registry)
            }
            2 => Ok(serde_json::from_str::<v2::Registry>(s)?.into_current()),
            1 => Ok(serde_json::from_str::<v1::Registry>(s)?.into_current()),
            other => Err(Error::UnsupportedRegistryVersion {
                found: other,
                supported: Self::CURRENT_VERSION,
            }),
        }
    }

    /// Semantic validation applied after a successful JSON parse (spec §5,
    /// hardening batch B #9). A violation means the ledger is *corrupt* and
    /// every caller fails closed on it (only `doctor --repair` may move it
    /// aside) -- being parseable JSON is necessary but not sufficient to
    /// trust the ledger.
    ///
    /// Checks: the schema `version` is recognized; `range` is ordered; every
    /// block is ordered, carries no port 0, and does not overlap any other
    /// block across the whole ledger. Note that a block lying *outside*
    /// `range` is deliberately NOT an error: `range` is frozen at creation
    /// while allocation follows the live config, so widening the configured
    /// pool legitimately produces such blocks (a `doctor` advisory).
    pub fn validate(&self) -> Result<()> {
        if self.version != Self::CURRENT_VERSION {
            return Err(Error::UnsupportedRegistryVersion {
                found: self.version,
                supported: Self::CURRENT_VERSION,
            });
        }
        if self.range.0 > self.range.1 {
            return Err(Error::General(format!(
                "invalid registry: range is reversed ([{}, {}])",
                self.range.0, self.range.1
            )));
        }

        // Owner-grouped blocks: an entry's own `block` and `pending_block`
        // belong to the same owner and are allowed to overlap each other (a
        // grow-in-place move); every other pair must be disjoint.
        let mut grouped = self.owner_grouped_blocks();
        for &(_, (start, end)) in &grouped {
            if start > end {
                return Err(Error::General(format!(
                    "invalid registry: block {start}-{end} is reversed"
                )));
            }
            if start == 0 {
                return Err(Error::General(
                    "invalid registry: a block includes port 0".to_string(),
                ));
            }
        }
        // Sort by start, then sweep with a single running (owner, max-end)
        // accumulator: within one connected run of overlapping intervals,
        // any block whose start falls inside the accumulator's span must
        // share its owner, or the two genuinely overlap and conflict.
        // O(n log n) instead of the previous O(n^2) all-pairs comparison.
        grouped.sort_by_key(|&(_, (start, _))| start);
        let mut active: Option<(usize, (u16, u16))> = None;
        for &(owner, block) in &grouped {
            if let Some((active_owner, active_block)) = active {
                if block.0 <= active_block.1 {
                    if owner != active_owner {
                        return Err(Error::General(format!(
                            "invalid registry: blocks {}-{} and {}-{} overlap",
                            active_block.0, active_block.1, block.0, block.1
                        )));
                    }
                    if block.1 > active_block.1 {
                        active = Some((active_owner, block));
                    }
                    continue;
                }
            }
            active = Some((owner, block));
        }

        Ok(())
    }

    /// Every allocated block across all projects' worktrees (including any
    /// pending move targets) plus all reservations, in no particular order.
    pub fn all_blocks(&self) -> Vec<(u16, u16)> {
        self.owner_grouped_blocks()
            .into_iter()
            .map(|(_, block)| block)
            .collect()
    }

    /// [`Registry::all_blocks`], tagged with an owner id so callers can
    /// tell which blocks belong to the same worktree entry (its `block` +
    /// `pending_block` pair). Reservations each get their own owner.
    fn owner_grouped_blocks(&self) -> Vec<(usize, (u16, u16))> {
        let mut owner = 0usize;
        let mut blocks = Vec::new();
        for project in self.projects.values() {
            for worktree in project.worktrees.values() {
                blocks.push((owner, worktree.block));
                if let Some(pending) = worktree.pending_block {
                    blocks.push((owner, pending));
                }
                owner += 1;
            }
        }
        for reservation in &self.reservations {
            blocks.push((owner, reservation.block));
            owner += 1;
        }
        blocks
    }

    /// Looks up a project entry by its common-dir key.
    pub fn find_project(&self, common_dir: &str) -> Option<&ProjectEntry> {
        self.projects.get(common_dir)
    }

    /// Mutable variant of [`Registry::find_project`].
    pub fn find_project_mut(&mut self, common_dir: &str) -> Option<&mut ProjectEntry> {
        self.projects.get_mut(common_dir)
    }
}

/// Whether inclusive ranges `a` and `b` share at least one point.
pub fn overlaps(a: (u16, u16), b: (u16, u16)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

/// The v2 ledger schema, read only for migration to the current version.
/// It differs from v3 solely in that `WorktreeEntry` had no `generation` /
/// `pending_block`; migration adds `generation = 1` and no pending block.
mod v2 {
    use super::Reservation;
    use chrono::{DateTime, FixedOffset};
    use serde::Deserialize;
    use std::collections::BTreeMap;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Registry {
        // Present so `deny_unknown_fields` accepts the `version` key; the
        // value is already known from the version peek, so it is unread.
        #[allow(dead_code)]
        pub version: u32,
        pub range: (u16, u16),
        pub projects: BTreeMap<String, ProjectEntry>,
        pub reservations: Vec<Reservation>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct ProjectEntry {
        pub name: String,
        pub worktrees: BTreeMap<String, WorktreeEntry>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct WorktreeEntry {
        pub block: (u16, u16),
        pub branch: Option<String>,
        pub manifest_hash: Option<String>,
        pub pinned: bool,
        pub label: Option<String>,
        pub allocated_at: DateTime<FixedOffset>,
        pub last_seen_at: DateTime<FixedOffset>,
    }

    impl WorktreeEntry {
        pub fn into_current(self) -> super::WorktreeEntry {
            super::WorktreeEntry {
                block: self.block,
                generation: 1,
                pending_block: None,
                branch: self.branch,
                manifest_hash: self.manifest_hash,
                pinned: self.pinned,
                label: self.label,
                allocated_at: self.allocated_at,
                last_seen_at: self.last_seen_at,
            }
        }
    }

    impl Registry {
        /// Converts a parsed v2 ledger to the current (v3) schema, keeping
        /// every worktree block verbatim.
        pub fn into_current(self) -> super::Registry {
            super::Registry {
                version: super::Registry::CURRENT_VERSION,
                range: self.range,
                sequence: 0,
                projects: self
                    .projects
                    .into_iter()
                    .map(|(key, project)| {
                        (
                            key,
                            super::ProjectEntry {
                                name: project.name,
                                worktrees: project
                                    .worktrees
                                    .into_iter()
                                    .map(|(path, entry)| (path, entry.into_current()))
                                    .collect(),
                            },
                        )
                    })
                    .collect(),
                reservations: self.reservations,
            }
        }
    }
}

/// The v1 ledger schema (pre-hardening), read only for migration to the
/// current version. It differs from v2 solely in that `ProjectEntry`
/// carried a `subranges` field; `WorktreeEntry` and `Reservation` are
/// reused from v2.
mod v1 {
    use super::{v2, Reservation};
    use serde::Deserialize;
    use std::collections::BTreeMap;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Registry {
        // Present so `deny_unknown_fields` accepts the `version` key; the
        // value is already known (1) from the version peek, so it is unread.
        #[allow(dead_code)]
        pub version: u32,
        pub range: (u16, u16),
        pub projects: BTreeMap<String, ProjectEntry>,
        pub reservations: Vec<Reservation>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct ProjectEntry {
        pub name: String,
        #[allow(dead_code)]
        pub subranges: Vec<(u16, u16)>,
        pub worktrees: BTreeMap<String, v2::WorktreeEntry>,
    }

    impl Registry {
        /// Converts a parsed v1 ledger to the current schema, dropping
        /// `subranges` and keeping every worktree block verbatim.
        pub fn into_current(self) -> super::Registry {
            super::Registry {
                version: super::Registry::CURRENT_VERSION,
                range: self.range,
                sequence: 0,
                projects: self
                    .projects
                    .into_iter()
                    .map(|(key, project)| {
                        (
                            key,
                            super::ProjectEntry {
                                name: project.name,
                                worktrees: project
                                    .worktrees
                                    .into_iter()
                                    .map(|(path, entry)| (path, entry.into_current()))
                                    .collect(),
                            },
                        )
                    })
                    .collect(),
                reservations: self.reservations,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC_EXAMPLE: &str = r#"
    {
      "version": 4,
      "range": [3000, 9999],
      "sequence": 7,
      "projects": {
        "/home/user/dev/myapp/.git": {
          "name": "myapp",
          "worktrees": {
            "/home/user/dev/myapp": {
              "block": [3000, 3004],
              "generation": 1,
              "pending_block": null,
              "branch": "main",
              "manifest_hash": "a1b2c3d4e5f6",
              "pinned": false,
              "label": null,
              "allocated_at": "2026-07-15T10:00:00+09:00",
              "last_seen_at": "2026-07-15T12:00:00+09:00"
            }
          }
        }
      },
      "reservations": []
    }
    "#;

    /// A v3 ledger (pre-sequence) for migration coverage.
    const SPEC_EXAMPLE_V3: &str = r#"
    {
      "version": 3,
      "range": [3000, 9999],
      "projects": {
        "/home/user/dev/myapp/.git": {
          "name": "myapp",
          "worktrees": {
            "/home/user/dev/myapp": {
              "block": [3000, 3004],
              "generation": 2,
              "pending_block": null,
              "branch": "main",
              "manifest_hash": "a1b2c3d4e5f6",
              "pinned": false,
              "label": null,
              "allocated_at": "2026-07-15T10:00:00+09:00",
              "last_seen_at": "2026-07-15T12:00:00+09:00"
            }
          }
        }
      },
      "reservations": []
    }
    "#;

    /// A v2 ledger (pre-generation/pending) for migration coverage.
    const SPEC_EXAMPLE_V2: &str = r#"
    {
      "version": 2,
      "range": [3000, 9999],
      "projects": {
        "/home/user/dev/myapp/.git": {
          "name": "myapp",
          "worktrees": {
            "/home/user/dev/myapp": {
              "block": [3000, 3004],
              "branch": "main",
              "manifest_hash": "a1b2c3d4e5f6",
              "pinned": false,
              "label": null,
              "allocated_at": "2026-07-15T10:00:00+09:00",
              "last_seen_at": "2026-07-15T12:00:00+09:00"
            }
          }
        }
      },
      "reservations": []
    }
    "#;

    /// A v1 ledger (with the old `subranges` field) for migration coverage.
    const SPEC_EXAMPLE_V1: &str = r#"
    {
      "version": 1,
      "range": [3000, 9999],
      "projects": {
        "/home/user/dev/myapp/.git": {
          "name": "myapp",
          "subranges": [[3000, 3499]],
          "worktrees": {
            "/home/user/dev/myapp": {
              "block": [3000, 3004],
              "branch": "main",
              "manifest_hash": "a1b2c3d4e5f6",
              "pinned": false,
              "label": null,
              "allocated_at": "2026-07-15T10:00:00+09:00",
              "last_seen_at": "2026-07-15T12:00:00+09:00"
            }
          }
        }
      },
      "reservations": []
    }
    "#;

    #[test]
    fn empty_has_no_projects_or_reservations() {
        let reg = Registry::empty((3000, 9999));
        assert_eq!(reg.version, 4);
        assert_eq!(reg.range, (3000, 9999));
        assert!(reg.projects.is_empty());
        assert!(reg.reservations.is_empty());
        assert!(reg.all_blocks().is_empty());
    }

    #[test]
    fn overlaps_detects_shared_and_disjoint_ranges() {
        assert!(overlaps((3000, 3004), (3004, 3009)));
        assert!(overlaps((3000, 3499), (3000, 3499)));
        assert!(!overlaps((3000, 3004), (3005, 3009)));
        assert!(!overlaps((3005, 3009), (3000, 3004)));
    }

    #[test]
    fn spec_example_round_trips() {
        let reg = Registry::from_json(SPEC_EXAMPLE).unwrap();

        assert_eq!(reg.version, 4);
        assert_eq!(reg.sequence, 7);
        assert_eq!(reg.range, (3000, 9999));
        assert!(reg.reservations.is_empty());

        let project = reg.find_project("/home/user/dev/myapp/.git").unwrap();
        assert_eq!(project.name, "myapp");

        let worktree = project.worktrees.get("/home/user/dev/myapp").unwrap();
        assert_eq!(worktree.block, (3000, 3004));
        assert_eq!(worktree.branch.as_deref(), Some("main"));
        assert_eq!(worktree.manifest_hash.as_deref(), Some("a1b2c3d4e5f6"));
        assert!(!worktree.pinned);
        assert_eq!(worktree.label, None);

        // Round trip: serialize back to JSON and deserialize again; the
        // resulting struct must be identical to the original.
        let serialized = serde_json::to_string(&reg).unwrap();
        let reg_again: Registry = serde_json::from_str(&serialized).unwrap();
        assert_eq!(reg, reg_again);
    }

    #[test]
    fn from_json_migrates_v1_dropping_subranges_and_keeping_blocks() {
        let reg = Registry::from_json(SPEC_EXAMPLE_V1).unwrap();

        // Migrated to the current version...
        assert_eq!(reg.version, Registry::CURRENT_VERSION);
        // ...with every worktree block preserved verbatim (no ports move).
        let project = reg.find_project("/home/user/dev/myapp/.git").unwrap();
        let worktree = project.worktrees.get("/home/user/dev/myapp").unwrap();
        assert_eq!(worktree.block, (3000, 3004));
        assert_eq!(worktree.generation, 1);
        assert_eq!(worktree.pending_block, None);
        // The migrated ledger is valid and re-serializes without subranges.
        assert!(reg.validate().is_ok());
        let serialized = serde_json::to_string(&reg).unwrap();
        assert!(!serialized.contains("subranges"));
        assert!(serialized.contains("\"version\":4"));
    }

    #[test]
    fn from_json_migrates_v2_adding_generation_and_no_pending() {
        let reg = Registry::from_json(SPEC_EXAMPLE_V2).unwrap();

        assert_eq!(reg.version, Registry::CURRENT_VERSION);
        let project = reg.find_project("/home/user/dev/myapp/.git").unwrap();
        let worktree = project.worktrees.get("/home/user/dev/myapp").unwrap();
        assert_eq!(worktree.block, (3000, 3004));
        assert_eq!(worktree.generation, 1);
        assert_eq!(worktree.pending_block, None);
        assert!(reg.validate().is_ok());
    }

    #[test]
    fn from_json_migrates_v3_adding_sequence_zero() {
        let reg = Registry::from_json(SPEC_EXAMPLE_V3).unwrap();
        assert_eq!(reg.version, Registry::CURRENT_VERSION);
        assert_eq!(reg.sequence, 0);
        let project = reg.find_project("/home/user/dev/myapp/.git").unwrap();
        let worktree = project.worktrees.get("/home/user/dev/myapp").unwrap();
        // Every block and generation preserved verbatim on upgrade.
        assert_eq!(worktree.block, (3000, 3004));
        assert_eq!(worktree.generation, 2);
        assert!(reg.validate().is_ok());
        let serialized = serde_json::to_string(&reg).unwrap();
        assert!(serialized.contains("\"version\":4"));
        assert!(serialized.contains("\"sequence\":0"));
    }

    #[test]
    fn from_json_rejects_unknown_version() {
        let json = r#"{"version":99,"range":[3000,9999],"projects":{},"reservations":[]}"#;
        assert!(Registry::from_json(json).is_err());
    }

    #[test]
    fn tuple_fields_serialize_as_json_arrays() {
        let reg = Registry::empty((3000, 9999));
        let json = serde_json::to_value(&reg).unwrap();
        assert_eq!(json["range"], serde_json::json!([3000, 9999]));
    }

    #[test]
    fn all_blocks_includes_worktrees_and_reservations() {
        let mut reg: Registry = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        reg.reservations.push(Reservation {
            block: (5000, 5009),
            label: Some("postgres-dev".to_string()),
            pinned: true,
        });

        let mut blocks = reg.all_blocks();
        blocks.sort();
        assert_eq!(blocks, vec![(3000, 3004), (5000, 5009)]);
    }

    #[test]
    fn find_project_missing_key_returns_none() {
        let reg = Registry::empty((3000, 9999));
        assert!(reg.find_project("/no/such/project/.git").is_none());
    }

    #[test]
    fn validate_accepts_a_well_formed_ledger() {
        let reg: Registry = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        assert!(reg.validate().is_ok());
        assert!(Registry::empty((3000, 9999)).validate().is_ok());
    }

    #[test]
    fn validate_rejects_unknown_version() {
        let mut reg = Registry::empty((3000, 9999));
        reg.version = 999;
        assert!(reg.validate().is_err());
    }

    #[test]
    fn validate_rejects_reversed_range() {
        let mut reg = Registry::empty((9999, 3000));
        reg.version = Registry::CURRENT_VERSION;
        assert!(reg.validate().is_err());
    }

    #[test]
    fn validate_rejects_port_zero() {
        let mut reg = Registry::empty((3000, 9999));
        reg.reservations.push(Reservation {
            block: (0, 4),
            label: None,
            pinned: false,
        });
        assert!(reg.validate().is_err());
    }

    #[test]
    fn validate_rejects_overlapping_blocks() {
        let mut reg = Registry::empty((3000, 9999));
        reg.reservations.push(Reservation {
            block: (3000, 3004),
            label: None,
            pinned: false,
        });
        reg.reservations.push(Reservation {
            block: (3003, 3007),
            label: None,
            pinned: false,
        });
        assert!(reg.validate().is_err());
    }

    #[test]
    fn pending_block_is_occupied_and_may_overlap_only_its_own_block() {
        let mut reg: Registry = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        let entry = reg
            .find_project_mut("/home/user/dev/myapp/.git")
            .unwrap()
            .worktrees
            .get_mut("/home/user/dev/myapp")
            .unwrap();

        // A grow-in-place move: pending (3000,3009) overlaps the entry's
        // own block (3000,3004) -- valid, and both count as occupied.
        entry.pending_block = Some((3000, 3009));
        assert!(reg.validate().is_ok());
        let mut blocks = reg.all_blocks();
        blocks.sort();
        assert_eq!(blocks, vec![(3000, 3004), (3000, 3009)]);

        // But a pending block overlapping a *different* owner is corrupt.
        reg.reservations.push(Reservation {
            block: (3008, 3012),
            label: None,
            pinned: true,
        });
        assert!(reg.validate().is_err());
    }

    #[test]
    fn overlap_validation_detects_cross_owner_overlap_after_sort() {
        let mut reg = Registry::empty((3000, 9999));
        // Pushed out of start order on purpose: owner0's block starts after
        // owner1's and owner2's, so the sweep only finds the true overlap
        // (owner2 vs owner0) if it sorts by start first -- comparing in raw
        // push order would either miss it or misfire on the disjoint
        // owner0/owner1 pair.
        reg.reservations.push(Reservation {
            block: (3010, 3020), // owner0: overlaps owner2, disjoint from owner1
            label: None,
            pinned: false,
        });
        reg.reservations.push(Reservation {
            block: (3000, 3005), // owner1: disjoint from both other blocks
            label: None,
            pinned: false,
        });
        reg.reservations.push(Reservation {
            block: (3008, 3012), // owner2: overlaps owner0, disjoint from owner1
            label: None,
            pinned: false,
        });
        assert!(reg.validate().is_err());

        // Drop the overlapping block: same three-owner shape, no overlap now.
        reg.reservations.pop();
        assert!(reg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_invalid_pending_block() {
        let mut reg: Registry = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        let entry = reg
            .find_project_mut("/home/user/dev/myapp/.git")
            .unwrap()
            .worktrees
            .get_mut("/home/user/dev/myapp")
            .unwrap();
        entry.pending_block = Some((4000, 3999));
        assert!(reg.validate().is_err());
    }

    #[test]
    fn validate_allows_a_block_outside_the_recorded_range() {
        // range is informational only; a block above it (e.g. after the
        // config pool was widened) must NOT be treated as corruption.
        let mut reg = Registry::empty((3000, 3999));
        reg.reservations.push(Reservation {
            block: (5000, 5004),
            label: None,
            pinned: false,
        });
        assert!(reg.validate().is_ok());
    }

    #[test]
    fn deny_unknown_fields_rejects_extra_keys() {
        let json =
            r#"{"version":1,"range":[3000,9999],"projects":{},"reservations":[],"bogus":true}"#;
        assert!(serde_json::from_str::<Registry>(json).is_err());
    }
}
