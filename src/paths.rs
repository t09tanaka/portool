//! XDG-based path resolution for the registry, its lock file, and the
//! global config file (spec §3).
//!
//! Every function reads the relevant environment variables fresh on each
//! call (rather than caching them), so that callers -- and tests -- can
//! swap `XDG_STATE_HOME` / `XDG_CONFIG_HOME` / `HOME` per invocation.

use std::env;
use std::path::PathBuf;

/// The portool state directory: `$XDG_STATE_HOME/portool`, falling back to
/// `$HOME/.local/state/portool` if `XDG_STATE_HOME` is unset (or empty).
pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state").join("portool")
}

/// The portool config directory: `$XDG_CONFIG_HOME/portool`, falling back
/// to `$HOME/.config/portool` if `XDG_CONFIG_HOME` is unset (or empty).
pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("portool")
}

/// `<state_dir>/registry.json` -- the ledger.
pub fn registry_path() -> PathBuf {
    state_dir().join("registry.json")
}

/// `<state_dir>/registry.json.lock` -- the flock target guarding the
/// ledger.
pub fn lock_path() -> PathBuf {
    state_dir().join("registry.json.lock")
}

/// `<config_dir>/config.toml` -- the optional global config file.
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Resolves an XDG base directory: `$<xdg_var>` if set to a non-empty
/// value, otherwise `$HOME/<home_suffix>`.
fn xdg_dir(xdg_var: &str, home_suffix: &str) -> PathBuf {
    if let Ok(value) = env::var(xdg_var) {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(home_suffix)
}
