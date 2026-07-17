//! XDG-based path resolution for the registry, its lock file, and the
//! global config file (spec §3).
//!
//! Every function reads the relevant environment variables fresh on each
//! call (rather than caching them), so that callers -- and tests -- can
//! swap `XDG_STATE_HOME` / `XDG_CONFIG_HOME` / `HOME` per invocation.
//!
//! Per the XDG Base Directory specification, `$XDG_*_HOME` values that are
//! not absolute paths are invalid and ignored; the `$HOME` fallback must
//! itself be absolute. When neither an absolute XDG value nor an absolute
//! `$HOME` is available, resolution fails loudly (rather than silently
//! degrading the "global" ledger to a current-directory-relative one).

use crate::error::{Error, Result};
use std::env;
use std::path::{Path, PathBuf};

/// The portool state directory: `$XDG_STATE_HOME/portool`, falling back to
/// `$HOME/.local/state/portool`.
pub fn state_dir() -> Result<PathBuf> {
    Ok(xdg_dir("XDG_STATE_HOME", ".local/state")?.join("portool"))
}

/// The portool config directory: `$XDG_CONFIG_HOME/portool`, falling back
/// to `$HOME/.config/portool`.
pub fn config_dir() -> Result<PathBuf> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?.join("portool"))
}

/// `<state_dir>/registry.json` -- the ledger.
pub fn registry_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("registry.json"))
}

/// `<state_dir>/registry.json.lock` -- the flock target guarding the ledger.
pub fn lock_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("registry.json.lock"))
}

/// `<config_dir>/config.toml` -- the optional global config file.
pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Resolves an XDG base directory: `$<xdg_var>` when set to an **absolute**
/// path, otherwise `$HOME/<home_suffix>` (with `$HOME` required to be
/// absolute). A relative or empty XDG value is ignored per the spec.
fn xdg_dir(xdg_var: &str, home_suffix: &str) -> Result<PathBuf> {
    if let Ok(value) = env::var(xdg_var) {
        let candidate = PathBuf::from(&value);
        if candidate.is_absolute() {
            return Ok(candidate);
        }
        if !value.is_empty() {
            eprintln!(
                "portool: warning: ignoring non-absolute {xdg_var}={value:?} \
                 (the XDG Base Directory spec requires an absolute path)"
            );
        }
    }

    match env::var("HOME") {
        Ok(home) if Path::new(&home).is_absolute() => Ok(PathBuf::from(home).join(home_suffix)),
        _ => Err(Error::General(format!(
            "cannot locate portool's directory: set an absolute ${xdg_var} or an absolute $HOME"
        ))),
    }
}
