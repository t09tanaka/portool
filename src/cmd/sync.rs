//! `portool sync` (spec §7): a fast, lock-free read-only check that
//! short-circuits when nothing needs to change, falling back to a locked
//! slow path that (re)allocates and writes the ledger + `.env.portool`.

use crate::alloc;
use crate::config::Config;
use crate::envfile;
use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::identity;
use crate::lock;
use crate::manifest::{default_block_size, manifest_hash, Manifest};
use crate::paths;
use crate::ports;
use crate::registry::{ProjectEntry, Registry, WorktreeEntry};
use crate::store;
use chrono::{DateTime, FixedOffset, Local, SubsecRound};
use std::path::Path;
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// The result of a successful sync: the worktree's block and its parsed
/// manifest (if any), for callers that need the allocated values.
pub struct SyncOutcome {
    pub block: (u16, u16),
    pub manifest: Option<Manifest>,
}

/// Runs `portool sync`. `quiet` suppresses the normal-case stdout summary
/// emitted by the slow path (spec §9.2); warnings and hints always go to
/// stderr regardless.
pub fn run(quiet: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;
    ensure(&ctx, quiet).map(|_| ())
}

/// Runs the sync algorithm for an already-discovered worktree and returns
/// the resulting allocation.
pub fn ensure(ctx: &GitCtx, quiet: bool) -> Result<SyncOutcome> {
    warn_if_hook_missing(ctx);

    if let Some(outcome) = try_fast_path(ctx)? {
        return Ok(outcome);
    }

    slow_path(ctx, quiet)
}

/// The manifest state for a worktree: the parsed manifest (if any) plus the
/// hash of its raw bytes, computed together so the two can never disagree.
struct ManifestState {
    manifest: Option<Manifest>,
    hash: Option<String>,
}

fn load_manifest_state(worktree_root: &Path) -> Result<ManifestState> {
    let path = worktree_root.join(".portool.toml");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| Error::General(format!("{} is not valid UTF-8", path.display())))?;
            let manifest = Manifest::parse(text)?;
            let hash = manifest_hash(&bytes);
            Ok(ManifestState {
                manifest: Some(manifest),
                hash: Some(hash),
            })
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ManifestState {
            manifest: None,
            hash: None,
        }),
        Err(err) => Err(Error::from(err)),
    }
}

/// Spec §7 steps 1-3: read-only, no lock. Returns the current allocation
/// if the worktree's ledger entry, manifest hash, and `.env.portool` bytes
/// all already match -- in which case sync is a complete no-op.
fn try_fast_path(ctx: &GitCtx) -> Result<Option<SyncOutcome>> {
    // Read-only, lock-free: never heal a corrupt ledger here (finding 3).
    let load_result = store::load(&paths::registry_path()?, false);
    if load_result.corrupt || load_result.read_error {
        return Ok(None);
    }

    let common_dir_key = key(&ctx.common_dir);
    let worktree_key = key(&ctx.worktree_root);

    let entry = match load_result
        .registry
        .find_project(&common_dir_key)
        .and_then(|p| p.worktrees.get(&worktree_key))
    {
        Some(entry) => entry,
        None => return Ok(None),
    };

    let manifest_state = load_manifest_state(&ctx.worktree_root)?;
    if entry.manifest_hash != manifest_state.hash {
        return Ok(None);
    }

    let expected = envfile::render(
        &ctx.project_name,
        &worktree_key,
        entry.block,
        manifest_state.manifest.as_ref(),
        &identity::project_id(&ctx.common_dir),
        &identity::worktree_id(&ctx.common_dir, &ctx.worktree_root),
    );
    // Batch D #14: even when block/manifest/env all match, fall through to
    // the (locked) slow path to refresh metadata when the branch changed or
    // `last_seen_at` is on an earlier calendar day -- so `branch` reflects
    // the current branch and `last_seen_at` is a real last-touched date. Day
    // granularity keeps the same-branch, same-day common case lock-free.
    let metadata_current = entry.branch.as_deref() == ctx.branch.as_deref()
        && entry.last_seen_at.date_naive() == Local::now().date_naive();

    let actual = std::fs::read(ctx.worktree_root.join(".env.portool"));
    if metadata_current && matches!(actual, Ok(bytes) if bytes == expected.as_bytes()) {
        Ok(Some(SyncOutcome {
            block: entry.block,
            manifest: manifest_state.manifest,
        }))
    } else {
        Ok(None)
    }
}

/// Spec §7 steps 4-10: locked read-modify-write.
fn slow_path(ctx: &GitCtx, quiet: bool) -> Result<SyncOutcome> {
    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;

    let registry_path = paths::registry_path()?;
    let existed_before = registry_path.exists();
    // Under the flock: safe to heal (rename aside) a corrupt ledger.
    let load_result = store::load(&registry_path, true);
    if load_result.read_error {
        return Err(Error::General(format!(
            "failed to read {}; aborting without writing to avoid clobbering an intact ledger",
            registry_path.display()
        )));
    }
    let mut registry = load_result.registry;

    let config = Config::load()?;
    if !existed_before || load_result.corrupt {
        // Frozen decision 14: a freshly created ledger records the live
        // pool at creation time; it is never updated after that.
        registry.range = config.range;
    }

    let common_dir_key = key(&ctx.common_dir);
    let worktree_key = key(&ctx.worktree_root);

    let manifest_state = load_manifest_state(&ctx.worktree_root)?;
    let block_size = manifest_state
        .manifest
        .as_ref()
        .map(|m| m.block_size(config.block_align))
        .unwrap_or_else(|| default_block_size(config.block_align));

    let existing_entry = registry
        .find_project(&common_dir_key)
        .and_then(|p| p.worktrees.get(&worktree_key))
        .cloned();

    let final_block = match &existing_entry {
        None => allocate_or_reuse_block(
            &registry,
            block_size,
            &config,
            ctx.branch.as_deref(),
            &worktree_key,
            None,
        )?,
        Some(entry) if entry.manifest_hash != manifest_state.hash => {
            let current_width = u32::from(entry.block.1) - u32::from(entry.block.0) + 1;
            if u32::from(block_size) <= current_width {
                // Spec §6.4: the new size still fits the existing block;
                // keep it in place and only refresh the hash + env file.
                entry.block
            } else {
                allocate_or_reuse_block(
                    &registry,
                    block_size,
                    &config,
                    ctx.branch.as_deref(),
                    &worktree_key,
                    Some(entry.block),
                )?
            }
        }
        Some(entry) => entry.block,
    };

    let now = now_local();
    {
        let project_name = ctx.project_name.clone();
        let project = registry
            .projects
            .entry(common_dir_key.clone())
            .or_insert_with(|| ProjectEntry {
                name: project_name,
                worktrees: std::collections::BTreeMap::new(),
            });

        let entry = project
            .worktrees
            .entry(worktree_key.clone())
            .or_insert_with(|| WorktreeEntry {
                block: final_block,
                branch: ctx.branch.clone(),
                manifest_hash: manifest_state.hash.clone(),
                pinned: false,
                label: None,
                allocated_at: now,
                last_seen_at: now,
            });
        entry.block = final_block;
        entry.branch = ctx.branch.clone();
        entry.manifest_hash = manifest_state.hash.clone();
        entry.last_seen_at = now;
    }

    // Spec §8.1: implicit GC of this project's own stale entries.
    {
        let project = registry
            .projects
            .get_mut(&common_dir_key)
            .expect("just inserted above");
        let live_worktrees = ctx.worktree_list()?;
        let dir_exists = |p: &Path| p.exists();
        let block_unused = |b: (u16, u16)| ports::block_free(b);
        crate::gc::collect(project, &live_worktrees, &dir_exists, &block_unused);
    }

    store::save(&registry_path, &registry)?;

    let rendered = envfile::render(
        &ctx.project_name,
        &worktree_key,
        final_block,
        manifest_state.manifest.as_ref(),
        &identity::project_id(&ctx.common_dir),
        &identity::worktree_id(&ctx.common_dir, &ctx.worktree_root),
    );
    write_atomic(&ctx.worktree_root.join(".env.portool"), rendered.as_bytes())?;

    if !quiet {
        let verb = match &existing_entry {
            None => "allocated",
            Some(entry) if entry.block != final_block => "reallocated",
            Some(_) => "synced",
        };
        println!(
            "portool: {verb} {}-{} for {}",
            final_block.0,
            final_block.1,
            ctx.worktree_root.display()
        );
    }

    Ok(SyncOutcome {
        block: final_block,
        manifest: manifest_state.manifest,
    })
}

/// Finds a block for this worktree directly from the configured pool
/// (hardening batch C). `existing_block`, if given, is excluded from the
/// occupied set so a worktree being reallocated may resettle into its own
/// freed slot. Errors with [`Error::PoolExhausted`] when the pool has no
/// free, bindable, `block_size`-wide slot left.
fn allocate_or_reuse_block(
    registry: &Registry,
    block_size: u16,
    config: &Config,
    branch: Option<&str>,
    worktree_key: &str,
    existing_block: Option<(u16, u16)>,
) -> Result<(u16, u16)> {
    let occupied: Vec<(u16, u16)> = registry
        .all_blocks()
        .into_iter()
        .filter(|&b| Some(b) != existing_block)
        .collect();

    let mut bind_ok = |block: (u16, u16)| ports::block_free(block);
    alloc::allocate_block(
        config.range,
        block_size,
        config.block_align,
        branch,
        worktree_key,
        &occupied,
        &mut bind_ok,
    )
    .ok_or(Error::PoolExhausted)
}

/// `portool reallocate`: discovers the current worktree and forces it onto a
/// fresh block.
pub fn reallocate_cmd(quiet: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;
    reallocate(&ctx, quiet).map(|_| ())
}

/// Forces `ctx`'s worktree onto a fresh free+bindable block, excluding its
/// current one, and rewrites `.env.portool` (hardening batch C #1). Errors if
/// the worktree has no ledger entry yet (run `sync` first). Its own current
/// block stays eligible, so with no conflict it may keep it; the point is to
/// escape a block whose ports something else now holds.
pub fn reallocate(ctx: &GitCtx, quiet: bool) -> Result<SyncOutcome> {
    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;

    let registry_path = paths::registry_path()?;
    let load_result = store::load(&registry_path, true);
    if load_result.read_error {
        return Err(Error::General(format!(
            "failed to read {}; aborting without writing to avoid clobbering an intact ledger",
            registry_path.display()
        )));
    }
    let mut registry = load_result.registry;
    let config = Config::load()?;

    let common_dir_key = key(&ctx.common_dir);
    let worktree_key = key(&ctx.worktree_root);

    let existing = registry
        .find_project(&common_dir_key)
        .and_then(|p| p.worktrees.get(&worktree_key))
        .cloned()
        .ok_or_else(|| {
            Error::General(format!(
                "{} has no allocated block yet; run 'portool sync' first",
                ctx.worktree_root.display()
            ))
        })?;

    let manifest_state = load_manifest_state(&ctx.worktree_root)?;
    let block_size = manifest_state
        .manifest
        .as_ref()
        .map(|m| m.block_size(config.block_align))
        .unwrap_or_else(|| default_block_size(config.block_align));

    let new_block = allocate_or_reuse_block(
        &registry,
        block_size,
        &config,
        ctx.branch.as_deref(),
        &worktree_key,
        Some(existing.block),
    )?;

    let now = now_local();
    {
        let project = registry
            .projects
            .get_mut(&common_dir_key)
            .expect("entry existed above");
        let entry = project
            .worktrees
            .get_mut(&worktree_key)
            .expect("entry existed above");
        entry.block = new_block;
        entry.branch = ctx.branch.clone();
        entry.manifest_hash = manifest_state.hash.clone();
        entry.last_seen_at = now;
    }

    store::save(&registry_path, &registry)?;

    let rendered = envfile::render(
        &ctx.project_name,
        &worktree_key,
        new_block,
        manifest_state.manifest.as_ref(),
        &identity::project_id(&ctx.common_dir),
        &identity::worktree_id(&ctx.common_dir, &ctx.worktree_root),
    );
    write_atomic(&ctx.worktree_root.join(".env.portool"), rendered.as_bytes())?;

    if !quiet {
        println!(
            "portool: reallocated {}-{} for {}",
            new_block.0,
            new_block.1,
            ctx.worktree_root.display()
        );
    }

    Ok(SyncOutcome {
        block: new_block,
        manifest: manifest_state.manifest,
    })
}

/// Frozen decision 10 (+ batch A #2): warns on stderr if the post-checkout
/// hook isn't installed for this project, or hints to re-run `init` if the
/// installed hook uses an unsafe old form that can fail `git checkout`.
/// Checks the *effective* hook location (honoring `core.hooksPath` /
/// Husky); an uninstallable location counts as missing -- `init` explains
/// the details.
fn warn_if_hook_missing(ctx: &GitCtx) {
    let content = crate::hooks::HooksLocation::resolve(ctx)
        .hook_file("post-checkout")
        .and_then(|hook_path| std::fs::read_to_string(hook_path).ok());
    match content {
        Some(c) if c.contains(crate::hooks::HOOK_MARKER) => {
            if crate::cmd::init::contains_unsafe_portool_form(&c) {
                eprintln!(
                    "portool: your post-checkout hook uses an old form that can fail \
                     'git checkout'; run 'portool init' to update it"
                );
            }
        }
        _ => eprintln!("hint: run 'portool init' to install the post-checkout hook"),
    }
}

/// Local-timezone RFC 3339 timestamp, truncated to whole seconds (cross-task
/// decision: matches the spec's `2026-07-15T10:00:00+09:00` shape).
fn now_local() -> DateTime<FixedOffset> {
    Local::now().trunc_subsecs(0).fixed_offset()
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .ok_or_else(|| Error::General(format!("{} has no parent directory", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(contents)?;
    tmp.persist(path).map_err(|e| Error::from(e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Reservation;

    /// Batch C: blocks come directly from the pool; a block must avoid an
    /// existing reservation.
    #[test]
    fn allocate_or_reuse_block_avoids_a_reservation() {
        let mut registry = Registry::empty((3000, 3999));
        registry.reservations.push(Reservation {
            block: (3000, 3004),
            label: None,
            pinned: true,
        });
        let config = Config {
            range: (3000, 3999),
            block_align: 5,
            gc_days: 30,
        };

        let block =
            allocate_or_reuse_block(&registry, 5, &config, Some("main"), "/project", None).unwrap();

        assert!(
            !crate::registry::overlaps(block, (3000, 3004)),
            "block {block:?} must not overlap the reservation"
        );
        assert!(block.0 >= 3000 && block.1 <= 3999);
    }

    /// Batch C: a block larger than the old (removed) subrange_size is fine --
    /// it is simply carved from the whole pool.
    #[test]
    fn allocate_or_reuse_block_allocates_a_large_block_from_the_pool() {
        let registry = Registry::empty((3000, 3999));
        let config = Config {
            range: (3000, 3999),
            block_align: 5,
            gc_days: 30,
        };

        let block = allocate_or_reuse_block(
            &registry,
            600, // far wider than the old default subrange_size of 500
            &config,
            Some("main"),
            "/project",
            None,
        )
        .expect("a 600-wide block fits the 1000-wide pool");

        assert_eq!(block.1 - block.0, 599);
        assert!(block.0 >= 3000 && block.1 <= 3999);
    }

    /// Batch C: when the pool has no free slot left, allocation fails with
    /// `PoolExhausted` (exit 3); the old `SubrangeExhausted` (exit 2) is gone.
    #[test]
    fn allocate_or_reuse_block_errors_when_pool_is_full() {
        let mut registry = Registry::empty((3000, 3009));
        // Two 5-wide reservations fill the entire 10-wide pool.
        for block in [(3000, 3004), (3005, 3009)] {
            registry.reservations.push(Reservation {
                block,
                label: None,
                pinned: true,
            });
        }
        let config = Config {
            range: (3000, 3009),
            block_align: 5,
            gc_days: 30,
        };

        let result = allocate_or_reuse_block(&registry, 5, &config, Some("main"), "/project", None);

        assert!(matches!(result, Err(Error::PoolExhausted)));
    }
}
