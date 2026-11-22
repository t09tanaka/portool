//! Removal of the files portool itself leaves behind: crash temp files and
//! old `registry.json.corrupt-*` copies. The decisions are pure functions
//! (names and ages in, names to delete out); [`sweep_dir`] does the I/O.
//!
//! Only names portool created are ever touched: `.portool-tmp-<pid>-<rest>`
//! anywhere, the pre-0.14 `.tmpXXXXXX` form only in portool's own state
//! directory, and `<ledger>.corrupt-<nanos>-<pid>` next to the ledger.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Prefix of every temp file portool creates for an atomic write.
pub const TMP_PREFIX: &str = ".portool-tmp-";

/// A temp file lives for milliseconds between create and rename; ten minutes
/// leaves room for a stalled `fsync` on a network filesystem and costs
/// nothing, since the owner must also be dead.
pub const TEMP_MIN_AGE: Duration = Duration::from_secs(10 * 60);

/// A corrupt copy is read by a human, right after the `doctor --repair` that
/// printed its path. A week covers a weekend away.
pub const CORRUPT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// The owner pid encoded in `.portool-tmp-<pid>-<rest>`.
pub fn tmp_owner(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(TMP_PREFIX)?;
    let (pid, tail) = rest.split_once('-')?;
    if tail.is_empty() || pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

/// The `.tmpXXXXXX` form older versions created (six alphanumerics).
pub fn is_legacy_tmp(name: &str) -> bool {
    name.strip_prefix(".tmp")
        .map(|r| r.len() == 6 && r.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or(false)
}

/// Whether process `pid` exists. Never signals pid <= 1 or one that does not
/// fit a pid_t: those count as alive (keep the file).
pub fn pid_alive(pid: u32) -> bool {
    if pid <= 1 || pid > i32::MAX as u32 {
        return true;
    }
    // SAFETY: signal 0 only checks existence.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Temp files to delete: a `.portool-tmp-<pid>-*` whose owner is dead and
/// that is at least [`TEMP_MIN_AGE`] old; with `legacy`, also an old
/// `.tmpXXXXXX` (only valid where every writer holds the ledger lock, so no
/// live writer can own one). Everything else is kept.
pub fn stale_temps(
    entries: &[(String, Duration)],
    legacy: bool,
    alive: &dyn Fn(u32) -> bool,
) -> Vec<String> {
    entries
        .iter()
        .filter(|(name, age)| {
            if *age < TEMP_MIN_AGE {
                return false;
            }
            match tmp_owner(name) {
                Some(pid) => !alive(pid),
                None => legacy && is_legacy_tmp(name),
            }
        })
        .map(|(n, _)| n.clone())
        .collect()
}

/// Corrupt copies to delete: every `<ledger>.corrupt-<nanos>-<pid>` except
/// the newest, and the newest too once it is older than [`CORRUPT_MAX_AGE`].
/// Names that do not parse are kept.
pub fn stale_corrupt(ledger_name: &str, names: &[String], now_nanos: u128) -> Vec<String> {
    let prefix = format!("{ledger_name}.corrupt-");
    let mut found: Vec<(u128, &String)> = names
        .iter()
        .filter_map(|n| {
            let rest = n.strip_prefix(&prefix)?;
            let (nanos, pid) = rest.split_once('-')?;
            if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((nanos.parse::<u128>().ok()?, n))
        })
        .collect();
    found.sort();
    let mut out = Vec::new();
    if let Some((newest, name)) = found.pop() {
        if now_nanos.saturating_sub(newest) > CORRUPT_MAX_AGE.as_nanos() {
            out.push(name.clone());
        }
    }
    out.extend(found.into_iter().map(|(_, n)| n.clone()));
    out
}

/// What one directory sweep looks at.
pub struct Sweep<'a> {
    /// Also treat old `.tmpXXXXXX` as ours (portool's own state directory only).
    pub legacy_temps: bool,
    /// The ledger file name, to expire its `.corrupt-*` copies.
    pub ledger_name: Option<&'a str>,
}

/// Lists `dir`, decides with the pure functions above, and removes through
/// `unlink` (so a caller holding a verified directory fd can unlink relative
/// to it). Best effort: a failure is reported once on stderr and never fails
/// the caller. Only names matching our patterns are `stat`ed.
pub fn sweep(
    dir: &Path,
    what: &Sweep,
    age_of: &dyn Fn(&str) -> Option<Duration>,
    unlink: &dyn Fn(&str) -> std::io::Result<()>,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let names: Vec<String> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();

    let mut doomed = Vec::new();
    let temp_candidates: Vec<(String, Duration)> = names
        .iter()
        .filter(|n| tmp_owner(n).is_some() || (what.legacy_temps && is_legacy_tmp(n)))
        .filter_map(|n| age_of(n).map(|a| (n.clone(), a)))
        .collect();
    doomed.extend(stale_temps(&temp_candidates, what.legacy_temps, &pid_alive));
    if let Some(ledger) = what.ledger_name {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        doomed.extend(stale_corrupt(ledger, &names, now));
    }

    let mut warned = false;
    for name in doomed {
        match unlink(&name) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if !warned => {
                warned = true;
                crate::warn::emit(format_args!(
                    "portool: warning: could not remove leftover {} in {}: {e}",
                    name,
                    dir.display()
                ));
            }
            Err(_) => {}
        }
    }
}

/// [`sweep`] with plain path-based `stat`/`unlink` (symlinks are never
/// followed: only regular files are aged, so a symlink is never removed).
pub fn sweep_path(dir: &Path, what: &Sweep) {
    let age_of = |name: &str| -> Option<Duration> {
        let md = std::fs::symlink_metadata(dir.join(name)).ok()?;
        if !md.file_type().is_file() {
            return None;
        }
        SystemTime::now().duration_since(md.modified().ok()?).ok()
    };
    sweep(dir, what, &age_of, &|n| std::fs::remove_file(dir.join(n)));
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = TEMP_MIN_AGE;
    fn e(n: &str, age: Duration) -> (String, Duration) {
        (n.to_string(), age)
    }
    fn dead(_: u32) -> bool {
        false
    }
    fn alive(_: u32) -> bool {
        true
    }

    #[test]
    fn owner_is_parsed_only_from_exact_pattern() {
        assert_eq!(tmp_owner(".portool-tmp-123-pre-commit"), Some(123));
        assert_eq!(tmp_owner(".portool-tmp-123-"), None);
        assert_eq!(tmp_owner(".portool-tmp--x"), None);
        assert_eq!(tmp_owner(".portool-tmp-12a-x"), None);
        assert_eq!(tmp_owner(".portool-tmp-99999999999999-x"), None);
        assert_eq!(tmp_owner("portool-tmp-1-x"), None);
        assert_eq!(tmp_owner(".tmpAbCdEf"), None);
    }

    #[test]
    fn legacy_pattern_is_exact() {
        assert!(is_legacy_tmp(".tmpAbC123"));
        assert!(!is_legacy_tmp(".tmpAbC12"));
        assert!(!is_legacy_tmp(".tmpAbC1234"));
        assert!(!is_legacy_tmp(".tmpAbC-23"));
        assert!(!is_legacy_tmp(".env.portool"));
    }

    #[test]
    fn dead_owner_at_exactly_the_age_limit_is_removed_just_under_is_kept() {
        let got = stale_temps(
            &[
                e(".portool-tmp-5-a", MIN),
                e(".portool-tmp-5-b", MIN - Duration::from_nanos(1)),
            ],
            false,
            &dead,
        );
        assert_eq!(got, vec![".portool-tmp-5-a"]);
    }

    #[test]
    fn live_owner_is_kept_however_old() {
        let got = stale_temps(&[e(".portool-tmp-5-a", MIN * 1000)], true, &alive);
        assert!(got.is_empty());
    }

    #[test]
    fn legacy_temps_only_where_allowed_and_unknown_names_kept() {
        let entries = [
            e(".tmpAbC123", MIN),
            e("notes.txt", MIN * 9),
            e(".portool-tmp-x-y", MIN),
        ];
        assert!(stale_temps(&entries, false, &dead).is_empty());
        assert_eq!(stale_temps(&entries, true, &dead), vec![".tmpAbC123"]);
    }

    #[test]
    fn pid_alive_never_signals_pid_one_or_below() {
        assert!(pid_alive(0));
        assert!(pid_alive(1));
        assert!(pid_alive(u32::MAX));
        assert!(pid_alive(std::process::id()));
    }

    fn n(s: &str) -> String {
        s.to_string()
    }
    const DAY: u128 = 24 * 3600 * 1_000_000_000;

    #[test]
    fn only_the_newest_corrupt_copy_survives() {
        let names = [
            n("registry.json.corrupt-300-1"),
            n("registry.json.corrupt-100-1"),
            n("registry.json.corrupt-200-9"),
            n("registry.json"),
        ];
        let got = stale_corrupt("registry.json", &names, 400);
        assert_eq!(
            got,
            vec!["registry.json.corrupt-100-1", "registry.json.corrupt-200-9"]
        );
    }

    #[test]
    fn newest_corrupt_copy_expires_strictly_after_the_limit() {
        let names = [n("registry.json.corrupt-1000-1")];
        let limit = CORRUPT_MAX_AGE.as_nanos();
        assert!(stale_corrupt("registry.json", &names, 1000 + limit).is_empty());
        assert_eq!(
            stale_corrupt("registry.json", &names, 1000 + limit + 1).len(),
            1
        );
        assert!(stale_corrupt("registry.json", &names, 7 * DAY / 8).is_empty());
    }

    #[test]
    fn other_names_and_other_ledgers_are_kept() {
        let names = [
            n("registry.json.corrupt-x-1"),
            n("registry.json.corrupt-5-"),
            n("registry.json.corrupt-5-z"),
            n("other.json.corrupt-1-1"),
            n("registry.json.bak"),
        ];
        assert!(stale_corrupt("registry.json", &names, u128::MAX).is_empty());
    }
}
