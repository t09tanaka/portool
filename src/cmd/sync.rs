//! `portool sync` (spec §7): a fast, lock-free read-only check that
//! short-circuits when nothing needs to change, falling back to a locked
//! slow path that (re)allocates and writes the ledger + `.env.portool`.

use crate::alloc;
use crate::config::Config;
use crate::envfile;
use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
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

/// Runs `portool sync`. `quiet` suppresses the normal-case stdout summary
/// emitted by the slow path (spec §9.2); warnings and hints always go to
/// stderr regardless.
pub fn run(quiet: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;

    warn_if_hook_missing(&ctx.common_dir);

    if try_fast_path(&ctx)? {
        return Ok(());
    }

    slow_path(&ctx, quiet)
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

/// Spec §7 steps 1-3: read-only, no lock. Returns `true` if the current
/// worktree's ledger entry, manifest hash, and `.env.portool` bytes all
/// already match -- in which case sync is a complete no-op.
fn try_fast_path(ctx: &GitCtx) -> Result<bool> {
    let load_result = store::load(&paths::registry_path());
    if load_result.corrupt || load_result.read_error {
        return Ok(false);
    }

    let common_dir_key = key(&ctx.common_dir);
    let worktree_key = key(&ctx.worktree_root);

    let entry = match load_result
        .registry
        .find_project(&common_dir_key)
        .and_then(|p| p.worktrees.get(&worktree_key))
    {
        Some(entry) => entry,
        None => return Ok(false),
    };

    let manifest_state = load_manifest_state(&ctx.worktree_root)?;
    if entry.manifest_hash != manifest_state.hash {
        return Ok(false);
    }

    let expected = envfile::render(
        &ctx.project_name,
        &worktree_key,
        entry.block,
        manifest_state.manifest.as_ref(),
    );
    let actual = std::fs::read(ctx.worktree_root.join(".env.portool"));
    Ok(matches!(actual, Ok(bytes) if bytes == expected.as_bytes()))
}

/// Spec §7 steps 4-10: locked read-modify-write.
fn slow_path(ctx: &GitCtx, quiet: bool) -> Result<()> {
    let _lock = lock::acquire(&paths::lock_path(), LOCK_TIMEOUT)?;

    let registry_path = paths::registry_path();
    let existed_before = registry_path.exists();
    let load_result = store::load(&registry_path);
    if load_result.read_error {
        return Err(Error::General(format!(
            "failed to read {}; aborting without writing to avoid clobbering an intact ledger",
            registry_path.display()
        )));
    }
    let mut registry = load_result.registry;

    let config = Config::load();
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

    Ok(())
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
    if block_size > config.subrange_size {
        // Frozen decision 4: detected before even trying to allocate a
        // subrange, since no subrange of the configured width could ever
        // hold this block.
        return Err(Error::SubrangeExhausted);
    }

    let mut subranges: Vec<(u16, u16)> = registry
        .find_project(common_dir_key)
        .map(|p| p.subranges.clone())
        .unwrap_or_default();
    let mut global_subranges = registry.all_subranges();

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

        match alloc::find_free_subrange(config.range, &global_subranges, config.subrange_size) {
            Some(new_subrange) => {
                subranges.push(new_subrange);
                global_subranges.push(new_subrange);
            }
            None => return Err(Error::PoolExhausted),
        }
    }
}

/// Frozen decision 10: warns on stderr if the post-checkout hook isn't
/// installed for this project, regardless of which sync path is taken.
fn warn_if_hook_missing(common_dir: &Path) {
    let hook_path = common_dir.join("hooks").join("post-checkout");
    let installed = std::fs::read_to_string(&hook_path)
        .map(|content| content.contains("portool sync"))
        .unwrap_or(false);
    if !installed {
        eprintln!("hint: run 'portool init' to install the post-checkout hook");
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
