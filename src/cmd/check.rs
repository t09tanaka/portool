//! `portool check` (hardening batch D #5): validate the global config and the
//! ledger, exiting non-zero on any problem. Read-only and script-friendly --
//! it never heals or writes anything.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::paths;
use crate::store;

/// Runs `portool check`. Loads the config (fail-closed) and the ledger
/// (validated), returning an error if either is unreadable, malformed, or
/// semantically invalid.
pub fn run() -> Result<()> {
    // `Config::load` is fail-closed: a malformed or unreadable config is an
    // error, a missing one means defaults.
    Config::load()?;

    // Read-only: `load` never mutates anything.
    let registry_path = paths::registry_path()?;
    match store::load(&registry_path) {
        store::LedgerLoad::Missing | store::LedgerLoad::Loaded(_) => {}
        store::LedgerLoad::Corrupt { reason } => {
            return Err(Error::General(format!("registry is corrupt: {reason}")));
        }
        store::LedgerLoad::UnsupportedVersion { found, supported } => {
            return Err(Error::UnsupportedRegistryVersion { found, supported });
        }
        store::LedgerLoad::ReadError { reason } => {
            return Err(Error::General(format!("registry is unreadable: {reason}")));
        }
    }

    // 指摘13: a failed backup refresh only warns at save time, so `.bak` can
    // silently go stale even though `registry.json` itself is healthy.
    // Surfaced here as a warning, not a failure: staleness heals on the
    // next save, and check's non-zero exits are reserved for real
    // corruption.
    if store::backup_is_stale(&registry_path).unwrap_or(false) {
        eprintln!(
            "portool: warning: {} is out of date (a backup refresh failed?); \
             it will be refreshed by the next save -- 'doctor --repair' would \
             currently restore stale state",
            crate::display::path(&store::backup_path(&registry_path))
        );
    }

    println!("portool: config and ledger are OK");
    Ok(())
}
