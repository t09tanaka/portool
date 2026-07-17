//! `portool release` (hardening batch D #5): free the current worktree's
//! block from the ledger and remove its `.env.portool`.

use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::lock;
use crate::paths;
use crate::store;
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `portool release`. Removes this worktree's entry from the ledger
/// under the lock and deletes its `.env.portool`. A worktree that had no
/// allocation is reported and treated as success (the desired end state --
/// no allocation -- already holds).
pub fn run() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;
    let common_dir_key = ctx.common_dir.to_string_lossy().into_owned();
    let worktree_key = ctx.worktree_root.to_string_lossy().into_owned();

    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;
    let registry_path = paths::registry_path()?;
    let load = store::load(&registry_path, true);
    if load.read_error {
        return Err(Error::General(
            "failed to read the ledger; aborting without writing".to_string(),
        ));
    }
    let mut registry = load.registry;

    let removed = registry
        .find_project_mut(&common_dir_key)
        .and_then(|project| project.worktrees.remove(&worktree_key))
        .is_some();

    if removed {
        store::save(&registry_path, &registry)?;
    }
    // Remove the env file regardless (best effort): its absence is the point.
    let _ = std::fs::remove_file(ctx.worktree_root.join(".env.portool"));

    if removed {
        println!("portool: released {}", ctx.worktree_root.display());
    } else {
        println!(
            "portool: {} had no allocation to release",
            ctx.worktree_root.display()
        );
    }
    Ok(())
}
