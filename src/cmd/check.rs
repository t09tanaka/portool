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

    // P0-2: report the backup's recovery health by parsed sequence. A backup
    // that is *behind* the ledger (or unreadable) is a degraded state -- a
    // `doctor --repair` from it would roll allocations back -- so it is a
    // non-zero exit, not a mere warning. A `Missing` backup is only a warning
    // (it heals on the next save).
    match store::backup_status(&registry_path) {
        store::BackupStatus::Fresh => {}
        store::BackupStatus::Missing => {
            eprintln!(
                "portool: warning: {} has no backup yet; it will be created on the next save",
                crate::display::path(&store::backup_path(&registry_path))
            );
        }
        store::BackupStatus::Behind { main_seq, bak_seq } => {
            return Err(Error::General(format!(
                "backup is behind the ledger (ledger sequence {main_seq}, backup {bak_seq}); \
                 a 'doctor --repair' would currently restore stale state -- investigate \
                 before relying on recovery"
            )));
        }
        store::BackupStatus::Corrupt => {
            return Err(Error::General(
                "the ledger backup is unreadable/corrupt; recovery from it is not \
                 currently possible"
                    .to_string(),
            ));
        }
    }

    println!("portool: config and ledger are OK");
    Ok(())
}
