//! XDG-based path resolution for the registry, its lock file, and the
//! global config file (spec §3).
//!
//! Every function reads the relevant environment variables fresh on each
//! call (rather than caching them), so that callers -- and tests -- can
//! swap `XDG_STATE_HOME` / `XDG_CONFIG_HOME` / `HOME` per invocation.
//!
//! A library entry point given an explicit [`Context`] runs its work inside
//! [`scoped`], which makes every lookup on the current thread read the
//! context's variables instead of the process environment.
//!
//! Per the XDG Base Directory specification, `$XDG_*_HOME` values that are
//! not absolute paths are invalid and ignored; the `$HOME` fallback must
//! itself be absolute. When neither an absolute XDG value nor an absolute
//! `$HOME` is available, resolution fails loudly (rather than silently
//! degrading the "global" ledger to a current-directory-relative one).

use crate::error::{Error, Result};
use std::cell::RefCell;
use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Where portool keeps its state (the registry and its lock) and its global
/// config, for library calls.
///
/// [`Context::process`] reads `HOME`, `XDG_STATE_HOME`, and
/// `XDG_CONFIG_HOME` from this process's environment on every call, which
/// is what the CLI and the library functions without an `_in` suffix do.
/// [`Context::from_env`] takes those variables from an explicit environment
/// instead, for a caller that runs commands with an environment different
/// from its own and needs the library to use the same ledger and config
/// those commands see.
///
/// Resolution is the CLI's: `$XDG_STATE_HOME/portool` (else
/// `$HOME/.local/state/portool`) for the registry and its lock, and
/// `$XDG_CONFIG_HOME/portool/config.toml` (else
/// `$HOME/.config/portool/config.toml`) for the config. An XDG value counts
/// only when it is an absolute path, and `HOME` must be absolute.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    /// `None`: the process environment, read on each call.
    vars: Option<Vars>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Vars {
    home: Option<String>,
    xdg_state_home: Option<String>,
    xdg_config_home: Option<String>,
}

impl Vars {
    fn get(&self, name: &str) -> Option<String> {
        match name {
            "HOME" => self.home.clone(),
            "XDG_STATE_HOME" => self.xdg_state_home.clone(),
            "XDG_CONFIG_HOME" => self.xdg_config_home.clone(),
            _ => unreachable!("portool resolves no path from ${name}"),
        }
    }
}

impl Context {
    /// This process's environment, read afresh on every call.
    pub fn process() -> Context {
        Context::default()
    }

    /// The locations resolved from `vars`, an environment given as
    /// name/value pairs (for example the environment a command will run
    /// with). Only `HOME`, `XDG_STATE_HOME`, and `XDG_CONFIG_HOME` are read
    /// and other variables are ignored. A variable missing from `vars` (or
    /// not valid UTF-8) counts as unset: the process environment is never
    /// consulted.
    pub fn from_env<I, K, V>(vars: I) -> Context
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let mut found = Vars::default();
        for (name, value) in vars {
            let Some(value) = value.as_ref().to_str() else {
                continue;
            };
            let slot = match name.as_ref().to_str() {
                Some("HOME") => &mut found.home,
                Some("XDG_STATE_HOME") => &mut found.xdg_state_home,
                Some("XDG_CONFIG_HOME") => &mut found.xdg_config_home,
                _ => continue,
            };
            *slot = Some(value.to_owned());
        }
        Context { vars: Some(found) }
    }
}

thread_local! {
    static SCOPE: RefCell<Option<Vars>> = const { RefCell::new(None) };
}

/// Restores the previous scope when a [`scoped`] call ends, even on panic.
struct Restore(Option<Vars>);

impl Drop for Restore {
    fn drop(&mut self) {
        let previous = self.0.take();
        SCOPE.with(|scope| *scope.borrow_mut() = previous);
    }
}

/// Runs `f` with every path lookup on this thread resolved from `context`.
/// A [`Context::process`] context leaves the current resolution as it is.
pub(crate) fn scoped<T>(context: &Context, f: impl FnOnce() -> T) -> T {
    let Some(vars) = &context.vars else {
        return f();
    };
    let previous = SCOPE.with(|scope| scope.borrow_mut().replace(vars.clone()));
    let _guard = Restore(previous);
    f()
}

/// Reads `HOME` or an `XDG_*_HOME` variable: from the active [`scoped`]
/// context if there is one, otherwise from the process environment.
fn var(name: &str) -> Option<String> {
    SCOPE
        .with(|scope| scope.borrow().as_ref().map(|vars| vars.get(name)))
        .unwrap_or_else(|| env::var(name).ok())
}

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
    if let Some(value) = var(xdg_var) {
        let candidate = PathBuf::from(&value);
        if candidate.is_absolute() {
            return Ok(candidate);
        }
        if !value.is_empty() {
            crate::warn::emit(format_args!(
                "portool: warning: ignoring non-absolute {xdg_var}={value:?} \
                 (the XDG Base Directory spec requires an absolute path)"
            ));
        }
    }

    match var("HOME") {
        Some(home) if Path::new(&home).is_absolute() => Ok(PathBuf::from(home).join(home_suffix)),
        _ => Err(Error::General(format!(
            "cannot locate portool's directory: set an absolute ${xdg_var} or an absolute $HOME"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_context_overrides_and_restores() {
        let outer = Context::from_env([("HOME", "/outer"), ("PATH", "/bin")]);
        let inner = Context::from_env([("XDG_STATE_HOME", "/xdg"), ("HOME", "/inner")]);
        scoped(&outer, || {
            assert_eq!(
                state_dir().unwrap(),
                Path::new("/outer/.local/state/portool")
            );
            scoped(&inner, || {
                assert_eq!(state_dir().unwrap(), Path::new("/xdg/portool"));
                assert_eq!(config_dir().unwrap(), Path::new("/inner/.config/portool"));
            });
            // A process context inside an explicit scope keeps that scope.
            scoped(&Context::process(), || {
                assert_eq!(config_dir().unwrap(), Path::new("/outer/.config/portool"));
            });
            assert_eq!(
                state_dir().unwrap(),
                Path::new("/outer/.local/state/portool")
            );
        });
    }

    #[test]
    fn explicit_context_never_falls_back_to_the_process() {
        let empty = Context::from_env(std::iter::empty::<(&str, &str)>());
        assert!(scoped(&empty, state_dir).is_err());
        let relative = Context::from_env([("HOME", "relative/home")]);
        let (result, warnings) = crate::warn::collect(|| scoped(&relative, config_dir));
        assert!(result.is_err());
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
