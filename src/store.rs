//! Ledger (`registry.json`) I/O: fail-closed loading and atomic (temp-file
//! + rename) saving (spec §5, §7).
//!
//! Loading never mutates the ledger. A corrupt or unsupported-version file
//! is reported as such and left exactly where it is; the only code path
//! that moves a bad ledger aside is `portool doctor --repair`, via
//! [`move_aside`].

use crate::error::{Error, Result};
use crate::fault;
use crate::registry::Registry;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The outcome of [`load`]: every distinct on-disk state a caller may need
/// to react to. `load` itself never prints, renames, or writes anything.
#[derive(Debug)]
pub enum LedgerLoad {
    /// No ledger file exists yet.
    Missing,
    /// The ledger parsed, validated, and (if it was an older schema)
    /// migrated in memory.
    Loaded(Registry),
    /// The file exists but is unparseable or violates a semantic invariant.
    /// The file has been left in place.
    Corrupt { reason: String },
    /// The file parsed far enough to reveal a schema version this build
    /// does not understand -- almost certainly written by a *newer*
    /// portool. Deliberately distinct from [`LedgerLoad::Corrupt`]: the
    /// right fix is upgrading portool, never "repairing" the file.
    UnsupportedVersion { found: u32, supported: u32 },
    /// The file exists but could not be read for a reason other than "not
    /// found" (permissions, EIO, path is a directory, ...). The ledger may
    /// well still be intact on disk, so callers must never overwrite it.
    ReadError { reason: String },
}

/// Loads the registry at `path` without ever touching the file.
pub fn load(path: &Path) -> LedgerLoad {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return LedgerLoad::Missing,
        Err(err) => {
            return LedgerLoad::ReadError {
                reason: err.to_string(),
            }
        }
    };

    // A ledger is corrupt if it fails to parse *or* parses but violates a
    // semantic invariant (batch B #9): being valid JSON is necessary but not
    // sufficient to trust it. `from_json` also migrates an older (v1) schema
    // to the current one in memory (batch C). An unrecognized schema version
    // is kept distinct from corruption throughout.
    let result = Registry::from_json(&contents).and_then(|registry| {
        registry.validate()?;
        Ok(registry)
    });
    match result {
        Ok(registry) => LedgerLoad::Loaded(registry),
        Err(Error::UnsupportedRegistryVersion { found, supported }) => {
            LedgerLoad::UnsupportedVersion { found, supported }
        }
        Err(err) => LedgerLoad::Corrupt {
            reason: err.to_string(),
        },
    }
}

/// Loads the registry for a command that must be able to trust it
/// (fail-closed): `Ok(None)` when no ledger exists yet, `Ok(Some(_))` for a
/// valid one, and a hard error for everything else -- a corrupt,
/// unsupported-version, or unreadable ledger is never silently replaced
/// with an empty one (external review P1 #1).
pub fn load_strict(path: &Path) -> Result<Option<Registry>> {
    match load(path) {
        LedgerLoad::Missing => Ok(None),
        LedgerLoad::Loaded(registry) => Ok(Some(registry)),
        LedgerLoad::Corrupt { reason } => Err(Error::General(format!(
            "{} is corrupt ({reason}); refusing to touch it -- run 'portool doctor --repair' \
             to restore it from backup and rebuild this project's entries",
            path.display()
        ))),
        LedgerLoad::UnsupportedVersion { found, supported } => Err(Error::General(format!(
            "{} uses registry schema version {found}, but this build understands version \
             {supported}; upgrade portool instead of modifying the ledger",
            path.display()
        ))),
        LedgerLoad::ReadError { reason } => Err(Error::General(format!(
            "failed to read {} ({reason}); aborting without writing to avoid clobbering an \
             intact ledger",
            path.display()
        ))),
    }
}

/// Renames the ledger aside to `<path>.corrupt-<unix seconds>` and returns
/// the new path. Only `portool doctor --repair` calls this -- an explicit,
/// user-requested reset is the single place a bad ledger may be moved.
pub fn move_aside(path: &Path) -> Result<PathBuf> {
    let corrupt_path = corrupt_sibling_path(path);
    fs::rename(path, &corrupt_path).map_err(|err| {
        Error::General(format!(
            "failed to move {} aside to {}: {err}",
            path.display(),
            corrupt_path.display()
        ))
    })?;
    Ok(corrupt_path)
}

/// Copies the (bad) ledger to a `.corrupt-<unix seconds>` sibling without
/// removing the original, and returns the sibling's path. The restore path
/// of `doctor --repair` uses this instead of [`move_aside`]: if the
/// subsequent save of the restored backup fails, `registry.json` still
/// holds the (corrupt) original -- it is never missing, so a later command
/// can never mistake the state for a fresh install and clobber the backup.
pub fn copy_aside(path: &Path) -> Result<PathBuf> {
    let corrupt_path = corrupt_sibling_path(path);
    fs::copy(path, &corrupt_path).map_err(|err| {
        Error::General(format!(
            "failed to copy {} aside to {}: {err}",
            path.display(),
            corrupt_path.display()
        ))
    })?;
    Ok(corrupt_path)
}

/// Saves `registry` to `path` atomically and durably, then refreshes
/// `<path>.bak` (see [`backup_path`]) the same way: a backup failure is a
/// warning on stderr, never a command failure. The backup is written via
/// temp-file + rename too (external review 3rd round P1-3) -- a crash mid-
/// backup can no longer leave a partially overwritten `.bak`.
pub fn save(path: &Path, registry: &Registry) -> Result<()> {
    let json = serde_json::to_string_pretty(registry)?;
    write_atomic_impl(path, json.as_bytes(), None)?;
    fault::point("after_registry_write");

    let bak = backup_path(path);
    if let Err(err) = write_atomic_impl(&bak, json.as_bytes(), Some("during_backup")) {
        eprintln!(
            "portool: warning: failed to update backup {}: {err}",
            bak.display()
        );
    }
    Ok(())
}

/// Writes `contents` to `path` atomically and durably: temp file in the
/// same directory, `sync_all`, rename into place, then a best-effort fsync
/// of the parent directory so the rename itself survives power loss. The
/// parent directory is created if necessary.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    write_atomic_impl(path, contents, None)
}

fn write_atomic_impl(
    path: &Path,
    contents: &[u8],
    fault_before_rename: Option<&str>,
) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::General(format!("{} has no parent directory", path.display())))?;
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(contents)?;
    tmp.as_file().sync_all()?;
    if let Some(name) = fault_before_rename {
        fault::point(name);
    }
    tmp.persist(path).map_err(|e| Error::from(e.error))?;
    fsync_dir(dir);
    Ok(())
}

/// Best-effort directory fsync after a rename. Failure is a stderr warning,
/// not an error: the rename already succeeded, and some filesystems refuse
/// directory fsyncs.
fn fsync_dir(dir: &Path) {
    if let Ok(handle) = fs::File::open(dir) {
        if let Err(err) = handle.sync_all() {
            eprintln!(
                "portool: warning: failed to fsync directory {}: {err}",
                dir.display()
            );
        }
    }
}

/// `<path>.bak`: a byte-exact copy of the last successfully saved ledger,
/// refreshed by every [`save`]. `doctor --repair` restores from it so that
/// a corrupt ledger costs at most one save, not every other project's
/// allocations (external review P0-1).
pub fn backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "registry.json".to_string());
    path.with_file_name(format!("{file_name}.bak"))
}

/// True when the ledger exists but its `.bak` sibling is missing or does
/// not match byte-for-byte -- i.e. the last backup refresh failed and a
/// `doctor --repair` would restore stale state. Heals on the next save.
pub fn backup_is_stale(path: &Path) -> std::io::Result<bool> {
    let main = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match std::fs::read(backup_path(path)) {
        Ok(bak) => Ok(bak != main),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
    }
}

fn corrupt_sibling_path(path: &Path) -> PathBuf {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "registry.json".to_string());
    path.with_file_name(format!("{file_name}.corrupt-{secs}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ProjectEntry, WorktreeEntry};
    use chrono::{FixedOffset, TimeZone};
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn sample_registry() -> Registry {
        let mut registry = Registry::empty((3000, 9999));
        let tz = FixedOffset::east_opt(9 * 3600).unwrap();
        let now = tz.with_ymd_and_hms(2026, 7, 15, 10, 0, 0).unwrap();

        let mut worktrees = BTreeMap::new();
        worktrees.insert(
            "/home/user/dev/myapp".to_string(),
            WorktreeEntry {
                block: (3000, 3004),
                generation: 1,
                pending_block: None,
                branch: Some("main".to_string()),
                manifest_hash: Some("a1b2c3d4e5f6".to_string()),
                pinned: false,
                label: None,
                allocated_at: now,
                last_seen_at: now,
            },
        );
        registry.projects.insert(
            "/home/user/dev/myapp/.git".to_string(),
            ProjectEntry {
                name: "myapp".to_string(),
                worktrees,
            },
        );
        registry
    }

    fn dir_entries(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn load_missing_file_is_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");

        assert!(matches!(load(&path), LedgerLoad::Missing));
        assert_eq!(load_strict(&path).unwrap(), None);
    }

    #[test]
    fn load_non_not_found_read_error_is_reported_without_touching_the_file() {
        let tmp = TempDir::new().unwrap();
        // A directory in place of registry.json is a deterministic
        // non-NotFound read error on every Unix platform.
        let path = tmp.path().join("registry.json");
        fs::create_dir(&path).unwrap();

        assert!(matches!(load(&path), LedgerLoad::ReadError { .. }));
        assert!(
            load_strict(&path).is_err(),
            "a non-NotFound read failure must abort fail-closed callers"
        );
        // A read error must not rename or remove anything: the ledger may
        // still be intact on disk.
        assert!(path.is_dir(), "the unreadable path must be left in place");
        assert_eq!(dir_entries(tmp.path()), vec!["registry.json".to_string()]);
    }

    #[test]
    fn load_corrupt_json_is_reported_and_left_in_place() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        let original_contents = b"{ this is not valid json".to_vec();
        fs::write(&path, &original_contents).unwrap();

        assert!(matches!(load(&path), LedgerLoad::Corrupt { .. }));
        assert!(
            load_strict(&path).is_err(),
            "a corrupt ledger must be a hard error, never an empty registry"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            original_contents,
            "loading must never modify the corrupt file"
        );
        assert_eq!(
            dir_entries(tmp.path()),
            vec!["registry.json".to_string()],
            "no corrupt-<timestamp> sibling may be created by load"
        );
    }

    #[test]
    fn load_future_schema_is_unsupported_version_not_corrupt() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        fs::write(
            &path,
            br#"{"version":999,"range":[3000,9999],"projects":{},"reservations":[]}"#,
        )
        .unwrap();

        match load(&path) {
            LedgerLoad::UnsupportedVersion { found, supported } => {
                assert_eq!(found, 999);
                assert_eq!(supported, Registry::CURRENT_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
        let err = load_strict(&path).unwrap_err();
        assert!(
            err.to_string().contains("upgrade portool"),
            "the error must steer toward upgrading, got: {err}"
        );
        assert!(path.exists(), "the newer-version ledger must be untouched");
    }

    #[test]
    fn load_semantically_invalid_ledger_is_corrupt() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        // Valid JSON, current version, but a reversed block -> validation
        // fails -> corrupt.
        fs::write(
            &path,
            br#"{"version":2,"range":[3000,9999],"projects":{},"reservations":[{"block":[4000,3999],"label":null,"pinned":false}]}"#,
        )
        .unwrap();

        assert!(matches!(load(&path), LedgerLoad::Corrupt { .. }));
        assert!(path.exists(), "the file must be left in place");
    }

    #[test]
    fn move_aside_renames_to_a_corrupt_sibling() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        fs::write(&path, b"{ this is not valid json").unwrap();

        let moved_to = move_aside(&path).unwrap();

        assert!(!path.exists(), "the original file must be gone");
        assert!(moved_to.exists());
        assert!(moved_to
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("registry.json.corrupt-"));
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        let registry = sample_registry();

        save(&path, &registry).unwrap();

        assert_eq!(load_strict(&path).unwrap(), Some(registry));
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("dir").join("registry.json");

        save(&path, &Registry::empty((3000, 9999))).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn save_leaves_no_temp_file_residue() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");

        save(&path, &Registry::empty((3000, 9999))).unwrap();

        let mut entries = dir_entries(tmp.path());
        entries.sort();
        assert_eq!(
            entries,
            vec!["registry.json".to_string(), "registry.json.bak".to_string()]
        );
    }

    #[test]
    fn save_overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");

        save(&path, &Registry::empty((3000, 9999))).unwrap();
        save(&path, &sample_registry()).unwrap();

        assert_eq!(load_strict(&path).unwrap(), Some(sample_registry()));
    }

    #[test]
    fn copy_aside_keeps_the_original_and_creates_a_corrupt_sibling() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        let contents = b"{ this is not valid json".to_vec();
        fs::write(&path, &contents).unwrap();

        let copied_to = copy_aside(&path).unwrap();

        assert!(
            path.exists(),
            "the original must never go missing during a copy-aside"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            contents,
            "the original must be byte-identical"
        );
        assert!(copied_to.exists());
        assert_eq!(
            fs::read(&copied_to).unwrap(),
            contents,
            "the sibling must be a byte-exact copy"
        );
        assert!(copied_to
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("registry.json.corrupt-"));
    }

    #[test]
    fn save_writes_a_backup_sibling() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        save(&path, &sample_registry()).unwrap();

        let bak = backup_path(&path);
        assert!(bak.exists());
        assert_eq!(
            fs::read(&path).unwrap(),
            fs::read(&bak).unwrap(),
            "backup must be a byte-exact copy of the last successful save"
        );
    }

    #[test]
    fn backup_lags_one_save_behind_on_the_next_write() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        save(&path, &Registry::empty((3000, 9999))).unwrap();
        save(&path, &sample_registry()).unwrap();
        // After the second save the backup equals the *second* saved state
        // (copy happens after persist), and load_strict on it succeeds.
        assert_eq!(
            load_strict(&backup_path(&path)).unwrap(),
            Some(sample_registry())
        );
    }

    #[test]
    fn write_atomic_writes_contents_and_leaves_no_temp_residue() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("file.txt");

        write_atomic(&path, b"hello").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"hello");
        assert_eq!(
            dir_entries(path.parent().unwrap()),
            vec!["file.txt".to_string()],
            "no temp residue"
        );
    }

    #[test]
    fn backup_is_stale_false_when_main_ledger_is_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");

        assert!(!backup_is_stale(&path).unwrap());
    }

    #[test]
    fn backup_is_stale_true_when_bak_is_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        fs::write(&path, b"{}").unwrap();

        assert!(backup_is_stale(&path).unwrap());
    }

    #[test]
    fn backup_is_stale_true_when_bak_differs_and_false_once_fresh() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        save(&path, &sample_registry()).unwrap();
        assert!(
            !backup_is_stale(&path).unwrap(),
            "a freshly saved backup must not be stale"
        );

        fs::write(backup_path(&path), b"stale contents").unwrap();
        assert!(backup_is_stale(&path).unwrap());
    }

    #[test]
    fn save_backup_is_atomic_not_a_plain_copy() {
        // After save, .bak must be byte-identical AND the directory must hold
        // exactly registry.json + registry.json.bak (no .bak.tmp residue).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        save(&path, &sample_registry()).unwrap();

        let mut entries = dir_entries(tmp.path());
        entries.sort();
        assert_eq!(
            entries,
            vec!["registry.json".to_string(), "registry.json.bak".to_string()]
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            fs::read(backup_path(&path)).unwrap()
        );
    }
}
