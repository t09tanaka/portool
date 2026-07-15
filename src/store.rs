//! Ledger (`registry.json`) I/O: loading with corruption recovery, and
//! atomic (temp-file + rename) saving (spec §5, §7).

use crate::config::Config;
use crate::error::{Error, Result};
use crate::registry::Registry;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The outcome of [`load`].
#[derive(Debug)]
pub struct LoadResult {
    /// The loaded (or freshly-empty) registry.
    pub registry: Registry,
    /// Whether the on-disk file existed but failed to parse. Callers on
    /// the sync fast path use this to force a fall-through to the slow
    /// path, since a corrupt ledger can never satisfy the fast-path
    /// equality checks.
    pub corrupt: bool,
}

/// Loads the registry at `path`.
///
/// - If the file does not exist, returns an empty registry (spec §5: "台帳が
///   存在しない...場合は空台帳として再生成").
/// - If the file exists but fails to parse as JSON, it is renamed aside to
///   `<path>.corrupt-<unix seconds>`, a warning is printed to stderr, and an
///   empty registry is returned. If the rename itself fails, a warning is
///   printed and loading still falls back to an empty registry (the
///   original corrupt file is left in place, to be retried next call).
///
/// The empty registry's `range` field is informational only; callers that
/// detect a freshly-created ledger (`corrupt` or a missing file) and hold a
/// live [`Config`] should overwrite `registry.range` with the config's pool
/// before the first save, per the frozen decision that `range` reflects the
/// pool at ledger-creation time.
pub fn load(path: &Path) -> LoadResult {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(_) => {
            return LoadResult {
                registry: Registry::empty(Config::default().range),
                corrupt: false,
            };
        }
    };

    match serde_json::from_str::<Registry>(&contents) {
        Ok(registry) => LoadResult {
            registry,
            corrupt: false,
        },
        Err(parse_err) => {
            let corrupt_path = corrupt_sibling_path(path);
            match fs::rename(path, &corrupt_path) {
                Ok(()) => eprintln!(
                    "portool: warning: {} is corrupt ({parse_err}); moved aside to {}",
                    path.display(),
                    corrupt_path.display()
                ),
                Err(rename_err) => eprintln!(
                    "portool: warning: {} is corrupt ({parse_err}); failed to move it aside: {rename_err}",
                    path.display()
                ),
            }
            LoadResult {
                registry: Registry::empty(Config::default().range),
                corrupt: true,
            }
        }
    }
}

/// Saves `registry` to `path` atomically: writes pretty-printed JSON to a
/// temp file in the same directory, then renames it into place. The parent
/// directory is created if necessary.
pub fn save(path: &Path, registry: &Registry) -> Result<()> {
    let dir = path.parent().ok_or_else(|| {
        Error::General(format!(
            "registry path {} has no parent directory",
            path.display()
        ))
    })?;
    fs::create_dir_all(dir)?;

    let json = serde_json::to_string_pretty(registry)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(json.as_bytes())?;
    tmp.persist(path).map_err(|e| Error::from(e.error))?;
    Ok(())
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
            "/home/takuto/dev/esimdb".to_string(),
            WorktreeEntry {
                block: (3000, 3004),
                branch: Some("main".to_string()),
                manifest_hash: Some("a1b2c3d4e5f6".to_string()),
                pinned: false,
                label: None,
                allocated_at: now,
                last_seen_at: now,
            },
        );
        registry.projects.insert(
            "/home/takuto/dev/esimdb/.git".to_string(),
            ProjectEntry {
                name: "esimdb".to_string(),
                subranges: vec![(3000, 3499)],
                worktrees,
            },
        );
        registry
    }

    #[test]
    fn load_missing_file_returns_empty_registry() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");

        let result = load(&path);

        assert!(!result.corrupt);
        assert!(result.registry.projects.is_empty());
        assert!(result.registry.reservations.is_empty());
    }

    #[test]
    fn load_corrupt_json_moves_it_aside_and_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        fs::write(&path, b"{ this is not valid json").unwrap();

        let result = load(&path);

        assert!(result.corrupt);
        assert!(result.registry.projects.is_empty());
        assert!(
            !path.exists(),
            "original corrupt file should be moved aside"
        );

        let corrupt_files: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("registry.json.corrupt-"))
            .collect();
        assert_eq!(corrupt_files.len(), 1);
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        let registry = sample_registry();

        save(&path, &registry).unwrap();
        let result = load(&path);

        assert!(!result.corrupt);
        assert_eq!(result.registry, registry);
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

        let entries: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["registry.json".to_string()]);
    }

    #[test]
    fn save_overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");

        save(&path, &Registry::empty((3000, 9999))).unwrap();
        save(&path, &sample_registry()).unwrap();

        let result = load(&path);
        assert_eq!(result.registry, sample_registry());
    }
}
