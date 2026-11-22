//! `portool prune` (spec §8.2, §8.3, frozen decision 13): explicit GC,
//! either for the current project or (`--all`) across the whole ledger.
//!
//! The rules are the automatic sweep's ([`crate::reclaim`]); this command
//! adds a report, and `--forget-unverifiable`, the one way to drop an entry
//! whose worktree can never be verified as gone (a volume that will not be
//! mounted again, a repository git can no longer read).

use crate::error::{Error, Result};
use crate::gitctx::{self, GitCtx};
use crate::lock;
use crate::paths;
use crate::reclaim::{self, Event};
use crate::registry::Registry;
use crate::store;
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `portool prune`. `--dry-run` never acquires the lock or writes the
/// ledger; it loads its own snapshot, reports what it would reclaim, and
/// discards it. Both modes are fail-closed on a corrupt, unsupported, or
/// unreadable ledger: reclamation must never run against a placeholder
/// empty registry.
pub fn run(all: bool, dry_run: bool, forget_unverifiable: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let registry_path = paths::registry_path()?;

    // Other projects' files and git are examined before the lock is taken:
    // a dead network mount must not stall every other portool command.
    let Some(snapshot) = store::load_strict(&registry_path)? else {
        return Ok(()); // no ledger -> nothing to prune
    };
    let plan = all.then(|| reclaim::plan_manual(&snapshot, forget_unverifiable));
    let ctx = match plan {
        Some(_) => None,
        None => Some(GitCtx::discover(&cwd)?),
    };

    let _lock = if dry_run {
        None // read-only, lock-free
    } else {
        Some(lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?)
    };
    let mut registry = match store::load_strict(&registry_path)? {
        Some(registry) => registry,
        None => return Ok(()),
    };

    let events = match (&plan, &ctx) {
        (Some(plan), _) => {
            for (key, reason) in plan.git_failed() {
                eprintln!(
                    "portool: prune: skipping project {}: listing worktrees failed \
                     ({reason}); its entries were kept",
                    crate::display::text(key)
                );
            }
            plan.apply_reporting(&mut registry)
        }
        (None, Some(ctx)) => prune_current(&mut registry, ctx, forget_unverifiable)?,
        (None, None) => unreachable!("either a plan or a git context exists"),
    };

    if report(&events, dry_run, forget_unverifiable) && !dry_run {
        store::save(&registry_path, &mut registry)?;
    }
    Ok(())
}

fn prune_current(registry: &mut Registry, ctx: &GitCtx, force: bool) -> Result<Vec<Event>> {
    let key = ctx.common_dir.to_string_lossy().into_owned();
    reclaim::sweep_project(
        registry,
        &key,
        &|| gitctx::worktree_listing_at(&ctx.worktree_root).map_err(|e| e.to_string()),
        force,
    )
    .map_err(Error::General)
}

/// Prints one line per event; returns whether the ledger changed.
fn report(events: &[Event], dry_run: bool, forced: bool) -> bool {
    let verb = if dry_run { "would prune" } else { "pruned" };
    let mut changed = false;
    for event in events {
        match event {
            Event::Released {
                path,
                block,
                forced,
            } => {
                changed = true;
                let note = match forced {
                    Some(why) => {
                        format!(" -- unverifiable, forgotten on request: {}", why.describe())
                    }
                    None => String::new(),
                };
                println!(
                    "{verb} {} (block {}-{}){note}",
                    crate::display::text(path),
                    block.0,
                    block.1
                );
            }
            Event::Project { name, key } => {
                changed = true;
                println!(
                    "{verb} project {} ({})",
                    crate::display::text(name),
                    crate::display::text(key)
                );
            }
            Event::Kept { path, block, why } => {
                let hint = if why.overridable() && !forced {
                    "; 'portool prune --forget-unverifiable' drops it if it is never coming back"
                } else {
                    ""
                };
                eprintln!(
                    "portool: prune: kept {} (block {}-{}), whose directory is missing: {}{hint}",
                    crate::display::text(path),
                    block.0,
                    block.1,
                    why.describe()
                );
            }
        }
    }
    changed
}
