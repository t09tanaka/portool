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
    /// Keyed by `realpath(git rev-parse --git-common-dir)`.
    pub projects: BTreeMap<String, ProjectEntry>,
    pub reservations: Vec<Reservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectEntry {
    /// Display name, inferred from the common-dir's parent directory name.
    pub name: String,
    /// Subranges owned by this project, in acquisition order.
    pub subranges: Vec<(u16, u16)>,
    /// Keyed by `realpath(worktree root)`.
    pub worktrees: BTreeMap<String, WorktreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeEntry {
    pub block: (u16, u16),
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
    /// The schema version this build reads and writes.
    pub const CURRENT_VERSION: u32 = 1;

    /// A freshly created, empty ledger recording `range` for informational
    /// purposes.
    pub fn empty(range: (u16, u16)) -> Registry {
        Registry {
            version: Self::CURRENT_VERSION,
            range,
            projects: BTreeMap::new(),
            reservations: Vec::new(),
        }
    }

    /// Semantic validation applied after a successful JSON parse (spec §5,
    /// hardening batch B #9). A violation means the ledger is *corrupt* and
    /// callers handle it as such (move aside under lock; non-zero for
    /// read-only callers) -- being parseable JSON is necessary but not
    /// sufficient to trust the ledger.
    ///
    /// Checks: the schema `version` is recognized; `range` is ordered; every
    /// block is ordered, carries no port 0, and does not overlap any other
    /// block across the whole ledger. Note that a block lying *outside*
    /// `range` is deliberately NOT an error: `range` is frozen at creation
    /// while allocation follows the live config, so widening the configured
    /// pool legitimately produces such blocks (a `doctor` advisory).
    pub fn validate(&self) -> Result<()> {
        if self.version != Self::CURRENT_VERSION {
            return Err(Error::General(format!(
                "unsupported registry version {} (this build understands version {})",
                self.version,
                Self::CURRENT_VERSION
            )));
        }
        if self.range.0 > self.range.1 {
            return Err(Error::General(format!(
                "invalid registry: range is reversed ([{}, {}])",
                self.range.0, self.range.1
            )));
        }

        let mut blocks = self.all_blocks();
        for &(start, end) in &blocks {
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

        blocks.sort_unstable();
        for pair in blocks.windows(2) {
            if overlaps(pair[0], pair[1]) {
                return Err(Error::General(format!(
                    "invalid registry: blocks {}-{} and {}-{} overlap",
                    pair[0].0, pair[0].1, pair[1].0, pair[1].1
                )));
            }
        }

        Ok(())
    }

    /// All subranges owned by any project, in no particular order.
    pub fn all_subranges(&self) -> Vec<(u16, u16)> {
        self.projects
            .values()
            .flat_map(|p| p.subranges.iter().copied())
            .collect()
    }

    /// Every allocated block across all projects' worktrees plus all
    /// reservations, in no particular order.
    pub fn all_blocks(&self) -> Vec<(u16, u16)> {
        let mut blocks: Vec<(u16, u16)> = self
            .projects
            .values()
            .flat_map(|p| p.worktrees.values().map(|w| w.block))
            .collect();
        blocks.extend(self.reservations.iter().map(|r| r.block));
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

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC_EXAMPLE: &str = r#"
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
        assert_eq!(reg.version, 1);
        assert_eq!(reg.range, (3000, 9999));
        assert!(reg.projects.is_empty());
        assert!(reg.reservations.is_empty());
        assert!(reg.all_subranges().is_empty());
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
        let reg: Registry = serde_json::from_str(SPEC_EXAMPLE).unwrap();

        assert_eq!(reg.version, 1);
        assert_eq!(reg.range, (3000, 9999));
        assert!(reg.reservations.is_empty());

        let project = reg.find_project("/home/user/dev/myapp/.git").unwrap();
        assert_eq!(project.name, "myapp");
        assert_eq!(project.subranges, vec![(3000, 3499)]);

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
