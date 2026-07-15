//! Isolated coverage for the process-environment-dependent parts of the
//! I/O layer: `paths::*` (XDG resolution) and `Config::load`.
//!
//! This lives in its own integration-test binary and contains exactly one
//! `#[test]` function. `std::env::set_var`/`remove_var` mutate the whole
//! process's environment table, which is documented as unsound to do
//! concurrently with other threads reading/writing it; every other test in
//! this crate takes paths as explicit arguments and never touches
//! `HOME` / `XDG_STATE_HOME` / `XDG_CONFIG_HOME`, so keeping all such
//! manipulation inside this single test (in its own process, since each
//! file under `tests/` compiles to a separate binary) guarantees there is
//! never a second thread that could race with it.

use portool::config::Config;
use portool::paths;
use tempfile::TempDir;

#[test]
fn xdg_and_home_resolution_and_config_load() {
    let saved: Vec<(&str, Option<String>)> = ["XDG_STATE_HOME", "XDG_CONFIG_HOME", "HOME"]
        .iter()
        .map(|&k| (k, std::env::var(k).ok()))
        .collect();

    // SAFETY: this is the only test in this binary, and no other thread in
    // this process reads or writes the environment while it runs.
    unsafe {
        std::env::remove_var("XDG_STATE_HOME");
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    // --- HOME fallback (no XDG_* set) ---------------------------------
    let home = TempDir::new().unwrap();
    // SAFETY: see above.
    unsafe {
        std::env::set_var("HOME", home.path());
    }

    assert_eq!(paths::state_dir(), home.path().join(".local/state/portool"));
    assert_eq!(
        paths::registry_path(),
        home.path().join(".local/state/portool/registry.json")
    );
    assert_eq!(
        paths::lock_path(),
        home.path().join(".local/state/portool/registry.json.lock")
    );
    assert_eq!(paths::config_dir(), home.path().join(".config/portool"));
    assert_eq!(
        paths::config_path(),
        home.path().join(".config/portool/config.toml")
    );

    // No config.toml present yet -> Config::load falls back to defaults.
    assert_eq!(Config::load(), Config::default());

    // --- XDG_* override HOME when set ---------------------------------
    let xdg_state = TempDir::new().unwrap();
    let xdg_config = TempDir::new().unwrap();
    // SAFETY: see above.
    unsafe {
        std::env::set_var("XDG_STATE_HOME", xdg_state.path());
        std::env::set_var("XDG_CONFIG_HOME", xdg_config.path());
    }

    assert_eq!(paths::state_dir(), xdg_state.path().join("portool"));
    assert_eq!(paths::config_dir(), xdg_config.path().join("portool"));
    assert_eq!(
        paths::registry_path(),
        xdg_state.path().join("portool/registry.json")
    );

    // --- Config::load parses a present, valid config file -------------
    std::fs::create_dir_all(paths::config_dir()).unwrap();
    std::fs::write(paths::config_path(), "block_align = 10\n").unwrap();

    let cfg = Config::load();
    assert_eq!(cfg.block_align, 10);
    assert_eq!(cfg.range, Config::default().range);

    // --- Config::load falls back to defaults on malformed TOML --------
    std::fs::write(paths::config_path(), "this is not valid toml =====").unwrap();
    assert_eq!(Config::load(), Config::default());

    // --- Config::load falls back to defaults on a non-NotFound read
    // --- error (config.toml is a directory), leaving it in place ------
    std::fs::remove_file(paths::config_path()).unwrap();
    std::fs::create_dir(paths::config_path()).unwrap();
    assert_eq!(Config::load(), Config::default());
    assert!(paths::config_path().is_dir());
    std::fs::remove_dir(paths::config_path()).unwrap();

    // --- empty XDG_STATE_HOME is treated as unset ----------------------
    // SAFETY: see above.
    unsafe {
        std::env::set_var("XDG_STATE_HOME", "");
    }
    assert_eq!(paths::state_dir(), home.path().join(".local/state/portool"));

    // Restore the original environment for hygiene.
    // SAFETY: see above.
    unsafe {
        for (key, value) in saved {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
