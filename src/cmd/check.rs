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

    // Read-only: never heal (rename aside) here. `load` reports corruption
    // and read errors via flags rather than mutating anything.
    let load = store::load(&paths::registry_path()?, false);
    if load.read_error {
        return Err(Error::General("registry is unreadable".to_string()));
    }
    if load.corrupt {
        return Err(Error::General("registry is corrupt".to_string()));
    }

    println!("portool: config and ledger are OK");
    Ok(())
}
