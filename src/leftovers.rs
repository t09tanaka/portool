//! Removal of the files portool itself leaves behind: crash temp files, old
//! `registry.json.corrupt-*` copies and an expired failed-check memory. The
//! decisions are pure functions (names and ages in, names to delete out);
//! [`sweep`] does the I/O.
//!
//! Only names portool created are ever touched: `.portool-tmp-<pid>-<rest>`
//! anywhere, the pre-0.14 `.tmpXXXXXX` form only in portool's own state
//! directory, and `<ledger>.corrupt-<nanos>-<pid>` and the failed-check
//! memory next to the ledger.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Prefix of every temp file portool creates for an atomic write.
pub const TMP_PREFIX: &str = ".portool-tmp-";

/// A temp file lives for milliseconds between create and rename; ten minutes
/// leaves room for a stalled `fsync` on a network filesystem and costs
/// nothing, since the owner must also be dead.
pub const TEMP_MIN_AGE: Duration = Duration::from_secs(10 * 60);

/// A corrupt copy is read by a human, after the `doctor --repair` that
/// printed its path; with `--abandon-other-projects` it is the only copy of
/// every other project's allocations. A week covers a weekend away. Copies
/// expire by age alone, so the bound is the number of repairs in a week.
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

/// Corrupt copies to delete: a `<ledger>.corrupt-<nanos>-<pid>` whose age
/// (by modification time, stamped when the copy is made) is more than
/// [`CORRUPT_MAX_AGE`]. Never by count and never by the time in the name: a
/// clock that stepped back must not make the copy just written look old.
/// Kept: a copy this process made (`own_pid`; a command never deletes the
/// copy whose path it prints), an age of `None` (modified in the future, or
/// not a regular file), and names that do not parse.
pub fn stale_corrupt(
    ledger_name: &str,
    entries: &[(String, Option<Duration>)],
    own_pid: u32,
) -> Vec<String> {
    let prefix = format!("{ledger_name}.corrupt-");
    entries
        .iter()
        .filter(|(name, age)| {
            let Some((nanos, pid)) = name
                .strip_prefix(&prefix)
                .and_then(|rest| rest.split_once('-'))
            else {
                return false;
            };
            let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            if !digits(nanos) || !digits(pid) || pid.parse() == Ok(own_pid) {
                return false;
            }
            age.is_some_and(|age| age > CORRUPT_MAX_AGE)
        })
        .map(|(name, _)| name.clone())
        .collect()
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
        let prefix = format!("{ledger}.corrupt-");
        let copies: Vec<(String, Option<Duration>)> = names
            .iter()
            .filter(|n| n.starts_with(&prefix))
            .map(|n| (n.clone(), age_of(n)))
            .collect();
        doomed.extend(stale_corrupt(ledger, &copies, std::process::id()));
        // The failed-check memory is useless once its newest entry expired.
        let memory = crate::reclaim::MEMORY_FILE;
        if names.iter().any(|n| n == memory)
            && age_of(memory).is_some_and(|age| age >= crate::reclaim::RETRY_AFTER)
        {
            doomed.push(memory.to_string());
        }
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

    fn c(name: &str, age: Option<Duration>) -> (String, Option<Duration>) {
        (name.to_string(), age)
    }
    const WEEK: Duration = CORRUPT_MAX_AGE;
    const OTHER: u32 = 1;

    #[test]
    fn corrupt_copies_expire_by_age_only_never_by_count() {
        let fresh = Some(Duration::from_secs(60));
        let copies = [
            c("registry.json.corrupt-300-7", fresh),
            c("registry.json.corrupt-100-7", fresh),
            c("registry.json.corrupt-200-9", fresh),
        ];
        assert!(stale_corrupt("registry.json", &copies, OTHER).is_empty());
    }

    #[test]
    fn corrupt_copy_expires_strictly_after_the_limit() {
        let at = |age| [c("registry.json.corrupt-1000-7", Some(age))];
        assert!(stale_corrupt("registry.json", &at(WEEK), OTHER).is_empty());
        assert_eq!(
            stale_corrupt("registry.json", &at(WEEK + Duration::from_nanos(1)), OTHER),
            vec!["registry.json.corrupt-1000-7"]
        );
    }

    #[test]
    fn the_time_in_the_name_is_ignored() {
        // The clock stepped back: older copies carry later times in their
        // names than the copy just made. Nothing is deleted for that.
        let copies = [
            c(
                "registry.json.corrupt-99999999999999999999-7",
                Some(Duration::ZERO),
            ),
            c("registry.json.corrupt-5-8", Some(Duration::ZERO)),
        ];
        assert!(stale_corrupt("registry.json", &copies, OTHER).is_empty());
    }

    #[test]
    fn own_copy_and_future_or_unknown_age_are_kept() {
        let old = Some(WEEK * 2);
        let copies = [
            c("registry.json.corrupt-1-42", old), // made by this process
            c("registry.json.corrupt-2-7", None), // modified in the future
        ];
        assert!(stale_corrupt("registry.json", &copies, 42).is_empty());
        assert_eq!(
            stale_corrupt("registry.json", &copies, OTHER),
            vec!["registry.json.corrupt-1-42"]
        );
    }

    #[test]
    fn other_names_and_other_ledgers_are_kept() {
        let old = Some(WEEK * 2);
        let copies = [
            c("registry.json.corrupt-x-1", old),
            c("registry.json.corrupt-5-", old),
            c("registry.json.corrupt-5-z", old),
            c("registry.json.corrupt--5", old),
            c("other.json.corrupt-1-1", old),
            c("registry.json.bak", old),
        ];
        assert!(stale_corrupt("registry.json", &copies, OTHER).is_empty());
    }
}
