//! `portool pin` / `unpin` (external review P1-9): mark the current
//! worktree's allocation as exempt from every GC path (implicit sync GC,
//! prune) until unpinned.

use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::lock;
use crate::paths;
use crate::store;
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

pub fn pin(label: Option<String>) -> Result<()> {
    set_pinned(true, label)
}

pub fn unpin() -> Result<()> {
    set_pinned(false, None)
}

fn set_pinned(pinned: bool, label: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;
    let common_dir_key = ctx.common_dir.to_string_lossy().into_owned();
    let worktree_key = ctx.worktree_root.to_string_lossy().into_owned();

    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;
    let registry_path = paths::registry_path()?;
    let mut registry =
        store::load_strict(&registry_path)?.ok_or_else(|| no_allocation(&worktree_key))?;
    let entry = registry
        .find_project_mut(&common_dir_key)
        .and_then(|p| p.worktrees.get_mut(&worktree_key))
        .ok_or_else(|| no_allocation(&worktree_key))?;

    entry.pinned = pinned;
    if pinned {
        if let Some(label) = label {
            entry.label = Some(label);
        }
    } else {
        // A label's lifetime is the pin's lifetime: unpin clears it so a
        // later label-less pin does not resurrect a stale name.
        entry.label = None;
    }
    store::save(&registry_path, &registry)?;
    println!(
        "portool: {} {}",
        if pinned { "pinned" } else { "unpinned" },
        worktree_key
    );
    Ok(())
}

fn no_allocation(worktree: &str) -> Error {
    Error::General(format!(
        "{worktree} has no allocated block yet; run 'portool sync' first"
    ))
}
