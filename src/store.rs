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
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
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

// --- symlink-safe, fd-relative writes under a repository boundary (P0-1) ---
//
// Every hook write goes through these instead of `fs::create_dir_all` +
// `NamedTempFile`, so a symlink planted anywhere along the path (a `.husky`
// symlink, a symlinked hooks dir, or the hook file itself) can never redirect
// a write outside the repository. The directory walk is fd-relative
// (`openat` with `O_NOFOLLOW | O_DIRECTORY`), which closes the TOCTOU window a
// canonicalize-then-write check would leave open: the same descriptor that was
// verified is the one written through.

/// `openat(dirfd, name, O_RDONLY|O_NOFOLLOW|O_DIRECTORY)` -- or, when `dirfd`
/// is `None`, `open(name, ...)` for the boundary itself. Fails on a symlink
/// component (`ELOOP`) or a non-directory.
fn open_dir_nofollow(name: &Path, dirfd: Option<RawFd>) -> std::io::Result<fs::File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(name.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC;
    let fd = match dirfd {
        Some(d) => unsafe { libc::openat(d, c.as_ptr(), flags) },
        None => unsafe { libc::open(c.as_ptr(), flags) },
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd is a freshly opened, owned descriptor.
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

/// The `Normal` path components of `target` relative to `boundary`, with the
/// leaf split off. Errors if `target` is not under `boundary` or any relative
/// component is `..`/`.`/root (an escape attempt).
fn repo_relative_components(
    boundary: &Path,
    target: &Path,
) -> Result<(Vec<std::ffi::OsString>, std::ffi::OsString)> {
    let rel = target.strip_prefix(boundary).map_err(|_| {
        Error::General(format!(
            "refusing to write {}: it is not under {}",
            target.display(),
            boundary.display()
        ))
    })?;
    let mut components: Vec<std::ffi::OsString> = Vec::new();
    for c in rel.components() {
        match c {
            std::path::Component::Normal(name) => components.push(name.to_os_string()),
            _ => {
                return Err(Error::General(format!(
                    "refusing to write {}: a path component escapes {}",
                    target.display(),
                    boundary.display()
                )))
            }
        }
    }
    let leaf = components.pop().ok_or_else(|| {
        Error::General(format!(
            "refusing to write {}: empty path",
            target.display()
        ))
    })?;
    Ok((components, leaf))
}

/// Opens the parent directory of `target` as a verified fd, plus the leaf
/// name, walking every component below `boundary` with `O_NOFOLLOW` so no
/// symlink is ever followed. Fails if a parent is missing (create it first
/// with [`create_dirs_repo_relative`]) or is a symlink/non-directory.
fn open_parent_dir_fd(boundary: &Path, target: &Path) -> Result<(fs::File, std::ffi::OsString)> {
    let (components, leaf) = repo_relative_components(boundary, target)?;
    let mut dir = open_dir_nofollow(boundary, None).map_err(|e| {
        Error::General(format!(
            "refusing to write under {}: boundary is not a real directory ({e})",
            boundary.display()
        ))
    })?;
    for name in components {
        dir = open_dir_nofollow(Path::new(&name), Some(dir.as_raw_fd())).map_err(|e| {
            Error::General(format!(
                "refusing to write under {}: intermediate '{}' is not a real directory ({e})",
                boundary.display(),
                Path::new(&name).display()
            ))
        })?;
    }
    Ok((dir, leaf))
}

/// Ensures every intermediate directory of `target` exists under `boundary`,
/// creating missing ones with `mkdirat` (mode 0755). Refuses to descend
/// through a symlink or non-directory.
pub fn create_dirs_repo_relative(boundary: &Path, target: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let (names, _leaf) = repo_relative_components(boundary, target)?;
    let mut dir = open_dir_nofollow(boundary, None).map_err(|e| {
        Error::General(format!(
            "refusing to create dirs under {}: boundary is not a real directory ({e})",
            boundary.display()
        ))
    })?;
    for name in names {
        let c = CString::new(name.as_bytes())
            .map_err(|_| Error::General("directory name contains NUL".into()))?;
        match open_dir_nofollow(Path::new(&name), Some(dir.as_raw_fd())) {
            Ok(next) => dir = next,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let rc = unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o755) };
                if rc < 0 {
                    return Err(Error::from(std::io::Error::last_os_error()));
                }
                dir = open_dir_nofollow(Path::new(&name), Some(dir.as_raw_fd())).map_err(|e| {
                    Error::General(format!(
                        "refusing to create dirs under {}: '{}' is not a real directory ({e})",
                        boundary.display(),
                        Path::new(&name).display()
                    ))
                })?;
            }
            Err(e) => {
                return Err(Error::General(format!(
                    "refusing to create dirs under {}: '{}' is not a real directory ({e})",
                    boundary.display(),
                    Path::new(&name).display()
                )))
            }
        }
    }
    Ok(())
}

/// Atomically writes `contents` to `target` with permission `mode`, refusing
/// if any component below `boundary` is a symlink or escapes it. Temp file +
/// `renameat`, both relative to the verified parent-directory fd, so the
/// verified directory is exactly the one written into (TOCTOU-safe). Returns
/// the final path written.
pub fn atomic_write_within(
    boundary: &Path,
    target: &Path,
    contents: &[u8],
    mode: u32,
) -> Result<PathBuf> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let (dir, leaf) = open_parent_dir_fd(boundary, target)?;
    let dirfd = dir.as_raw_fd();

    let tmp_name = format!(
        ".portool-tmp-{}-{}",
        std::process::id(),
        leaf.to_string_lossy()
    );
    let tmp_c = CString::new(tmp_name.as_bytes())
        .map_err(|_| Error::General("temp name contains NUL".into()))?;
    // O_EXCL: a pre-existing temp of this exact name is an anomaly, not
    // something to silently truncate. O_NOFOLLOW: never write through a
    // symlink even at the leaf temp.
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(dirfd, tmp_c.as_ptr(), flags, mode as libc::c_uint) };
    if fd < 0 {
        return Err(Error::from(std::io::Error::last_os_error()));
    }
    // SAFETY: fd is a freshly opened, owned descriptor.
    let mut tmp = unsafe { fs::File::from_raw_fd(fd) };
    let write_result = (|| -> std::io::Result<()> {
        tmp.write_all(contents)?;
        tmp.sync_all()?;
        Ok(())
    })();
    drop(tmp);
    if let Err(e) = write_result {
        unsafe { libc::unlinkat(dirfd, tmp_c.as_ptr(), 0) };
        return Err(Error::from(e));
    }
    // openat with O_CREAT|O_EXCL honors `mode` only subject to umask; force
    // the exact bits so a restrictive umask can't drop the execute bit a hook
    // needs.
    let rc = unsafe { libc::fchmodat(dirfd, tmp_c.as_ptr(), mode as libc::mode_t, 0) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        unsafe { libc::unlinkat(dirfd, tmp_c.as_ptr(), 0) };
        return Err(Error::from(e));
    }
    let leaf_c = CString::new(leaf.as_bytes())
        .map_err(|_| Error::General("leaf name contains NUL".into()))?;
    let rc = unsafe { libc::renameat(dirfd, tmp_c.as_ptr(), dirfd, leaf_c.as_ptr()) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        unsafe { libc::unlinkat(dirfd, tmp_c.as_ptr(), 0) };
        return Err(Error::from(e));
    }
    // Best-effort durability of the rename.
    let _ = dir.sync_all();
    Ok(boundary.join(
        target
            .strip_prefix(boundary)
            .expect("checked in repo_relative_components"),
    ))
}

/// Sets `target`'s permission bits to `mode`, refusing if any component below
/// `boundary` is a symlink or escapes it. Uses `fchmodat` relative to the
/// verified parent-directory fd, so it can never chmod a file outside the
/// repository even if a component was swapped for a symlink.
pub fn chmod_within(boundary: &Path, target: &Path, mode: u32) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let (dir, leaf) = open_parent_dir_fd(boundary, target)?;
    let leaf_c = CString::new(leaf.as_bytes())
        .map_err(|_| Error::General("leaf name contains NUL".into()))?;
    // AT_SYMLINK_NOFOLLOW so a leaf that became a symlink is never chmod'd
    // through; portool's caller has already rejected symlink leaves, this is
    // belt-and-suspenders. (Linux may reject NOFOLLOW-on-non-symlink with
    // ENOTSUP; fall back to a plain fchmodat, still fd-relative so it stays
    // inside the verified directory.)
    let rc = unsafe {
        libc::fchmodat(
            dir.as_raw_fd(),
            leaf_c.as_ptr(),
            mode as libc::mode_t,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    let enotsup =
        err.raw_os_error() == Some(libc::ENOTSUP) || err.raw_os_error() == Some(libc::EOPNOTSUPP);
    if enotsup {
        let rc =
            unsafe { libc::fchmodat(dir.as_raw_fd(), leaf_c.as_ptr(), mode as libc::mode_t, 0) };
        if rc == 0 {
            return Ok(());
        }
        return Err(Error::from(std::io::Error::last_os_error()));
    }
    Err(Error::from(err))
}

/// Removes `target`, refusing if any component below `boundary` is a symlink
/// or escapes it. `unlinkat` relative to the verified parent-directory fd.
pub fn unlink_within(boundary: &Path, target: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let (dir, leaf) = open_parent_dir_fd(boundary, target)?;
    let leaf_c = CString::new(leaf.as_bytes())
        .map_err(|_| Error::General("leaf name contains NUL".into()))?;
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), leaf_c.as_ptr(), 0) };
    if rc < 0 {
        return Err(Error::from(std::io::Error::last_os_error()));
    }
    Ok(())
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

/// Renames the ledger aside to `<path>.corrupt-<nanos>-<pid>` and returns
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

/// Copies the (bad) ledger to a `.corrupt-<nanos>-<pid>` sibling without
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
pub fn save(path: &Path, registry: &mut Registry) -> Result<()> {
    // Bump the monotonic ledger sequence on every save (external review v0.10
    // P0-2). `save` is always called under the ledger lock, so this is the
    // single serialization point for the counter. `saturating_add` keeps a
    // (practically unreachable) overflow from wrapping the counter back.
    registry.sequence = registry.sequence.saturating_add(1);
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

/// The recovery health of the ledger's `.bak` sibling, compared by parsed
/// `sequence` (external review v0.10 P0-2). `Behind` means restoring the
/// backup would roll the ledger *back* -- a degraded state `check` surfaces
/// as a non-zero exit, because a `doctor --repair` would currently lose
/// allocations newer than the backup.
#[derive(Debug, PartialEq, Eq)]
pub enum BackupStatus {
    /// The backup is at least as new as the ledger (or there is no ledger yet).
    Fresh,
    /// The ledger exists but has no `.bak` sibling.
    Missing,
    /// The backup's sequence is older than the ledger's.
    Behind { main_seq: u64, bak_seq: u64 },
    /// The ledger or its backup is unreadable/corrupt; recovery is unsafe.
    Corrupt,
}

/// Compares the ledger and its `.bak` by parsed `sequence`.
pub fn backup_status(path: &Path) -> BackupStatus {
    let main = match load(path) {
        LedgerLoad::Loaded(registry) => registry,
        LedgerLoad::Missing => return BackupStatus::Fresh, // nothing to back up yet
        _ => return BackupStatus::Corrupt,
    };
    match load(&backup_path(path)) {
        LedgerLoad::Loaded(bak) => {
            if bak.sequence < main.sequence {
                BackupStatus::Behind {
                    main_seq: main.sequence,
                    bak_seq: bak.sequence,
                }
            } else {
                BackupStatus::Fresh
            }
        }
        LedgerLoad::Missing => BackupStatus::Missing,
        _ => BackupStatus::Corrupt,
    }
}

fn corrupt_sibling_path(path: &Path) -> PathBuf {
    // Nanosecond resolution + pid keeps two repairs of the same ledger in
    // the same second from colliding and silently overwriting each other's
    // forensic aside file (external review P3): second-resolution alone
    // isn't unique enough when `doctor --repair` can run more than once per
    // second, e.g. in tests or scripted repair loops.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let pid = std::process::id();
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "registry.json".to_string());
    path.with_file_name(format!("{file_name}.corrupt-{}-{pid}", now.as_nanos()))
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
    fn corrupt_sibling_paths_are_unique_within_a_second() {
        // Two repairs of the same ledger within the same wall-clock second
        // must not collide (external review P3): second-resolution alone
        // isn't unique enough. Calling `corrupt_sibling_path` twice and
        // asserting the results differ would be flaky (nanos could
        // theoretically tie under a coarse clock source), so instead assert
        // the format actually carries the nanos and pid components that
        // make collisions practically impossible.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");

        let sibling = corrupt_sibling_path(&path);
        let name = sibling.file_name().unwrap().to_string_lossy().to_string();
        let suffix = name
            .strip_prefix("registry.json.corrupt-")
            .expect("must keep the .corrupt- prefix");
        let (nanos, pid) = suffix
            .split_once('-')
            .expect("suffix must be `<nanos>-<pid>`");

        assert!(
            nanos.parse::<u128>().is_ok(),
            "nanos component must be numeric: {nanos}"
        );
        assert_eq!(
            pid.parse::<u32>().ok(),
            Some(std::process::id()),
            "pid component must be the current process id"
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        let mut registry = sample_registry();

        save(&path, &mut registry).unwrap();

        // save bumps the in-memory registry's sequence too, so the reloaded
        // ledger matches it exactly.
        assert_eq!(registry.sequence, 1);
        assert_eq!(load_strict(&path).unwrap(), Some(registry));
    }

    #[test]
    fn save_increments_sequence_each_time() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        let mut registry = Registry::empty((3000, 9999));
        save(&path, &mut registry).unwrap();
        assert_eq!(registry.sequence, 1);
        save(&path, &mut registry).unwrap();
        assert_eq!(registry.sequence, 2);
        assert_eq!(load_strict(&path).unwrap().unwrap().sequence, 2);
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("dir").join("registry.json");

        save(&path, &mut Registry::empty((3000, 9999))).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn save_leaves_no_temp_file_residue() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");

        save(&path, &mut Registry::empty((3000, 9999))).unwrap();

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

        save(&path, &mut Registry::empty((3000, 9999))).unwrap();
        let mut sample = sample_registry();
        save(&path, &mut sample).unwrap();

        assert_eq!(load_strict(&path).unwrap(), Some(sample));
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
        save(&path, &mut sample_registry()).unwrap();

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
        save(&path, &mut Registry::empty((3000, 9999))).unwrap();
        let mut sample = sample_registry();
        save(&path, &mut sample).unwrap(); // bumps sample.sequence
                                           // After the second save the backup equals the *second* saved state
                                           // (copy happens after persist), sequence and all.
        assert_eq!(load_strict(&backup_path(&path)).unwrap(), Some(sample));
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
    fn backup_status_fresh_when_main_ledger_is_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        assert_eq!(backup_status(&path), BackupStatus::Fresh);
    }

    #[test]
    fn backup_status_missing_when_bak_absent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        // A valid ledger with no .bak sibling.
        write_atomic(
            &path,
            serde_json::to_string(&Registry::empty((3000, 9999)))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(backup_status(&path), BackupStatus::Missing);
    }

    #[test]
    fn backup_status_fresh_after_save_and_behind_when_bak_sequence_is_older() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        let mut reg = sample_registry();
        save(&path, &mut reg).unwrap(); // ledger seq 1, bak seq 1
        assert_eq!(backup_status(&path), BackupStatus::Fresh);

        // Advance the ledger without refreshing the backup (simulating a
        // failed backup write): the backup's sequence now lags behind.
        save(&path, &mut reg).unwrap(); // ledger seq 2
                                        // Roll the backup back to a stale (seq 1) copy by hand.
        let mut stale = sample_registry();
        stale.sequence = 1;
        write_atomic(
            &backup_path(&path),
            serde_json::to_string(&stale).unwrap().as_bytes(),
        )
        .unwrap();
        match backup_status(&path) {
            BackupStatus::Behind { main_seq, bak_seq } => {
                assert_eq!(main_seq, 2);
                assert_eq!(bak_seq, 1);
            }
            other => panic!("expected Behind, got {other:?}"),
        }
    }

    #[test]
    fn backup_status_corrupt_when_bak_unparseable() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        save(&path, &mut sample_registry()).unwrap();
        fs::write(backup_path(&path), b"{ not json").unwrap();
        assert_eq!(backup_status(&path), BackupStatus::Corrupt);
    }

    #[test]
    fn atomic_write_within_writes_and_sets_mode() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("sub/dir/file");
        create_dirs_repo_relative(tmp.path(), &target).unwrap();
        let written = atomic_write_within(tmp.path(), &target, b"hi", 0o755).unwrap();
        assert_eq!(fs::read(&written).unwrap(), b"hi");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn atomic_write_within_refuses_a_symlinked_intermediate() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        // repo/hooks -> ../outside (escape)
        std::os::unix::fs::symlink(&outside, repo.join("hooks")).unwrap();
        let target = repo.join("hooks/post-checkout");

        let err = atomic_write_within(&repo, &target, b"x", 0o755).unwrap_err();
        assert!(
            err.to_string().contains("not a real directory")
                || err.to_string().contains("refusing"),
            "unexpected: {err}"
        );
        // Nothing leaked into the escape target.
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn atomic_write_within_refuses_a_target_outside_boundary() {
        let tmp = TempDir::new().unwrap();
        let boundary = tmp.path().join("repo");
        fs::create_dir_all(&boundary).unwrap();
        let target = tmp.path().join("elsewhere/file");
        assert!(atomic_write_within(&boundary, &target, b"x", 0o644).is_err());
    }

    #[test]
    fn create_dirs_repo_relative_refuses_symlinked_component() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("link")).unwrap();
        let target = repo.join("link/sub/file");
        assert!(create_dirs_repo_relative(&repo, &target).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn unlink_within_removes_only_inside_boundary() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("f");
        fs::write(&target, b"x").unwrap();
        unlink_within(tmp.path(), &target).unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn save_backup_is_atomic_not_a_plain_copy() {
        // After save, .bak must be byte-identical AND the directory must hold
        // exactly registry.json + registry.json.bak (no .bak.tmp residue).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        save(&path, &mut sample_registry()).unwrap();

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
