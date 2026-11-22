//! `portool release` (hardening batch D #5): free the current worktree's
//! block from the ledger and remove its `.env.portool`.

use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::lock;
use crate::paths;
use crate::store;
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `portool release`. See [`release_worktree`] for the ordering and
/// idempotence rules; this only adds the CLI's stdout reporting.
pub fn run() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;
    if release_worktree(&ctx)? {
        println!(
            "portool: released {}",
            crate::display::path(&ctx.worktree_root)
        );
    } else {
        println!(
            "portool: {} had no allocation to release",
            crate::display::path(&ctx.worktree_root)
        );
    }
    Ok(())
}

/// Removes the worktree's `.env.portool` first, then its ledger entry under
/// the lock -- in that order, so a failed env removal keeps the block
/// reserved (external review P1 #2: the unsafe inconsistency would be
/// "block freed in the ledger but the old env still hands out its ports";
/// the safe one is "env gone but block still reserved"). A worktree that
/// had no allocation is success (the desired end state -- no allocation --
/// already holds); the return value says whether an entry was freed.
pub(crate) fn release_worktree(ctx: &GitCtx) -> Result<bool> {
    let common_dir_key = ctx.common_dir.to_string_lossy().into_owned();
    let worktree_key = ctx.worktree_root.to_string_lossy().into_owned();

    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;
    let registry_path = paths::registry_path()?;
    let mut registry = match store::load_strict(&registry_path)? {
        Some(registry) => registry,
        None => {
            // No ledger at all: there is nothing to free. Still remove a
            // stray env file so the end state is consistent.
            remove_env_file(ctx)?;
            return Ok(false);
        }
    };

    let has_entry = registry
        .find_project(&common_dir_key)
        .map(|project| project.worktrees.contains_key(&worktree_key))
        .unwrap_or(false);

    // Env file first: if this fails, the ledger entry is left in place and
    // the block stays reserved.
    remove_env_file(ctx)?;

    if has_entry {
        registry
            .find_project_mut(&common_dir_key)
            .expect("has_entry checked above")
            .worktrees
            .remove(&worktree_key);
        // After the removal above, so a vanished worktree still reports
        // `Freed` here rather than being swept first; then the sweep for
        // every other project rides on the same save.
        crate::reclaim::auto(&mut registry);
        store::save(&registry_path, &mut registry)?;
    } else {
        crate::reclaim::auto_and_save(&mut registry, &registry_path)?;
    }
    Ok(has_entry)
}

/// Removes the worktree's `.env.portool`. Absence is success (it is the
/// desired end state); any other failure is a hard error so the caller
/// never frees the ledger entry while the env file still exists.
fn remove_env_file(ctx: &GitCtx) -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let env_path = ctx.worktree_root.join(".env.portool");
    let dir = match std::fs::File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(&ctx.worktree_root)
    {
        Ok(dir) => dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(Error::General(format!(
                "cannot open {} for release: {err}",
                ctx.worktree_root.display()
            )))
        }
    };
    // SAFETY: dir remains open and the nul-terminated filename is fixed.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), c".env.portool".as_ptr(), 0) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::NotFound {
        return Ok(());
    }
    Err(Error::General(format!(
        "failed to remove {}: {err}; the ledger entry was left in place",
        env_path.display()
    )))
}
