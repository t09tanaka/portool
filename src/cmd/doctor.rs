//! `portool doctor` (hardening batch D #5, C4; restore-first repair added in
//! v0.7.0, external review P0-1): diagnose and repair the current project.
//!
//! - **Repair** (`--repair`): the one and only place a bad ledger is set
//!   aside to `registry.json.corrupt-<nanos>-<pid>`. Every other command fails
//!   closed on such a ledger and points here. Two distinct cases:
//!   - *Corrupt*: restore-first. A valid `registry.json.bak` is restored in
//!     place of the corrupt file, so every other project's allocations
//!     survive -- restoring costs at most the one save since the last
//!     backup refresh. The corrupt file is *copied* (never renamed) aside,
//!     so `registry.json` is never missing at any instant even if the
//!     restore's save fails. Without a valid backup, plain `--repair`
//!     refuses and points at `--abandon-other-projects`.
//!   - *Unsupported version* (written by a newer portool): never
//!     auto-restored from backup (that would silently roll back a newer
//!     binary's ledger). `--repair` alone always errors toward upgrading.
//!   - `--abandon-other-projects`: by explicit request only, either case
//!     falls back to the old destructive move-aside-and-start-empty.
//! - **Rebuild**: re-imports blocks recorded in live worktrees'
//!   `.env.portool` that the ledger has lost (e.g. after `--repair`
//!   abandoned the old ledger, or for any project not yet reconciled after
//!   a restore). Import is validity- and overlap-guarded, so a nonsense
//!   block baked into an env file is reported and skipped rather than
//!   written into the ledger.
//! - **Report**: flags this project's blocks whose ports are currently in
//!   use (on `127.0.0.1`).
//!
//! Rebuild is per-project: it only touches the project `doctor` runs in;
//! other projects stay dropped until `doctor` runs in each (unless a
//! backup restore already brought them back). The set-aside
//! `registry.json.corrupt-<nanos>-<pid>` file is the authoritative artifact for
//! reconciling anything `doctor` didn't already restore or rebuild.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::lock;
use crate::paths;
use crate::ports;
use crate::registry::{overlaps, ProjectEntry, Registry, WorktreeEntry};
use crate::store;
use chrono::{DateTime, FixedOffset, Local, SubsecRound};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `portool doctor` for the current project. Without `repair`, a bad
/// ledger is a hard error; with it, a corrupt ledger is restored from its
/// backup (all projects kept) unless `abandon_other_projects` forces the
/// destructive move-aside-and-start-empty path; an unsupported-version
/// ledger is only ever discarded via `abandon_other_projects`, never
/// auto-restored from backup. Either way this project's entries are then
/// rebuilt from live worktrees' `.env.portool`.
pub fn run(repair: bool, abandon_other_projects: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;
    let common_dir_key = ctx.common_dir.to_string_lossy().into_owned();

    let config = Config::load()?;
    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;
    let registry_path = paths::registry_path()?;
    let mut restored_from_backup = false;
    let mut registry = match store::load(&registry_path) {
        store::LedgerLoad::Loaded(registry) => registry,
        store::LedgerLoad::Missing => Registry::empty(config.range),
        store::LedgerLoad::ReadError { reason } => {
            // Not repairable from here: the file may be perfectly intact and
            // merely unreadable (permissions, EIO); moving it aside blind
            // could destroy a healthy ledger.
            return Err(Error::General(format!(
                "failed to read {} ({reason}); fix the underlying I/O problem first",
                registry_path.display()
            )));
        }
        store::LedgerLoad::UnsupportedVersion { found, supported } => {
            if !repair || !abandon_other_projects {
                return Err(Error::General(format!(
                    "{} uses registry schema version {found}, but this build understands \
                     version {supported}; upgrade portool instead. (If you really want to \
                     discard that ledger, re-run with 'portool doctor --repair \
                     --abandon-other-projects'.)",
                    registry_path.display()
                )));
            }
            let moved_to = store::move_aside(&registry_path)?;
            eprintln!(
                "portool: doctor: abandoned the version-{found} ledger (moved aside to {})",
                crate::display::path(&moved_to)
            );
            Registry::empty(config.range)
        }
        store::LedgerLoad::Corrupt { reason } => {
            if !repair {
                return Err(Error::General(format!(
                    "{} is corrupt ({reason}); re-run with 'portool doctor --repair' to \
                     restore it from backup and rebuild this project's entries",
                    registry_path.display()
                )));
            }
            let (registry, restored) =
                repair_corrupt(&registry_path, &reason, abandon_other_projects, &config)?;
            restored_from_backup = restored;
            registry
        }
    };

    // 1. Rebuild lost entries from live worktrees' env files.
    let live = ctx.worktree_list()?;
    let mut occupied = registry.all_blocks();
    let now = now_local();
    let mut imported = 0usize;

    for wt in &live {
        let wt_key = match utf8_worktree_key(wt) {
            Some(s) => s,
            None => {
                println!(
                    "doctor: warning: skipping worktree with non-UTF-8 path: {}",
                    crate::display::path(wt)
                );
                continue;
            }
        };
        let already = registry
            .find_project(&common_dir_key)
            .map(|p| p.worktrees.contains_key(&wt_key))
            .unwrap_or(false);
        if already {
            continue;
        }
        let block = match crate::envfile::read_block_from_env(wt) {
            Some(block) => block,
            None => continue,
        };
        // Three-valued identity check (external review v0.10 P0-5):
        // - `Absent` (no ID lines) bypasses the cross-check by design -- a
        //   pre-identity or hand-edited file has nothing to verify, and the
        //   accidental-copy threat model is specifically about *present but
        //   wrong* IDs. It falls through to normal block validation.
        // - `Partial` (exactly one ID line) is corruption, never conflated
        //   with `Absent`: refuse to import and ask for manual repair.
        match crate::envfile::read_identity_from_env(wt) {
            crate::envfile::EnvIdentity::Complete(env_project_id, env_worktree_id) => {
                let expect_p = crate::identity::project_id(&ctx.common_dir);
                let expect_w = crate::identity::worktree_id(&ctx.common_dir, wt);
                if env_project_id != expect_p || env_worktree_id != expect_w {
                    println!(
                        "doctor: warning: {}/.env.portool identifies a different \
                         project/worktree (copied from elsewhere, or written by portool \
                         < 0.9?); not importing its block",
                        crate::display::text(&wt_key)
                    );
                    continue;
                }
            }
            crate::envfile::EnvIdentity::Partial => {
                println!(
                    "doctor: warning: {}/.env.portool has only one of \
                     PORTOOL_PROJECT_ID/PORTOOL_WORKTREE_ID (corrupt or half-written); \
                     not importing -- repair or remove its .env.portool by hand",
                    crate::display::text(&wt_key)
                );
                continue;
            }
            crate::envfile::EnvIdentity::Absent => { /* pre-identity file: fall through */ }
        }
        if let Err(reason) = validate_block(block) {
            eprintln!(
                "portool: doctor: {} records block {}-{}, which {reason}; skipping \
                 re-import (fix its .env.portool or run reallocate)",
                crate::display::path(wt),
                block.0,
                block.1
            );
            continue;
        }
        if occupied.iter().any(|&b| overlaps(b, block)) {
            eprintln!(
                "portool: doctor: {} records block {}-{}, which overlaps an existing \
                 allocation; skipping re-import (fix its .env.portool or run reallocate)",
                crate::display::path(wt),
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
                generation: 1,
                pending_block: None,
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
            crate::display::path(wt)
        );
    }

    // P0-2 sequence reconciliation: after a stale-backup restore, the
    // restored ledger's `sequence` can sit *below* a tracked worktree's env
    // sequence (the env was written at a sequence the ledger has since
    // regressed past). That state permanently quarantines `sync`
    // (`env_seq > ledger_seq`), and its user-facing remedy is *this* command,
    // so `doctor` must clear it: having just reconciled every live block, the
    // ledger is authoritative, so advance its sequence to at least the
    // highest env sequence. The subsequent `save` bumps it strictly past
    // that, clearing the quarantine and refreshing the (now-fresh) backup.
    let mut sequence_advanced = false;
    if let Some(max_env_seq) = max_project_env_sequence(&registry, &common_dir_key) {
        if registry.sequence <= max_env_seq {
            registry.sequence = max_env_seq;
            sequence_advanced = true;
        }
    }

    if imported > 0 || sequence_advanced {
        // Never persist a ledger the next command would reject as corrupt:
        // the per-block guards above should make this unreachable, but a
        // validation failure here must abort the save, not ship.
        registry.validate().map_err(|err| {
            Error::General(format!(
                "doctor produced an invalid ledger ({err}); nothing was saved"
            ))
        })?;
        store::save(&registry_path, &mut registry)?;
        if sequence_advanced {
            println!(
                "portool: doctor: advanced the ledger sequence past this project's env \
                 files (cleared a stale-backup quarantine)"
            );
        }
    }

    // 2. Report this project's blocks whose ports are currently in use.
    let mut in_use = 0usize;
    if let Some(project) = registry.find_project(&common_dir_key) {
        for (path, entry) in &project.worktrees {
            if !ports::block_free(entry.block) {
                in_use += 1;
                println!(
                    "portool: doctor: block {}-{} ({}) has ports in use \
                     -- may be this worktree's own processes",
                    entry.block.0,
                    entry.block.1,
                    crate::display::text(path)
                );
            }
        }
    }

    // 3. Hook effectiveness (external review P1-4): "installed" must mean
    //    "will actually run".
    let hook_findings = report_hook_health(&ctx);

    // 4. P0-2: after a restore-from-backup, this project has been reconciled
    //    from its live worktrees' env blocks, but other projects have NOT --
    //    doctor is per-project. Report this machine-readably so automation can
    //    tell that a recovery-loss window may exist until `doctor --repair`
    //    runs in each other project too.
    if restored_from_backup {
        println!(
            "{{\"portool_recovery\":{{\"restored_from_backup\":true,\"restored_sequence\":{},\"reconciled_project\":{},\"other_projects_may_need_repair\":true}}}}",
            registry.sequence,
            serde_json::to_string(&common_dir_key).expect("string serializes")
        );
    }

    if imported == 0 && in_use == 0 && hook_findings == 0 && !restored_from_backup {
        println!("portool: doctor: nothing to repair for this project");
    }
    Ok(())
}

/// Diagnoses whether each managed hook is actually installed and will run:
/// missing, non-executable, present but not invoking portool, or invoking a
/// `PORTOOL_BIN` path that no longer exists (or relying on PATH lookup
/// only). All advisories -- doctor's exit code is unaffected -- but a
/// nonzero return suppresses the "nothing to repair" summary line.
fn report_hook_health(ctx: &GitCtx) -> usize {
    use crate::cmd::init::{managed_block_state, ManagedBlockState};
    use crate::hooks::{self, HooksLocation};
    use std::os::unix::fs::PermissionsExt;

    let loc = HooksLocation::resolve(ctx);
    let mut findings = 0usize;
    for name in ["post-checkout", "post-merge"] {
        let Some(path) = loc.hook_file(name) else {
            println!("portool: doctor: hook {name}: no installable location (see 'portool init')");
            findings += 1;
            continue;
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => {
                println!("portool: doctor: hook {name}: not installed (run 'portool init')");
                findings += 1;
                continue;
            }
        };
        if std::fs::metadata(&path)
            .map(|m| m.permissions().mode() & 0o100 == 0)
            .unwrap_or(true)
        {
            println!(
                "portool: doctor: hook {name}: not executable (chmod +x {})",
                crate::display::path(&path)
            );
            findings += 1;
            continue;
        }
        if !hooks::contains_portool_invocation(&content) {
            println!("portool: doctor: hook {name}: does not invoke portool (run 'portool init')");
            findings += 1;
            continue;
        }
        match managed_block_state(&content) {
            ManagedBlockState::Valid { begin, .. } if top_level_exit_precedes(&content, begin) => {
                println!(
                    "portool: doctor: hook {name}: has a top-level exit/exec before portool's \
                     block; the block may never run (re-run 'portool init' to move it)"
                );
                findings += 1;
            }
            ManagedBlockState::Malformed => {
                println!(
                    "portool: doctor: hook {name}: managed block is malformed; fix it by hand"
                );
                findings += 1;
            }
            _ => {}
        }
        match embedded_bin_path(&content) {
            Some(bin) if !Path::new(&bin).exists() => {
                println!(
                    "portool: doctor: hook {name}: embedded portool path {} no longer \
                     exists; it falls back to PATH lookup, which GUI git clients may not \
                     have (re-run 'portool init')",
                    crate::display::text(&bin)
                );
                findings += 1;
            }
            None => {
                println!(
                    "portool: doctor: hook {name}: uses PATH lookup only; GUI git clients \
                     may not find portool (re-run 'portool init' to embed the absolute path)"
                );
                findings += 1;
            }
            Some(_) => {}
        }
    }
    findings
}

/// True when a line starting at column 0 (i.e. not indented -- so not inside
/// an `if`/`case`/function body) and consisting of `exit`, `exit N`, `exec`,
/// or `exec ...` appears among `content`'s first `before_line` lines. Used to
/// flag a managed block that portool <= 0.8 appended at EOF, after a
/// top-level `exit 0` git will never reach past.
fn top_level_exit_precedes(content: &str, before_line: usize) -> bool {
    content.lines().take(before_line).any(|line| {
        if line.starts_with(char::is_whitespace) {
            return false;
        }
        let t = line.trim_end();
        t == "exit" || t.starts_with("exit ") || t == "exec" || t.starts_with("exec ")
    })
}

/// Extracts the absolute path embedded as `PORTOOL_BIN="<path>"` in a hook
/// script installed by v0.6.0+ `init`, if present.
fn embedded_bin_path(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        line.trim()
            .strip_prefix("PORTOOL_BIN=\"")
            .and_then(|rest| rest.strip_suffix('"'))
            .map(str::to_string)
    })
}

/// The --repair path for a corrupt ledger. Restore-first: a valid
/// `registry.json.bak` brings back *every* project (external review P0-1);
/// only the explicit --abandon-other-projects flag falls back to the
/// destructive start-from-empty rebuild.
///
/// Returns `(registry, restored_from_backup)`: the second field is `true`
/// only on the restore-from-backup path, so `run` can emit the P0-2 recovery
/// advisory afterward.
fn repair_corrupt(
    registry_path: &Path,
    reason: &str,
    abandon_other_projects: bool,
    config: &Config,
) -> Result<(Registry, bool)> {
    if abandon_other_projects {
        let moved_to = store::move_aside(registry_path)?;
        eprintln!(
            "portool: doctor: {} is corrupt ({reason}); moved aside to {} and starting \
             empty (--abandon-other-projects)",
            crate::display::path(registry_path),
            crate::display::path(&moved_to)
        );
        return Ok((Registry::empty(config.range), false));
    }

    match store::load(&store::backup_path(registry_path)) {
        store::LedgerLoad::Loaded(mut backup) => {
            // Copy (not rename) the corrupt file aside, then let save()'s
            // atomic rename overwrite it in one step: registry.json is
            // never missing at any instant. If the save fails here, the
            // corrupt original is still in place, so no later command can
            // mistake the state for a fresh install and refresh the backup
            // with a near-empty ledger.
            let copied_to = store::copy_aside(registry_path)?;
            store::save(registry_path, &mut backup)?;
            eprintln!(
                "portool: doctor: {} was corrupt ({reason}); copied aside to {} and \
                 restored the last good backup (all projects kept)",
                crate::display::path(registry_path),
                crate::display::path(&copied_to)
            );
            Ok((backup, true))
        }
        _ => Err(Error::General(format!(
            "{} is corrupt ({reason}) and no valid backup exists; a plain repair would \
             abandon every other project's allocations. Re-run with 'portool doctor \
             --repair --abandon-other-projects' if you accept that",
            registry_path.display()
        ))),
    }
}

/// The highest ledger `sequence` recorded across the `.env.portool` files of
/// the worktrees this ledger tracks for `common_dir_key` (mirrors the set
/// `sync`'s quarantine scans), or `None` when none record one. Used to
/// advance a restored ledger past its own worktrees' envs so their newer
/// sequence can never permanently quarantine `sync`.
fn max_project_env_sequence(registry: &Registry, common_dir_key: &str) -> Option<u64> {
    let project = registry.find_project(common_dir_key)?;
    let mut max = None;
    for path in project.worktrees.keys() {
        if let Some(seq) = crate::envfile::read_sequence_from_env(Path::new(path)) {
            max = Some(max.map_or(seq, |m: u64| m.max(seq)));
        }
    }
    max
}

/// Converts a live worktree's path into its ledger key, or `None` when the
/// path is not valid UTF-8. Ledger keys are JSON strings; a lossy
/// conversion (`to_string_lossy`) could collide two distinct non-UTF-8
/// paths onto the same key, reintroducing the risk `GitCtx::discover`
/// deliberately fails closed on (external review P2 #7) -- so a non-UTF-8
/// worktree path is skipped here rather than re-keyed.
fn utf8_worktree_key(wt: &Path) -> Option<String> {
    wt.to_str().map(str::to_owned)
}

/// The same per-block invariants [`Registry::validate`] enforces, applied
/// to a single candidate before it is imported: ordered, and no port 0.
fn validate_block(block: (u16, u16)) -> std::result::Result<(), &'static str> {
    if block.0 > block.1 {
        return Err("is reversed");
    }
    if block.0 == 0 {
        return Err("includes port 0");
    }
    Ok(())
}

fn now_local() -> DateTime<FixedOffset> {
    Local::now().trunc_subsecs(0).fixed_offset()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_block_rejects_zero_and_reversed() {
        assert!(validate_block((0, 0)).is_err());
        assert!(validate_block((0, 4)).is_err());
        assert!(validate_block((4000, 3999)).is_err());
        assert!(validate_block((3000, 3004)).is_ok());
        assert!(validate_block((3000, 3000)).is_ok());
    }

    #[test]
    fn utf8_worktree_key_rejects_non_utf8_paths() {
        // macOS's APFS enforces UTF-8 filenames, so this path can never be
        // produced by a real worktree on this platform -- exercised directly
        // via OsStr::from_bytes per the review-fix brief (external review
        // P2 #7).
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let bad = Path::new(OsStr::from_bytes(b"/tmp/wt-\xff\xfe"));
        assert_eq!(utf8_worktree_key(bad), None);
    }

    #[test]
    fn utf8_worktree_key_accepts_utf8_paths() {
        assert_eq!(
            utf8_worktree_key(Path::new("/tmp/wt")),
            Some("/tmp/wt".to_string())
        );
    }

    #[test]
    fn detects_top_level_exit_before_block() {
        let content = "#!/bin/sh\nexit 0\n# >>> portool >>>\nB\n# <<< portool <<<\n";
        assert!(top_level_exit_precedes(content, 2));
        let indented =
            "#!/bin/sh\nif x; then\n  exit 1\nfi\n# >>> portool >>>\nB\n# <<< portool <<<\n";
        assert!(
            !top_level_exit_precedes(indented, 4),
            "indented exit is not top-level"
        );
        let exec_line =
            "#!/bin/sh\nexec other-hook \"$@\"\n# >>> portool >>>\nB\n# <<< portool <<<\n";
        assert!(top_level_exit_precedes(exec_line, 2));
    }
}
