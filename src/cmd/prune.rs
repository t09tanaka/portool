//! `portool prune` (spec §8.2, §8.3, frozen decision 13): explicit GC,
//! either for the current project or (`--all`) across the whole ledger.

use crate::error::Result;
use crate::gc;
use crate::gitctx::{self, GitCtx};
use crate::lock;
use crate::paths;
use crate::ports;
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

/// Spec §8.2 `--all`: for every project, if its `common_dir` still exists,
/// apply the same per-worktree reclamation as the default mode; if the
/// `common_dir` itself is gone (the whole repository was deleted), reclaim
/// each unpinned worktree entry whose ports are confirmed unused, and drop
/// the project entry itself once no worktrees remain. Pinned entries are
/// exempt, same as every other GC path.
fn prune_all(registry: &mut Registry, dry_run: bool) -> bool {
    let mut changed = false;
    let keys: Vec<String> = registry.projects.keys().cloned().collect();
    let mut projects_to_remove = Vec::new();

    for key in &keys {
        if !Path::new(key).exists() {
            let project = registry
                .projects
                .get_mut(key)
                .expect("key came from registry.projects.keys()");
            // The repository is gone, but the pin contract still holds: pinned
            // entries are exempt from every GC path, so reclaim only unpinned
            // entries whose ports (block + pending) are confirmed unused, and
            // drop the project entry only once nothing remains.
            let reclaimable: Vec<String> = project
                .worktrees
                .iter()
                .filter(|(_, w)| {
                    !w.pinned
                        && ports::block_free(w.block)
                        && w.pending_block.map(ports::block_free).unwrap_or(true)
                })
                .map(|(path, _)| path.clone())
                .collect();
            let verb = if dry_run { "would prune" } else { "pruned" };
            for path in &reclaimable {
                let block = project.worktrees[path].block;
                println!(
                    "{verb} {} (block {}-{})",
                    crate::display::text(path),
                    block.0,
                    block.1
                );
                project.worktrees.remove(path);
                changed = true;
            }
            if project.worktrees.is_empty() {
                println!(
                    "{verb} project {} ({})",
                    crate::display::text(&project.name),
                    crate::display::text(key)
                );
                projects_to_remove.push(key.clone());
                changed = true;
            }
            continue;
        }

        let live = match gitctx::worktree_list_at(Path::new(key)) {
            Ok(live) => live,
            Err(err) => {
                // Fail-closed (external review P0-2): a Git failure is not
                // "this project has zero worktrees". Reclaiming on that
                // misreading could free blocks that live worktrees' env
                // files still hand out. Keep every entry and move on.
                eprintln!(
                    "portool: prune: skipping project {}: listing worktrees failed \
                     ({err}); its entries were kept",
                    crate::display::text(key)
                );
                continue;
            }
        };
        let project = registry
            .projects
            .get_mut(key)
            .expect("key came from registry.projects.keys()");
        if reclaim_project_worktrees(project, &live, dry_run) {
            changed = true;
        }
    }

    for key in projects_to_remove {
        registry.projects.remove(&key);
    }

    changed
}

fn reclaim_project_worktrees(project: &mut ProjectEntry, live: &[PathBuf], dry_run: bool) -> bool {
    let dir_exists = |p: &Path| p.exists();
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
