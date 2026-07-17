//! Exclusive `flock` on `registry.json.lock`, acquired via polling with a
//! timeout (spec §11).

use crate::error::{Error, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A held exclusive lock. The lock is released automatically when this
/// value is dropped.
#[derive(Debug)]
pub struct Lock {
    file: File,
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Acquires an exclusive lock on `lock_path`, creating the file if it does
/// not already exist (and its parent directory, if necessary).
///
/// Polls [`fs2::FileExt::try_lock_exclusive`] every 50ms until it succeeds
/// or `timeout` elapses, in which case [`Error::LockTimeout`] is returned.
/// Only genuine lock *contention* is retried; any other flock failure (an
/// unsupported filesystem, EIO, ...) is returned immediately as itself, so
/// it can never masquerade as a 10-second timeout (external review P2 #8).
pub fn acquire(lock_path: &Path, timeout: Duration) -> Result<Lock> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;

    let contended_kind = fs2::lock_contended_error().kind();
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(Lock { file }),
            Err(err) if err.kind() == contended_kind => {}
            Err(err) => {
                return Err(Error::General(format!(
                    "failed to lock {}: {err}",
                    lock_path.display()
                )));
            }
        }
        if Instant::now() >= deadline {
            return Err(Error::LockTimeout);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use tempfile::TempDir;

    #[test]
    fn second_acquire_blocks_until_release_then_succeeds() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json.lock");

        let first = acquire(&path, Duration::from_secs(5)).unwrap();

        let (tx, rx) = mpsc::channel();
        let path_clone = path.clone();
        let handle = thread::spawn(move || {
            let start = Instant::now();
            let result = acquire(&path_clone, Duration::from_secs(5));
            tx.send((result.is_ok(), start.elapsed())).unwrap();
        });

        // Hold the lock for a while; the spawned thread must still be
        // waiting when we release it.
        thread::sleep(Duration::from_millis(200));
        drop(first);

        let (acquired, elapsed) = rx.recv().unwrap();
        handle.join().unwrap();

        assert!(acquired, "second acquire should succeed after release");
        assert!(
            elapsed >= Duration::from_millis(150),
            "second acquire returned too quickly ({elapsed:?}); it should have blocked"
        );
    }

    #[test]
    fn short_timeout_returns_lock_timeout() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registry.json.lock");

        let _held = acquire(&path, Duration::from_secs(5)).unwrap();

        let start = Instant::now();
        let err = acquire(&path, Duration::from_millis(150)).unwrap_err();
        let elapsed = start.elapsed();

        assert_eq!(err.exit_code(), 4);
        assert!(elapsed >= Duration::from_millis(150));
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout took too long: {elapsed:?}"
        );
    }

    #[test]
    fn acquire_creates_missing_parent_directory() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("registry.json.lock");

        let lock = acquire(&path, Duration::from_secs(1)).unwrap();
        assert!(path.exists());
        drop(lock);
    }
}
