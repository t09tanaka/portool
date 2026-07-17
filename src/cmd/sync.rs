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
    // Fail-closed on the config *before* the fast path (external review P1
    // #4): whether a malformed config.toml is an error must not depend on
    // which path this run happens to take.
    let config = Config::load()?;

    warn_if_hook_missing(ctx);

    if let Some(outcome) = try_fast_path(ctx)? {
        return Ok(outcome);
    }

    slow_path(ctx, &config, quiet)
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
    // Read-only, lock-free. Anything other than a valid ledger (missing,
    // corrupt, unsupported version, unreadable) falls through to the slow
    // path, which decides under the lock -- and fails closed there.
    let registry = match store::load(&paths::registry_path()?) {
        store::LedgerLoad::Loaded(registry) => registry,
        _ => return Ok(None),
    };

    let common_dir_key = key(&ctx.common_dir);
    let worktree_key = key(&ctx.worktree_root);

    let entry = match registry
        .find_project(&common_dir_key)
        .and_then(|p| p.worktrees.get(&worktree_key))
    {
        Some(entry) => entry,
        None => return Ok(None),
    };

    // A worktree mid-move (interrupted two-phase update) always takes the
    // slow path, which resolves the pending block under the lock.
    if entry.pending_block.is_some() {
        return Ok(None);
    }

    let manifest_state = load_manifest_state(&ctx.worktree_root)?;
    if entry.manifest_hash != manifest_state.hash {
        return Ok(None);
    }

    let expected = match envfile::render(
        &ctx.project_name,
        &worktree_key,
        entry.block,
        entry.generation,
        manifest_state.manifest.as_ref(),
        &identity::project_id(&ctx.common_dir),
        &identity::worktree_id(&ctx.common_dir, &ctx.worktree_root),
    ) {
        Ok(expected) => expected,
        Err(_) => return Ok(None), // slow path re-derives and fixes
    };
    // Batch D #14: even when block/manifest/env all match, fall through to
    // the (locked) slow path to refresh metadata when the branch changed or
    // `last_seen_at` is on an earlier calendar day -- so `branch` reflects
    // the current branch and `last_seen_at` is a real last-touched date. Day
    // granularity keeps the same-branch, same-day common case lock-free.
    let metadata_current = entry.branch.as_deref() == ctx.branch.as_deref()
        && entry.last_seen_at.date_naive() == Local::now().date_naive();

    let actual = std::fs::read(ctx.worktree_root.join(".env.portool"));
    if !(metadata_current && matches!(actual, Ok(bytes) if bytes == expected.as_bytes())) {
        return Ok(None);
    }

    // Snapshot revalidation (v0.6): the ledger and env were read at
    // different instants without the lock, so a concurrent locked writer
    // (reallocate, release) may have moved this worktree in between.
    // Re-read the ledger and require the exact same (block, generation,
    // no-pending) state; the generation counter makes even an A->B->A
    // move visible. Any difference falls through to the locked slow path.
    let entry_snapshot = (entry.block, entry.generation);
    let still_current = match store::load(&paths::registry_path()?) {
        store::LedgerLoad::Loaded(registry) => registry
            .find_project(&common_dir_key)
            .and_then(|p| p.worktrees.get(&worktree_key))
            .is_some_and(|e| {
                (e.block, e.generation) == entry_snapshot && e.pending_block.is_none()
            }),
        _ => false,
    };
    if !still_current {
        return Ok(None);
    }

    Ok(Some(SyncOutcome {
        block: entry_snapshot.0,
        manifest: manifest_state.manifest,
    }))
}

/// Spec §7 steps 4-10: locked read-modify-write. Fail-closed: a corrupt,
/// unsupported-version, or unreadable ledger aborts here instead of being
/// reset (external review P1 #1); `doctor --repair` is the explicit
/// recovery path.
fn slow_path(ctx: &GitCtx, config: &Config, quiet: bool) -> Result<SyncOutcome> {
    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;

    let registry_path = paths::registry_path()?;
    // Frozen decision 14: a freshly created ledger records the live pool at
    // creation time; it is never updated after that.
    let mut registry =
        store::load_strict(&registry_path)?.unwrap_or_else(|| Registry::empty(config.range));

    let common_dir_key = key(&ctx.common_dir);
    let worktree_key = key(&ctx.worktree_root);

    // Resolve an interrupted two-phase move for this worktree before
    // anything else looks at its blocks: if the crash happened after the
    // env write, the env already carries the pending block -- roll the
    // ledger forward to match it; otherwise roll back (the pending target
    // was reserved but never handed out).
    resolve_pending_move(&mut registry, &common_dir_key, &worktree_key, ctx);

    // Spec §8.1 implicit GC, run BEFORE allocation (external review P1-8):
    // freeing this project's stale entries first lets a re-created worktree
    // on the same branch reclaim its just-freed block instead of forcing a
    // fresh one that only becomes free after the fact.
    if let Some(project) = registry.projects.get_mut(&common_dir_key) {
        let live_worktrees = ctx.worktree_list()?;
        let dir_exists = |p: &Path| p.exists();
        let block_unused = |b: (u16, u16)| ports::block_free(b);
        crate::gc::collect(project, &live_worktrees, &dir_exists, &block_unused);
    }

    let manifest_state = load_manifest_state(&ctx.worktree_root)?;
    let block_size = manifest_state
        .manifest
        .as_ref()
        .map(|m| m.block_size(config.block_align))
        .transpose()?
        .unwrap_or_else(|| default_block_size(config.block_align));

    let existing_entry = registry
        .find_project(&common_dir_key)
        .and_then(|p| p.worktrees.get(&worktree_key))
        .cloned();

    let final_block = match &existing_entry {
        None => allocate_or_reuse_block(
            &registry,
            block_size,
            config,
            &common_dir_key,
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
                let mut bind_ok = |block: (u16, u16)| ports::block_free(block);
                if let Some(grown) = try_grow_in_place(
                    &registry,
                    entry.block,
                    block_size,
                    config.range.1,
                    &mut bind_ok,
                ) {
                    grown
                } else {
                    // In-place growth is impossible, so satisfying the
                    // manifest means moving. Moving while something is
                    // still listening on the current block would split the
                    // worktree across old and new ports (external review
                    // 3rd round P1-1): refuse and ask for an explicit
                    // action instead.
                    if !ports::block_free(entry.block) {
                        return Err(Error::General(format!(
                            "the manifest now needs {} ports but block {}-{} cannot \
                             grow in place, and something is still listening on it; \
                             stop those processes and re-run 'portool sync', or run \
                             'portool reallocate' to move this worktree explicitly",
                            block_size, entry.block.0, entry.block.1
                        )));
                    }
                    allocate_or_reuse_block(
                        &registry,
                        block_size,
                        config,
                        &common_dir_key,
                        ctx.branch.as_deref(),
                        &worktree_key,
                        Some(entry.block),
                    )?
                }
            }
        }
        Some(entry) => entry.block,
    };

    let now = now_local();
    let moving = matches!(&existing_entry, Some(e) if e.block != final_block);
    let new_generation = match &existing_entry {
        Some(entry) if entry.block != final_block => entry.generation + 1,
        Some(entry) => entry.generation,
        None => 1,
    };
    let rendered = envfile::render(
        &ctx.project_name,
        &worktree_key,
        final_block,
        new_generation,
        manifest_state.manifest.as_ref(),
        &identity::project_id(&ctx.common_dir),
        &identity::worktree_id(&ctx.common_dir, &ctx.worktree_root),
    )?;

    // Two-phase update for a block move (external review v0.6): reserve the
    // target alongside the old block, write the env, then finalize. A crash
    // at any point leaves the env's block reserved in the ledger, so no
    // other worktree can be handed a block this one's env still points at.
    if moving {
        let entry = registry
            .find_project_mut(&common_dir_key)
            .expect("existing_entry implies the project exists")
            .worktrees
            .get_mut(&worktree_key)
            .expect("existing_entry implies the entry exists");
        entry.pending_block = Some(final_block);
        store::save(&registry_path, &registry)?;
        crate::fault::point("after_pending_save");

        crate::fault::point("before_env_write");
        store::write_atomic(&ctx.worktree_root.join(".env.portool"), rendered.as_bytes())?;
        crate::fault::point("after_env_write");
    }

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
                generation: new_generation,
                pending_block: None,
                branch: ctx.branch.clone(),
                manifest_hash: manifest_state.hash.clone(),
                pinned: false,
                label: None,
                allocated_at: now,
                last_seen_at: now,
            });
        entry.block = final_block;
        entry.generation = new_generation;
        entry.pending_block = None;
        entry.branch = ctx.branch.clone();
        entry.manifest_hash = manifest_state.hash.clone();
        entry.last_seen_at = now;
    }

    store::save(&registry_path, &registry)?;

    // For a non-move (fresh allocation or in-place refresh), the ledger
    // write comes first: a crash here leaves the block reserved with a
    // stale/missing env, which the next sync simply rewrites.
    if !moving {
        store::write_atomic(&ctx.worktree_root.join(".env.portool"), rendered.as_bytes())?;
    }

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
/// occupied set so a worktree whose manifest outgrew its block may resettle
/// into (an extension of) its own freed slot; `reallocate` passes `None` so
/// the current block can never be chosen again. Errors with
/// [`Error::PoolExhausted`] when the pool has no free, bindable,
/// `block_size`-wide slot left.
fn allocate_or_reuse_block(
    registry: &Registry,
    block_size: u16,
    config: &Config,
    project_key: &str,
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
        project_key,
        branch,
        worktree_key,
        &occupied,
        &mut bind_ok,
    )
    .ok_or(Error::PoolExhausted)
}

/// Tries to widen `current` in place to `new_size` ports, keeping its start
/// (external review 3rd round P1-1: the preferred-slot hash shifts when the
/// block size changes, so the general allocator tends to move a growing
/// worktree even when its own block could simply be extended). The
/// extension must stay inside the pool and clear of every *other* block or
/// reservation; only the newly added tail ports are bind-checked -- the
/// current block's ports are expected to be in use by this worktree's own
/// processes.
fn try_grow_in_place(
    registry: &Registry,
    current: (u16, u16),
    new_size: u16,
    pool_end: u16,
    bind_ok: &mut dyn FnMut((u16, u16)) -> bool,
) -> Option<(u16, u16)> {
    let end = u32::from(current.0) + u32::from(new_size) - 1;
    if end > u32::from(pool_end) {
        return None;
    }
    let candidate = (current.0, end as u16);
    let occupied: Vec<(u16, u16)> = registry
        .all_blocks()
        .into_iter()
        .filter(|&b| b != current)
        .collect();
    if occupied
        .iter()
        .any(|&o| crate::registry::overlaps(o, candidate))
    {
        return None;
    }
    // new_size > current width, so the tail start never overflows past end.
    let tail = (current.1 + 1, candidate.1);
    if !bind_ok(tail) {
        return None;
    }
    Some(candidate)
}

/// `portool reallocate`: discovers the current worktree and forces it onto a
/// fresh block.
pub fn reallocate_cmd(quiet: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;
    reallocate(&ctx, quiet).map(|_| ())
}

/// Forces `ctx`'s worktree onto a fresh free+bindable block -- always a
/// *different* one, per the CLI contract (external review P2 #5) -- and
/// rewrites `.env.portool` (hardening batch C #1). Errors if the worktree
/// has no ledger entry yet (run `sync` first), and with `PoolExhausted`
/// when no other block fits.
pub fn reallocate(ctx: &GitCtx, quiet: bool) -> Result<SyncOutcome> {
    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;

    let config = Config::load()?;
    let registry_path = paths::registry_path()?;
    let mut registry =
        store::load_strict(&registry_path)?.unwrap_or_else(|| Registry::empty(config.range));

    let common_dir_key = key(&ctx.common_dir);
    let worktree_key = key(&ctx.worktree_root);

    // Same recovery as the sync slow path: never allocate on top of an
    // unresolved pending move.
    resolve_pending_move(&mut registry, &common_dir_key, &worktree_key, ctx);

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
        .transpose()?
        .unwrap_or_else(|| default_block_size(config.block_align));

    // The current block stays in the occupied set, so the allocator can
    // never hand it back: "reallocate" must actually move.
    let new_block = allocate_or_reuse_block(
        &registry,
        block_size,
        &config,
        &common_dir_key,
        ctx.branch.as_deref(),
        &worktree_key,
        None,
    )?;

    let new_generation = existing.generation + 1;
    let rendered = envfile::render(
        &ctx.project_name,
        &worktree_key,
        new_block,
        new_generation,
        manifest_state.manifest.as_ref(),
        &identity::project_id(&ctx.common_dir),
        &identity::worktree_id(&ctx.common_dir, &ctx.worktree_root),
    )?;

    // Two-phase move, same protocol as the sync slow path: reserve, write
    // the env, finalize.
    {
        let entry = registry
            .find_project_mut(&common_dir_key)
            .expect("entry existed above")
            .worktrees
            .get_mut(&worktree_key)
            .expect("entry existed above");
        entry.pending_block = Some(new_block);
    }
    store::save(&registry_path, &registry)?;
    crate::fault::point("after_pending_save");

    crate::fault::point("before_env_write");
    store::write_atomic(&ctx.worktree_root.join(".env.portool"), rendered.as_bytes())?;
    crate::fault::point("after_env_write");

    let now = now_local();
    {
        let entry = registry
            .find_project_mut(&common_dir_key)
            .expect("entry existed above")
            .worktrees
            .get_mut(&worktree_key)
            .expect("entry existed above");
        entry.block = new_block;
        entry.generation = new_generation;
        entry.pending_block = None;
        entry.branch = ctx.branch.clone();
        entry.manifest_hash = manifest_state.hash.clone();
        entry.last_seen_at = now;
    }
    store::save(&registry_path, &registry)?;

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
        Some(c) if crate::hooks::contains_portool_invocation(&c) => {
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

/// Resolves an interrupted two-phase move (a `pending_block` left behind by
/// a crash) for one worktree. Must be called under the registry lock. If
/// the worktree's env file already carries the pending block, the move
/// completed on disk and the ledger is rolled forward (the env was written
/// with `generation + 1`, so the counter follows); otherwise the target was
/// reserved but never handed out, and the reservation is rolled back. The
/// caller persists the result with its next save.
fn resolve_pending_move(
    registry: &mut Registry,
    common_dir_key: &str,
    worktree_key: &str,
    ctx: &GitCtx,
) {
    let entry = match registry
        .find_project_mut(common_dir_key)
        .and_then(|p| p.worktrees.get_mut(worktree_key))
    {
        Some(entry) => entry,
        None => return,
    };
    let pending = match entry.pending_block {
        Some(pending) => pending,
        None => return,
    };
    if envfile::read_block_from_env(&ctx.worktree_root) == Some(pending) {
        entry.block = pending;
        entry.generation += 1;
    }
    entry.pending_block = None;
}

/// Local-timezone RFC 3339 timestamp, truncated to whole seconds (cross-task
/// decision: matches the spec's `2026-07-15T10:00:00+09:00` shape).
fn now_local() -> DateTime<FixedOffset> {
    Local::now().trunc_subsecs(0).fixed_offset()
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
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
        };

        let block = allocate_or_reuse_block(
            &registry,
            5,
            &config,
            "/p/.git",
            Some("main"),
            "/project",
            None,
        )
        .unwrap();

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
        };

        let block = allocate_or_reuse_block(
            &registry,
            600, // far wider than the old default subrange_size of 500
            &config,
            "/p/.git",
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
        };

        let result = allocate_or_reuse_block(
            &registry,
            5,
            &config,
            "/p/.git",
            Some("main"),
            "/project",
            None,
        );

        assert!(matches!(result, Err(Error::PoolExhausted)));
    }

    /// External review 3rd round P1-1: a manifest that outgrows its block must
    /// first try extending the current block in place.
    #[test]
    fn try_grow_in_place_extends_keeping_the_start() {
        let registry = Registry::empty((3000, 3999));
        let mut bind_ok = |_b: (u16, u16)| true;
        assert_eq!(
            try_grow_in_place(&registry, (3000, 3004), 10, 3999, &mut bind_ok),
            Some((3000, 3009))
        );
    }

    #[test]
    fn try_grow_in_place_fails_when_a_neighbor_occupies_the_extension() {
        let mut registry = Registry::empty((3000, 3999));
        registry.reservations.push(Reservation {
            block: (3005, 3009),
            label: None,
            pinned: true,
        });
        let mut bind_ok = |_b: (u16, u16)| true;
        assert_eq!(
            try_grow_in_place(&registry, (3000, 3004), 10, 3999, &mut bind_ok),
            None
        );
    }

    #[test]
    fn try_grow_in_place_fails_when_the_pool_ends() {
        let registry = Registry::empty((3000, 3009));
        let mut bind_ok = |_b: (u16, u16)| true;
        assert_eq!(
            try_grow_in_place(&registry, (3005, 3009), 10, 3009, &mut bind_ok),
            None
        );
    }

    /// Only the newly added tail is bind-checked: the current block's ports are
    /// expected to be in use by this worktree's own processes.
    #[test]
    fn try_grow_in_place_bind_checks_only_the_added_tail() {
        let registry = Registry::empty((3000, 3999));
        let mut checked = Vec::new();
        let mut bind_ok = |b: (u16, u16)| {
            checked.push(b);
            true
        };
        try_grow_in_place(&registry, (3000, 3004), 10, 3999, &mut bind_ok);
        assert_eq!(checked, vec![(3005, 3009)]);
    }

    #[test]
    fn try_grow_in_place_fails_when_the_tail_is_bound() {
        let registry = Registry::empty((3000, 3999));
        let mut bind_ok = |_b: (u16, u16)| false;
        assert_eq!(
            try_grow_in_place(&registry, (3000, 3004), 10, 3999, &mut bind_ok),
            None
        );
    }
}
