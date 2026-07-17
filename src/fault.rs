//! Test-only crash injection: [`point`] aborts the process when the
//! `PORTOOL_TEST_FAULT` environment variable equals the point's name.
//! Compiled to a no-op outside debug builds, so release binaries carry no
//! fault logic. E2E tests (tests/fault.rs) set the variable on a spawned
//! debug binary to verify crash-consistency at each boundary.

/// Aborts the process if `PORTOOL_TEST_FAULT` equals `name` (debug builds
/// only). Placed at crash-consistency boundaries so E2E tests can verify
/// that an interruption at each point leaves the ledger recoverable.
pub fn point(name: &str) {
    #[cfg(debug_assertions)]
    if std::env::var("PORTOOL_TEST_FAULT").as_deref() == Ok(name) {
        eprintln!("portool: test fault triggered at {name}");
        std::process::abort();
    }
    #[cfg(not(debug_assertions))]
    let _ = name;
}

#[cfg(test)]
mod tests {
    use super::*;

    // The abort path is exercised end-to-end in tests/fault.rs (a spawned
    // binary can die; this test process must not). Here we only verify the
    // no-match paths return normally.
    #[test]
    fn point_returns_when_variable_is_unset_or_different() {
        // The test harness never sets PORTOOL_TEST_FAULT for unit tests.
        point("after_pending_save");
        point("during_backup");
    }
}
