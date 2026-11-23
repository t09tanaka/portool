//! `portool prune` (spec §8.2, §8.3, frozen decision 13): explicit GC,
//! either for the current project or (`--all`) across the whole ledger.
//!
//! The rules are the automatic sweep's ([`crate::reclaim`]); this command
//! adds a report, and `--forget-unverifiable`, the one way to drop an entry
//! whose worktree can never be verified as gone (a volume that will not be
//! mounted again, a repository git can no longer read).

use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::lock;
use crate::paths;
use crate::reclaim::{self, Event, Scope};
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

    // Files and git are examined before the lock is taken, for this
    // project as for the others: a dead network mount must not stall every
    // other portool command.
    let Some(snapshot) = store::load_strict(&registry_path)? else {
        return Ok(()); // no ledger -> nothing to prune
    };
    let current = match all {
        true => None,
        false => Some(
            GitCtx::discover(&cwd)?
                .common_dir
                .to_string_lossy()
                .into_owned(),
        ),
    };
    let scope = match &current {
        Some(key) => Scope::Only(key),
        None => Scope::All,
    };
    let plan = reclaim::plan_manual(&snapshot, scope, forget_unverifiable);
    if current.is_some() {
        // In its own project a git that cannot list the worktrees is the
        // command's error, `--forget-unverifiable` or not.
        if let Some((_, reason)) = plan.git_failed().first() {
            return Err(Error::General(reason.clone()));
        }
    }

    let _lock = if dry_run {
        None // read-only, lock-free
    } else {
        Some(lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?)
    };
    let mut registry = match store::load_strict(&registry_path)? {
        Some(registry) => registry,
        None => return Ok(()),
    };

    for (key, reason) in plan.git_failed() {
        eprintln!(
            "portool: prune: skipping project {}: listing worktrees failed \
             ({reason}); its entries were kept",
            crate::display::text(key)
        );
    }
    let events = plan.apply_reporting(&mut registry);

    if report(&events, dry_run, forget_unverifiable) && !dry_run {
        store::save(&registry_path, &mut registry)?;
    }
    // What could not be checked at all is not "nothing to prune": say what,
    // and fail, after what could be decided has been done.
    let unchecked = plan.unchecked();
    if !unchecked.is_empty() {
        for line in unchecked {
            eprintln!("portool: prune: could not check {line}; its entries were kept");
        }
        return Err(Error::General(format!(
            "prune is incomplete: {} check(s) did not finish (limit: {}s)",
            unchecked.len(),
            reclaim::MANUAL_BUDGET.as_secs()
        )));
    }
    Ok(())
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
