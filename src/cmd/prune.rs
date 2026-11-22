//! `portool prune` (spec §8.2, §8.3, frozen decision 13): explicit GC,
//! either for the current project or (`--all`) across the whole ledger.

use crate::error::Result;
use crate::gc;
use crate::gitctx::GitCtx;
use crate::lock;
use crate::paths;
use crate::ports;
use crate::reclaim::{self, Event};
use crate::registry::{ProjectEntry, Registry};
use crate::store;
use std::path::{Path, PathBuf};
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `portool prune`. `--dry-run` never acquires the lock or writes the
/// ledger; it loads its own snapshot, reports what it would reclaim, and
/// discards it. Both modes are fail-closed on a corrupt, unsupported, or
/// unreadable ledger: reclamation must never run against a placeholder
/// empty registry.
pub fn run(all: bool, dry_run: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;

    if dry_run {
        // Read-only, lock-free.
        let mut registry = match store::load_strict(&paths::registry_path()?)? {
            Some(registry) => registry,
            None => return Ok(()), // no ledger -> nothing to prune
        };
        if all {
            prune_all(&mut registry, true);
        } else {
            let ctx = GitCtx::discover(&cwd)?;
            prune_current(&mut registry, &ctx, true)?;
        }
        return Ok(());
    }

    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;
    let mut registry = match store::load_strict(&paths::registry_path()?)? {
        Some(registry) => registry,
        None => return Ok(()),
    };

    let changed = if all {
        prune_all(&mut registry, false)
    } else {
        let ctx = GitCtx::discover(&cwd)?;
        prune_current(&mut registry, &ctx, false)?
    };

    if changed {
        store::save(&paths::registry_path()?, &mut registry)?;
    }
    Ok(())
}

fn prune_current(registry: &mut Registry, ctx: &GitCtx, dry_run: bool) -> Result<bool> {
    let key = ctx.common_dir.to_string_lossy().into_owned();
    let live = ctx.worktree_list()?;
    match registry.projects.get_mut(&key) {
        Some(project) => Ok(reclaim_project_worktrees(project, &live, dry_run)),
        None => Ok(false),
    }
}

/// Spec §8.2 `--all`: the same reclamation the automatic sweep runs
/// ([`reclaim::reclaim_all`]), with a report line per removal. Pinned entries
/// are exempt, a Git failure keeps the project's entries, and a repository or
/// worktree on an unreachable (unmounted) volume is never treated as gone.
fn prune_all(registry: &mut Registry, dry_run: bool) -> bool {
    let verb = if dry_run { "would prune" } else { "pruned" };
    let mut changed = false;
    for event in reclaim::reclaim_all(registry) {
        match event {
            Event::Worktree { path, block } => {
                println!(
                    "{verb} {} (block {}-{})",
                    crate::display::text(&path),
                    block.0,
                    block.1
                );
                changed = true;
            }
            Event::Project { name, key } => {
                println!(
                    "{verb} project {} ({})",
                    crate::display::text(&name),
                    crate::display::text(&key)
                );
                changed = true;
            }
            Event::Skipped { key, reason } => eprintln!(
                "portool: prune: skipping project {}: listing worktrees failed \
                 ({reason}); its entries were kept",
                crate::display::text(&key)
            ),
        }
    }
    changed
}

fn reclaim_project_worktrees(project: &mut ProjectEntry, live: &[PathBuf], dry_run: bool) -> bool {
    let dir_exists = |p: &Path| !reclaim::path_gone(&reclaim::RealFs, p);
    let block_unused = |b: (u16, u16)| ports::block_free(b);
    let reclaimed = gc::collect(project, live, &dir_exists, &block_unused);

    let verb = if dry_run { "would prune" } else { "pruned" };
    for (path, block) in &reclaimed {
        println!(
            "{verb} {} (block {}-{})",
            crate::display::text(path),
            block.0,
            block.1
        );
    }
    !reclaimed.is_empty()
}
