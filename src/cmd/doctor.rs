//! `portool doctor` (hardening batch D #5, C4): diagnose and repair the
//! current project.
//!
//! - **Rebuild**: re-imports blocks recorded in live worktrees'
//!   `.env.portool` that the ledger has lost (e.g. after a corruption reset
//!   moved the old ledger aside). Import is overlap-guarded, so a corrupt
//!   block baked into an env file is reported and skipped rather than
//!   re-produced in the ledger.
//! - **Report**: flags this project's blocks whose ports are currently in
//!   use (on `127.0.0.1`), and any moved-aside corrupt ledger.
//!
//! Rebuild is per-project: it only touches the project `doctor` runs in;
//! other projects stay dropped until `doctor` runs in each. The moved-aside
//! `registry.json.corrupt-<ts>` file (printed by the loader) is the
//! authoritative artifact for reconciling projects `doctor` didn't rebuild.

use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::lock;
use crate::paths;
use crate::ports;
use crate::registry::{overlaps, ProjectEntry, WorktreeEntry};
use crate::store;
use chrono::{DateTime, FixedOffset, Local, SubsecRound};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `portool doctor` for the current project.
pub fn run() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;
    let common_dir_key = ctx.common_dir.to_string_lossy().into_owned();

    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;
    let registry_path = paths::registry_path()?;
    let load = store::load(&registry_path, true);
    if load.read_error {
        return Err(Error::General(
            "failed to read the ledger; aborting without writing".to_string(),
        ));
    }
    let mut registry = load.registry;

    // 1. Rebuild lost entries from live worktrees' env files.
    let live = ctx.worktree_list()?;
    let mut occupied = registry.all_blocks();
    let now = now_local();
    let mut imported = 0usize;

    for wt in &live {
        let wt_key = wt.to_string_lossy().into_owned();
        let already = registry
            .find_project(&common_dir_key)
            .map(|p| p.worktrees.contains_key(&wt_key))
            .unwrap_or(false);
        if already {
            continue;
        }
        let block = match read_block_from_env(wt) {
            Some(block) => block,
            None => continue,
        };
        if occupied.iter().any(|&b| overlaps(b, block)) {
            eprintln!(
                "portool: doctor: {} records block {}-{}, which overlaps an existing \
                 allocation; skipping re-import (fix its .env.portool or run reallocate)",
                wt.display(),
                block.0,
                block.1
            );
            continue;
        }

        let project = registry
            .projects
            .entry(common_dir_key.clone())
            .or_insert_with(|| ProjectEntry {
                name: ctx.project_name.clone(),
                worktrees: BTreeMap::new(),
            });
        project.worktrees.insert(
            wt_key,
            WorktreeEntry {
                block,
                branch: None,
                manifest_hash: None,
                pinned: false,
                label: None,
                allocated_at: now,
                last_seen_at: now,
            },
        );
        occupied.push(block);
        imported += 1;
        println!(
            "portool: doctor: re-imported {}-{} for {}",
            block.0,
            block.1,
            wt.display()
        );
    }

    if imported > 0 {
        store::save(&registry_path, &registry)?;
    }

    // 2. Report this project's blocks whose ports are currently in use.
    let mut in_use = 0usize;
    if let Some(project) = registry.find_project(&common_dir_key) {
        for (path, entry) in &project.worktrees {
            if !ports::block_free(entry.block) {
                in_use += 1;
                println!(
                    "portool: doctor: block {}-{} ({path}) has ports in use \
                     -- may be this worktree's own processes",
                    entry.block.0, entry.block.1
                );
            }
        }
    }

    if imported == 0 && in_use == 0 {
        println!("portool: doctor: nothing to repair for this project");
    }
    Ok(())
}

/// Extracts the block from a worktree's `.env.portool` header line
/// (`# block: START-END  ...`), or `None` if the file or line is absent or
/// unparseable.
fn read_block_from_env(worktree: &Path) -> Option<(u16, u16)> {
    let contents = std::fs::read_to_string(worktree.join(".env.portool")).ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("# block: ") {
            let token = rest.split_whitespace().next()?;
            let (start, end) = token.split_once('-')?;
            return Some((start.parse().ok()?, end.parse().ok()?));
        }
    }
    None
}

fn now_local() -> DateTime<FixedOffset> {
    Local::now().trunc_subsecs(0).fixed_offset()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn read_block_from_env_parses_the_header() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".env.portool"),
            "# generated by portool \u{2014} DO NOT EDIT\n\
             # block: 3005-3009  project: p  worktree: /w\n\
             PORT=3005\n",
        )
        .unwrap();
        assert_eq!(read_block_from_env(tmp.path()), Some((3005, 3009)));
    }

    #[test]
    fn read_block_from_env_missing_file_is_none() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(read_block_from_env(tmp.path()), None);
    }
}
