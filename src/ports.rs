//! `127.0.0.1` TCP bind checks, used both by block allocation (spec §6.3)
//! and by GC (spec §8.1) to detect ports in use by processes outside the
//! ledger.

use std::net::TcpListener;

/// Whether `port` is currently free on `127.0.0.1`, determined by
/// attempting to bind it. The listener is dropped immediately, so this is
/// inherently TOCTOU (spec §6.3 acknowledges this).
pub fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Whether every port in the inclusive range `block` is free.
pub fn block_free(block: (u16, u16)) -> bool {
    (block.0..=block.1).all(port_free)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;

    /// These tests use `bind(0)` to obtain a genuinely free ephemeral port
    /// and then immediately release it to check `port_free`/`block_free`
    /// against it. `cargo test`'s default parallelism means the OS could
    /// otherwise hand that just-released port to some other concurrently
    /// running test (in this crate or elsewhere on the machine) before
    /// this one re-checks it; serializing the three tests here plus a
    /// short bounded retry on the "now free" assertions absorbs that
    /// transient contention. Production `port_free`/`block_free` remain
    /// inherently TOCTOU against processes outside this test binary, as
    /// documented above -- this is purely test robustness, not a change
    /// to that behavior.
    static PORT_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock_guard() -> std::sync::MutexGuard<'static, ()> {
        PORT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Retries `check` for up to ~1s, on the assumption that any conflict
    /// on a just-released ephemeral port is transient.
    fn eventually(mut check: impl FnMut() -> bool) -> bool {
        for _ in 0..50 {
            if check() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn port_free_is_false_while_bound_and_true_after_release() {
        let _guard = lock_guard();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        assert!(!port_free(port));

        drop(listener);

        assert!(
            eventually(|| port_free(port)),
            "port {port} never became free"
        );
    }

    #[test]
    fn block_free_true_for_a_released_port() {
        let _guard = lock_guard();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        assert!(
            eventually(|| block_free((port, port))),
            "block ({port}, {port}) never became free"
        );
    }

    #[test]
    fn block_free_false_when_any_port_in_block_is_occupied() {
        let _guard = lock_guard();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let block = if port < u16::MAX - 1 {
            (port, port + 1)
        } else {
            (port - 1, port)
        };

        assert!(!block_free(block));
    }
}
