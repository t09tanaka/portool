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

/// The chosen block for a worktree, plus its project's (possibly grown)
/// subranges list.
type BlockAllocation = ((u16, u16), Vec<(u16, u16)>);

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
    let actual = std::fs::read(ctx.worktree_root.join(".env.portool"));
    if matches!(actual, Ok(bytes) if bytes == expected.as_bytes()) {
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

    let (final_block, updated_subranges) = match &existing_entry {
        None => {
            let (block, subs) = allocate_or_reuse_block(
                &registry,
                &common_dir_key,
                block_size,
                ctx.branch.as_deref(),
                &worktree_key,
                &config,
                None,
            )?;
            (block, Some(subs))
        }
        Some(entry) if entry.manifest_hash != manifest_state.hash => {
            let current_width = u32::from(entry.block.1) - u32::from(entry.block.0) + 1;
            if u32::from(block_size) <= current_width {
                // Spec §6.4: the new size still fits the existing block;
                // keep it in place and only refresh the hash + env file.
                (entry.block, None)
            } else {
                let (block, subs) = allocate_or_reuse_block(
                    &registry,
                    &common_dir_key,
                    block_size,
                    ctx.branch.as_deref(),
                    &worktree_key,
                    &config,
                    Some(entry.block),
                )?;
                (block, Some(subs))
            }
        }
        Some(entry) => (entry.block, None),
    };

    let now = now_local();
    {
        let project_name = ctx.project_name.clone();
        let project = registry
            .projects
            .entry(common_dir_key.clone())
            .or_insert_with(|| ProjectEntry {
                name: project_name,
                subranges: Vec::new(),
                worktrees: std::collections::BTreeMap::new(),
            });
        if let Some(subs) = updated_subranges {
            project.subranges = subs;
        }

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

/// Finds a block for this worktree within `registry`'s current state
/// (spec §6.2-6.4, frozen decisions 3-5). `existing_block`, if given, is
/// excluded from the occupied set so a worktree being reallocated may
/// resettle into its own freed slot.
///
/// Returns the chosen block and the project's (possibly grown) subranges
/// list; the caller is responsible for writing both back into the
/// registry.
#[allow(clippy::too_many_arguments)]
fn allocate_or_reuse_block(
    registry: &Registry,
    common_dir_key: &str,
    block_size: u16,
    branch: Option<&str>,
    worktree_key: &str,
    config: &Config,
    existing_block: Option<(u16, u16)>,
) -> Result<BlockAllocation> {
    let mut subranges: Vec<(u16, u16)> = registry
        .find_project(common_dir_key)
        .map(|p| p.subranges.clone())
        .unwrap_or_default();
    // Spec §6.2: a newly acquired subrange must avoid both existing
    // subranges and reservations.
    let mut global_subranges = registry.all_subranges();
    global_subranges.extend(registry.reservations.iter().map(|r| r.block));

    let occupied: Vec<(u16, u16)> = registry
        .all_blocks()
        .into_iter()
        .filter(|&b| Some(b) != existing_block)
        .collect();

    loop {
        let mut bind_ok = |block: (u16, u16)| ports::block_free(block);
        if let Some(block) = alloc::allocate_block(
            &subranges,
            block_size,
            branch,
            worktree_key,
            &occupied,
            &mut bind_ok,
        ) {
            return Ok((block, subranges));
        }

        if block_size > config.subrange_size {
            // Frozen decision 4 (spec §12: config changes affect only new
            // acquisitions): the project's existing subranges -- possibly
            // wider than the current config -- were already tried above.
            // Acquiring a NEW subrange can never help here, since it would
            // be capped at the configured width, so stop before looping
            // forever.
            return Err(Error::SubrangeExhausted);
        }

        match alloc::find_free_subrange(config.range, &global_subranges, config.subrange_size) {
            Some(new_subrange) => {
                subranges.push(new_subrange);
                global_subranges.push(new_subrange);
            }
            None => return Err(Error::PoolExhausted),
        }
    }
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

    /// Spec §6.2 deviation fix: subrange acquisition must avoid
    /// reservations, not just other projects' subranges.
    #[test]
    fn allocate_or_reuse_block_avoids_reservation_when_acquiring_first_subrange() {
        let mut registry = Registry::empty((3000, 3999));
        registry.reservations.push(Reservation {
            block: (3000, 3499),
            label: None,
            pinned: true,
        });
        let config = Config {
            range: (3000, 3999),
            subrange_size: 500,
            block_align: 5,
            gc_days: 30,
        };

        let (block, subranges) = allocate_or_reuse_block(
            &registry,
            "/project/.git",
            5,
            Some("main"),
            "/project",
            &config,
            None,
        )
        .unwrap();

        assert_eq!(
            subranges,
            vec![(3500, 3999)],
            "the newly acquired subrange must not overlap the reservation"
        );
        assert_eq!(block, (3500, 3504));
    }

    /// Spec §12: config changes affect only new acquisitions. A project
    /// that already owns a wide subrange must still be able to allocate a
    /// block larger than a since-shrunk `subrange_size`, because the
    /// allocation happens inside the existing subrange -- no NEW subrange
    /// is ever acquired.
    #[test]
    fn allocate_or_reuse_block_fits_within_existing_wide_subrange_after_config_shrinks() {
        let mut registry = Registry::empty((3000, 3999));
        registry.projects.insert(
            "/project/.git".to_string(),
            ProjectEntry {
                name: "project".to_string(),
                subranges: vec![(3000, 3499)],
                worktrees: std::collections::BTreeMap::new(),
            },
        );
        // subrange_size shrunk to 5 well after the project acquired its
        // 500-wide subrange; block_size (10) now exceeds it.
        let config = Config {
            range: (3000, 3999),
            subrange_size: 5,
            block_align: 5,
            gc_days: 30,
        };

        let (block, subranges) = allocate_or_reuse_block(
            &registry,
            "/project/.git",
            10,
            Some("main"),
            "/project",
            &config,
            None,
        )
        .expect("must allocate inside the existing, wider subrange");

        assert_eq!(subranges, vec![(3000, 3499)], "no new subrange acquired");
        assert!(
            block.0 >= 3000 && block.1 <= 3499,
            "block {block:?} must fall within the existing subrange"
        );
    }

    /// Inverse of the above: a project with NO existing subranges, whose
    /// block size exceeds the configured `subrange_size`, can never be
    /// helped by acquiring a new subrange (frozen decision 4) and must
    /// fail fast with `SubrangeExhausted`.
    #[test]
    fn allocate_or_reuse_block_errors_when_no_subranges_and_block_exceeds_subrange_size() {
        let registry = Registry::empty((3000, 3999));
        let config = Config {
            range: (3000, 3999),
            subrange_size: 3,
            block_align: 5,
            gc_days: 30,
        };

        let result = allocate_or_reuse_block(
            &registry,
            "/project/.git",
            5,
            Some("main"),
            "/project",
            &config,
            None,
        );

        assert!(matches!(result, Err(Error::SubrangeExhausted)));
    }
}
