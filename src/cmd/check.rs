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
    match store::load(&paths::registry_path()?) {
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

    println!("portool: config and ledger are OK");
    Ok(())
}
