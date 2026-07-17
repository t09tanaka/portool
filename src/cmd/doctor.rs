//! `portool doctor` (hardening batch D #5, C4; restore-first repair added in
//! v0.7.0, external review P0-1): diagnose and repair the current project.
//!
//! - **Repair** (`--repair`): the one and only place a bad ledger is moved
//!   aside to `registry.json.corrupt-<ts>`. Every other command fails
//!   closed on such a ledger and points here. Two distinct cases:
//!   - *Corrupt*: restore-first. A valid `registry.json.bak` is restored in
//!     place of the corrupt file, so every other project's allocations
//!     survive -- restoring costs at most the one save since the last
//!     backup refresh. Without a valid backup, plain `--repair` refuses and
//!     points at `--abandon-other-projects`.
//!   - *Unsupported version* (written by a newer portool): never
//!     auto-restored from backup (that would silently roll back a newer
//!     binary's ledger). `--repair` alone always errors toward upgrading.
//!   - `--abandon-other-projects`: by explicit request only, either case
//!     falls back to the old destructive move-aside-and-start-empty.
//! - **Rebuild**: re-imports blocks recorded in live worktrees'
//!   `.env.portool` that the ledger has lost (e.g. after `--repair`
//!   abandoned the old ledger, or for any project not yet reconciled after
//!   a restore). Import is validity- and overlap-guarded, so a nonsense
//!   block baked into an env file is reported and skipped rather than
//!   written into the ledger.
//! - **Report**: flags this project's blocks whose ports are currently in
//!   use (on `127.0.0.1`).
//!
//! Rebuild is per-project: it only touches the project `doctor` runs in;
//! other projects stay dropped until `doctor` runs in each (unless a
//! backup restore already brought them back). The moved-aside
//! `registry.json.corrupt-<ts>` file is the authoritative artifact for
//! reconciling anything `doctor` didn't already restore or rebuild.

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
use std::path::Path;
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `portool doctor` for the current project. Without `repair`, a bad
/// ledger is a hard error; with it, a corrupt ledger is restored from its
/// backup (all projects kept) unless `abandon_other_projects` forces the
/// destructive move-aside-and-start-empty path; an unsupported-version
/// ledger is only ever discarded via `abandon_other_projects`, never
/// auto-restored from backup. Either way this project's entries are then
/// rebuilt from live worktrees' `.env.portool`.
pub fn run(repair: bool, abandon_other_projects: bool) -> Result<()> {
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
        store::LedgerLoad::UnsupportedVersion { found, supported } => {
            if !repair || !abandon_other_projects {
                return Err(Error::General(format!(
                    "{} uses registry schema version {found}, but this build understands \
                     version {supported}; upgrade portool instead. (If you really want to \
                     discard that ledger, re-run with 'portool doctor --repair \
                     --abandon-other-projects'.)",
                    registry_path.display()
                )));
            }
            let moved_to = store::move_aside(&registry_path)?;
            eprintln!(
                "portool: doctor: abandoned the version-{found} ledger (moved aside to {})",
                moved_to.display()
            );
            Registry::empty(config.range)
        }
        store::LedgerLoad::Corrupt { reason } => {
            if !repair {
                return Err(Error::General(format!(
                    "{} is corrupt ({reason}); re-run with 'portool doctor --repair' to \
                     restore it from backup and rebuild this project's entries",
                    registry_path.display()
                )));
            }
            repair_corrupt(&registry_path, &reason, abandon_other_projects, &config)?
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

/// The --repair path for a corrupt ledger. Restore-first: a valid
/// `registry.json.bak` brings back *every* project (external review P0-1);
/// only the explicit --abandon-other-projects flag falls back to the
/// destructive start-from-empty rebuild.
fn repair_corrupt(
    registry_path: &Path,
    reason: &str,
    abandon_other_projects: bool,
    config: &Config,
) -> Result<Registry> {
    if abandon_other_projects {
        let moved_to = store::move_aside(registry_path)?;
        eprintln!(
            "portool: doctor: {} is corrupt ({reason}); moved aside to {} and starting \
             empty (--abandon-other-projects)",
            registry_path.display(),
            moved_to.display()
        );
        return Ok(Registry::empty(config.range));
    }

    match store::load(&store::backup_path(registry_path)) {
        store::LedgerLoad::Loaded(backup) => {
            let moved_to = store::move_aside(registry_path)?;
            store::save(registry_path, &backup)?;
            eprintln!(
                "portool: doctor: {} was corrupt ({reason}); moved aside to {} and \
                 restored the last good backup (all projects kept)",
                registry_path.display(),
                moved_to.display()
            );
            Ok(backup)
        }
        _ => Err(Error::General(format!(
            "{} is corrupt ({reason}) and no valid backup exists; a plain repair would \
             abandon every other project's allocations. Re-run with 'portool doctor \
             --repair --abandon-other-projects' if you accept that",
            registry_path.display()
        ))),
    }
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
