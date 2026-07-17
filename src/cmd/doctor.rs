//! `portool doctor` (hardening batch D #5, C4): diagnose and repair the
//! current project.
//!
//! - **Repair** (`--repair`): the one and only place a corrupt (or
//!   explicitly-abandoned unsupported-version) ledger is moved aside to
//!   `registry.json.corrupt-<ts>` and rebuilt from scratch. Every other
//!   command fails closed on such a ledger and points here.
//! - **Rebuild**: re-imports blocks recorded in live worktrees'
//!   `.env.portool` that the ledger has lost (e.g. after `--repair` moved
//!   the old ledger aside). Import is validity- and overlap-guarded, so a
//!   nonsense block baked into an env file is reported and skipped rather
//!   than written into the ledger.
//! - **Report**: flags this project's blocks whose ports are currently in
//!   use (on `127.0.0.1`).
//!
//! Rebuild is per-project: it only touches the project `doctor` runs in;
//! other projects stay dropped until `doctor` runs in each. The moved-aside
//! `registry.json.corrupt-<ts>` file is the authoritative artifact for
//! reconciling projects `doctor` didn't rebuild.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::lock;
use crate::paths;
use crate::ports;
use crate::registry::{overlaps, ProjectEntry, Registry, WorktreeEntry};
use crate::store;
use chrono::{DateTime, FixedOffset, Local, SubsecRound};
use std::collections::BTreeMap;
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `portool doctor` for the current project. Without `repair`, a bad
/// ledger is a hard error; with it, the bad file is moved aside and this
/// project's entries are rebuilt from live worktrees' `.env.portool`.
pub fn run(repair: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;
    let common_dir_key = ctx.common_dir.to_string_lossy().into_owned();

    let config = Config::load()?;
    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;
    let registry_path = paths::registry_path()?;
    let mut registry = match store::load(&registry_path) {
        store::LedgerLoad::Loaded(registry) => registry,
        store::LedgerLoad::Missing => Registry::empty(config.range),
        store::LedgerLoad::ReadError { reason } => {
            // Not repairable from here: the file may be perfectly intact and
            // merely unreadable (permissions, EIO); moving it aside blind
            // could destroy a healthy ledger.
            return Err(Error::General(format!(
                "failed to read {} ({reason}); fix the underlying I/O problem first",
                registry_path.display()
            )));
        }
        bad
        @ (store::LedgerLoad::Corrupt { .. } | store::LedgerLoad::UnsupportedVersion { .. }) => {
            let what = match &bad {
                store::LedgerLoad::Corrupt { reason } => format!("is corrupt ({reason})"),
                store::LedgerLoad::UnsupportedVersion { found, supported } => format!(
                    "uses registry schema version {found}, which this build does not \
                     understand (it understands version {supported}) -- prefer upgrading \
                     portool over --repair, which abandons that ledger"
                ),
                _ => unreachable!(),
            };
            if !repair {
                return Err(Error::General(format!(
                    "{} {what}; re-run with 'portool doctor --repair' to move it aside and \
                     rebuild this project's entries",
                    registry_path.display()
                )));
            }
            let moved_to = store::move_aside(&registry_path)?;
            eprintln!(
                "portool: doctor: {} {what}; moved aside to {}",
                registry_path.display(),
                moved_to.display()
            );
            Registry::empty(config.range)
        }
    };

    // 1. Rebuild lost entries from live worktrees' env files.
    let live = ctx.worktree_list()?;
    let mut occupied = registry.all_blocks();
    let now = now_local();
    let mut imported = 0usize;

    for wt in &live {
        let wt_key = wt.to_string_lossy().into_owned();
        let already = registry
            .find_project(&common_dir_key)
            .map(|p| p.worktrees.contains_key(&wt_key))
            .unwrap_or(false);
        if already {
            continue;
        }
        let block = match crate::envfile::read_block_from_env(wt) {
            Some(block) => block,
            None => continue,
        };
        if let Err(reason) = validate_block(block) {
            eprintln!(
                "portool: doctor: {} records block {}-{}, which {reason}; skipping \
                 re-import (fix its .env.portool or run reallocate)",
                wt.display(),
                block.0,
                block.1
            );
            continue;
        }
        if occupied.iter().any(|&b| overlaps(b, block)) {
            eprintln!(
                "portool: doctor: {} records block {}-{}, which overlaps an existing \
                 allocation; skipping re-import (fix its .env.portool or run reallocate)",
                wt.display(),
                block.0,
                block.1
            );
            continue;
        }

        let project = registry
            .projects
            .entry(common_dir_key.clone())
            .or_insert_with(|| ProjectEntry {
                name: ctx.project_name.clone(),
                worktrees: BTreeMap::new(),
            });
        project.worktrees.insert(
            wt_key,
            WorktreeEntry {
                block,
                generation: 1,
                pending_block: None,
                branch: None,
                manifest_hash: None,
                pinned: false,
                label: None,
                allocated_at: now,
                last_seen_at: now,
            },
        );
        occupied.push(block);
        imported += 1;
        println!(
            "portool: doctor: re-imported {}-{} for {}",
            block.0,
            block.1,
            wt.display()
        );
    }

    if imported > 0 {
        // Never persist a ledger the next command would reject as corrupt:
        // the per-block guards above should make this unreachable, but a
        // validation failure here must abort the save, not ship.
        registry.validate().map_err(|err| {
            Error::General(format!(
                "doctor produced an invalid ledger ({err}); nothing was saved"
            ))
        })?;
        store::save(&registry_path, &registry)?;
    }

    // 2. Report this project's blocks whose ports are currently in use.
    let mut in_use = 0usize;
    if let Some(project) = registry.find_project(&common_dir_key) {
        for (path, entry) in &project.worktrees {
            if !ports::block_free(entry.block) {
                in_use += 1;
                println!(
                    "portool: doctor: block {}-{} ({path}) has ports in use \
                     -- may be this worktree's own processes",
                    entry.block.0, entry.block.1
                );
            }
        }
    }

    if imported == 0 && in_use == 0 {
        println!("portool: doctor: nothing to repair for this project");
    }
    Ok(())
}

/// The same per-block invariants [`Registry::validate`] enforces, applied
/// to a single candidate before it is imported: ordered, and no port 0.
fn validate_block(block: (u16, u16)) -> std::result::Result<(), &'static str> {
    if block.0 > block.1 {
        return Err("is reversed");
    }
    if block.0 == 0 {
        return Err("includes port 0");
    }
    Ok(())
}

fn now_local() -> DateTime<FixedOffset> {
    Local::now().trunc_subsecs(0).fixed_offset()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_block_rejects_zero_and_reversed() {
        assert!(validate_block((0, 0)).is_err());
        assert!(validate_block((0, 4)).is_err());
        assert!(validate_block((4000, 3999)).is_err());
        assert!(validate_block((3000, 3004)).is_ok());
        assert!(validate_block((3000, 3000)).is_ok());
    }
}
