//! `portool prune` (spec §8.2, §8.3, frozen decision 13): explicit GC,
//! either for the current project or (`--all`) across the whole ledger.

use crate::error::{Error, Result};
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
/// discards it.
pub fn run(all: bool, dry_run: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;

    if dry_run {
        // Read-only, lock-free: never heal a corrupt ledger here.
        let mut registry = load_registry_or_abort(false)?;
        if all {
            prune_all(&mut registry, true);
        } else {
            let ctx = GitCtx::discover(&cwd)?;
            prune_current(&mut registry, &ctx, true)?;
        }
        return Ok(());
    }

    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;
    // Under the flock: safe to heal (rename aside) a corrupt ledger.
    let mut registry = load_registry_or_abort(true)?;

    let changed = if all {
        prune_all(&mut registry, false)
    } else {
        let ctx = GitCtx::discover(&cwd)?;
        prune_current(&mut registry, &ctx, false)?
    };

    if changed {
        store::save(&paths::registry_path()?, &registry)?;
    }
    Ok(())
}

/// Cross-task decision: a non-`NotFound` read failure must abort rather
/// than proceed to (dry-run or real) reclamation against a placeholder
/// empty registry, which for the real run would then overwrite a ledger
/// that may still be intact on disk.
fn load_registry_or_abort(heal: bool) -> Result<Registry> {
    let load_result = store::load(&paths::registry_path()?, heal);
    if load_result.read_error {
        return Err(Error::General(
            "failed to read the ledger; aborting without writing to avoid clobbering an intact ledger".to_string(),
        ));
    }
    Ok(load_result.registry)
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
/// `common_dir` itself is gone (the whole repository was deleted), the
/// entire project entry is reclaimed once every port across all its
/// worktrees is confirmed unused.
fn prune_all(registry: &mut Registry, dry_run: bool) -> bool {
    let mut changed = false;
    let keys: Vec<String> = registry.projects.keys().cloned().collect();
    let mut projects_to_remove = Vec::new();

    for key in &keys {
        if !Path::new(key).exists() {
            let project = registry
                .projects
                .get(key)
                .expect("key came from registry.projects.keys()");
            let all_ports_free = project
                .worktrees
                .values()
                .all(|w| ports::block_free(w.block));
            if all_ports_free {
                let verb = if dry_run { "would prune" } else { "pruned" };
                println!("{verb} project {} ({key})", project.name);
                projects_to_remove.push(key.clone());
                changed = true;
            }
            continue;
        }

        let live = gitctx::worktree_list_at(Path::new(key)).unwrap_or_default();
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
        println!("{verb} {path} (block {}-{})", block.0, block.1);
    }
    !reclaimed.is_empty()
}
