//! Isolated coverage for the process-environment-dependent parts of the I/O
//! layer: XDG resolution (`paths::*`) and `Config::load`, exercised as a
//! CLI black box (`portool` is binary-only as of v0.9.0 -- there is no
//! `portool::config` / `portool::paths` to import and call directly).
//!
//! Each test spawns the `portool` binary with `Command::env_clear()` plus
//! an explicit, per-test `HOME` / `XDG_STATE_HOME` / `XDG_CONFIG_HOME` (as
//! `tests/cli.rs`'s `TestEnv` already does). Unlike the old single-test
//! approach built on `std::env::set_var`/`remove_var` -- which mutate the
//! whole process's environment table and are documented as unsound to do
//! concurrently with other threads -- each test here isolates its
//! environment to its own child process, so these tests run in parallel
//! like every other test in this crate. The real `~/.local/state` and
//! `~/.config` are never touched.
//!
//! `portool reserve` is used as the environment-observing command for the
//! state-dir/registry/lock-path checks: unlike `sync`, it never requires a
//! git repository, and it always creates the registry (and its lock file)
//! on success, so a single command exercises `paths::state_dir`,
//! `paths::registry_path`, and `paths::lock_path` together. `portool ls
//! --json` is used for the config-dir/`Config::load` checks: it loads the
//! config before touching the registry and, on `--json`, always emits a
//! versioned JSON envelope (`ok`/`error`) even on failure, so a config
//! problem is observable without depending on a nonzero exit code alone.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// An isolated `HOME` / `XDG_STATE_HOME` / `XDG_CONFIG_HOME` for one test.
/// `state`/`config` are deliberately *not* pre-created (unlike
/// `tests/cli.rs`'s `TestEnv`): part of what these tests verify is that
/// `portool` creates its state/config directories on demand at the
/// expected, environment-derived path.
struct Env {
    home: PathBuf,
    state: PathBuf,
    config: PathBuf,
    _tmp: TempDir,
}

impl Env {
    fn new() -> Self {
        let tmp = TempDir::new().expect("failed to create temp dir");
        Env {
            home: tmp.path().join("home"),
            state: tmp.path().join("xdg-state"),
            config: tmp.path().join("xdg-config"),
            _tmp: tmp,
        }
    }
}

/// Runs `portool` in `cwd` with a fully isolated (`env_clear`-ed)
/// environment: only `PATH` (needed to spawn `git`, which some commands
/// invoke even outside a git repo's fast paths) plus whatever `env_vars`
/// the caller supplies.
fn run(cwd: &Path, args: &[&str], env_vars: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_portool"));
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    for (key, value) in env_vars {
        cmd.env(key, value);
    }
    cmd.current_dir(cwd)
        .args(args)
        .output()
        .expect("failed to spawn portool")
}

fn ls_json(cwd: &Path, env_vars: &[(&str, &str)]) -> serde_json::Value {
    let out = run(cwd, &["ls", "--json", "--all"], env_vars);
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout was not JSON: {e}; stdout: {:?}, stderr: {:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn write_config(dir: &Path, contents: &str) {
    let portool_dir = dir.join("portool");
    fs::create_dir_all(&portool_dir).unwrap();
    fs::write(portool_dir.join("config.toml"), contents).unwrap();
}

// --- state_dir / registry_path / lock_path: HOME fallback ------------------

/// No `XDG_STATE_HOME` set: the registry and its lock file must land under
/// `$HOME/.local/state/portool` (mirrors the old
/// `paths::state_dir`/`registry_path`/`lock_path` assertions under the HOME
/// fallback).
#[test]
fn home_fallback_resolves_state_dir() {
    let env = Env::new();
    fs::create_dir_all(&env.home).unwrap();
    let scratch = env._tmp.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();

    let out = run(
        &scratch,
        &["reserve", "18500-18504"],
        &[("HOME", env.home.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let registry_path = env.home.join(".local/state/portool/registry.json");
    let lock_path = env.home.join(".local/state/portool/registry.json.lock");
    assert!(
        registry_path.is_file(),
        "registry.json must be created at $HOME/.local/state/portool/registry.json"
    );
    assert!(
        lock_path.is_file(),
        "registry.json.lock must be created alongside the registry"
    );

    let contents = fs::read_to_string(&registry_path).unwrap();
    assert!(contents.contains("18500"), "registry contents: {contents}");
}

// --- state_dir / config_dir: XDG_* override HOME ----------------------------

/// `XDG_STATE_HOME` / `XDG_CONFIG_HOME` set: both must take priority over
/// `HOME`, and the registry must NOT be created under the (unused) HOME
/// fallback path.
#[test]
fn xdg_vars_override_home_for_state_dir() {
    let env = Env::new();
    fs::create_dir_all(&env.home).unwrap();
    fs::create_dir_all(&env.state).unwrap();
    let scratch = env._tmp.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();

    let out = run(
        &scratch,
        &["reserve", "18510-18514"],
        &[
            ("HOME", env.home.to_str().unwrap()),
            ("XDG_STATE_HOME", env.state.to_str().unwrap()),
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let xdg_registry = env.state.join("portool/registry.json");
    assert!(
        xdg_registry.is_file(),
        "registry.json must be created under XDG_STATE_HOME/portool, not the HOME fallback"
    );
    assert!(
        !env.home.join(".local").exists(),
        "the HOME fallback path must not be touched when XDG_STATE_HOME is set"
    );
}

/// `XDG_CONFIG_HOME` set: `Config::load` must read `config.toml` from
/// `$XDG_CONFIG_HOME/portool`, not `$HOME/.config/portool`.
#[test]
fn xdg_config_home_overrides_home_for_config_dir() {
    let env = Env::new();
    fs::create_dir_all(&env.home).unwrap();
    fs::create_dir_all(&env.state).unwrap();
    fs::create_dir_all(&env.config).unwrap();
    write_config(&env.config, "block_align = 10\n");
    let repo = env._tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();

    let env_vars = [
        ("HOME", env.home.to_str().unwrap()),
        ("XDG_STATE_HOME", env.state.to_str().unwrap()),
        ("XDG_CONFIG_HOME", env.config.to_str().unwrap()),
    ];
    let v = ls_json(&repo, &env_vars);
    assert_eq!(v["ok"], true, "stdout: {v}");
    assert_eq!(
        v["effective_config"]["block_align"],
        serde_json::json!(10),
        "must read config.toml from XDG_CONFIG_HOME/portool, not the HOME fallback: {v}"
    );
    assert!(
        !env.home.join(".config").exists(),
        "the HOME fallback config path must not be touched when XDG_CONFIG_HOME is set"
    );
}

// --- Config::load: no config file present -> defaults -----------------------

/// No `config.toml` present anywhere: `Config::load` returns
/// `Config::default()` (range 3000-9999, block_align 5), not an error.
#[test]
fn missing_config_file_yields_defaults() {
    let env = Env::new();
    fs::create_dir_all(&env.home).unwrap();
    let repo = env._tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();

    let v = ls_json(&repo, &[("HOME", env.home.to_str().unwrap())]);
    assert_eq!(v["ok"], true, "stdout: {v}");
    assert_eq!(
        v["effective_config"],
        serde_json::json!({"range": [3000, 9999], "block_align": 5})
    );
}

// --- Config::load: fail-closed behaviors -------------------------------------

/// Malformed TOML must be a hard error (never a silent revert to defaults).
#[test]
fn malformed_config_is_fail_closed() {
    let env = Env::new();
    fs::create_dir_all(&env.home).unwrap();
    write_config(&env.home.join(".config"), "this is not valid toml =====");
    let repo = env._tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();

    let out = run(
        &repo,
        &["ls", "--json", "--all"],
        &[("HOME", env.home.to_str().unwrap())],
    );
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().is_some());
}

/// An unknown field (typo, e.g. `ragne`) must be rejected, not silently
/// ignored.
#[test]
fn unknown_config_field_is_rejected() {
    let env = Env::new();
    fs::create_dir_all(&env.home).unwrap();
    write_config(&env.home.join(".config"), "ragne = [4000, 5000]\n");
    let repo = env._tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();

    let out = run(
        &repo,
        &["ls", "--json", "--all"],
        &[("HOME", env.home.to_str().unwrap())],
    );
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().is_some());
}

/// A non-`NotFound` read error (config.toml is a directory, not a file)
/// must also be fail-closed, and must leave the offending path untouched.
#[test]
fn config_path_as_directory_is_fail_closed_and_left_in_place() {
    let env = Env::new();
    fs::create_dir_all(&env.home).unwrap();
    let config_dir = env.home.join(".config/portool");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir(config_dir.join("config.toml")).unwrap();
    let repo = env._tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();

    let out = run(
        &repo,
        &["ls", "--json", "--all"],
        &[("HOME", env.home.to_str().unwrap())],
    );
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().is_some());
    assert!(
        config_dir.join("config.toml").is_dir(),
        "portool must not delete/replace the unreadable config path"
    );
}

// --- XDG_STATE_HOME edge cases: empty and non-absolute ----------------------

/// An empty `XDG_STATE_HOME` must be treated as unset, falling back to
/// `$HOME/.local/state/portool`.
#[test]
fn empty_xdg_state_home_is_treated_as_unset() {
    let env = Env::new();
    fs::create_dir_all(&env.home).unwrap();
    let scratch = env._tmp.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();

    let out = run(
        &scratch,
        &["reserve", "18520-18524"],
        &[("HOME", env.home.to_str().unwrap()), ("XDG_STATE_HOME", "")],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        env.home
            .join(".local/state/portool/registry.json")
            .is_file(),
        "an empty XDG_STATE_HOME must fall back to $HOME/.local/state/portool"
    );
}

/// A non-absolute `XDG_STATE_HOME` must be ignored per the XDG Base
/// Directory spec (not used verbatim, e.g. relative to cwd), falling back
/// to `$HOME/.local/state/portool` and warning on stderr.
#[test]
fn non_absolute_xdg_state_home_is_ignored_with_warning() {
    let env = Env::new();
    fs::create_dir_all(&env.home).unwrap();
    let scratch = env._tmp.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();

    let out = run(
        &scratch,
        &["reserve", "18530-18534"],
        &[
            ("HOME", env.home.to_str().unwrap()),
            ("XDG_STATE_HOME", "relative/state"),
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        env.home
            .join(".local/state/portool/registry.json")
            .is_file(),
        "a relative XDG_STATE_HOME must be ignored, falling back to $HOME/.local/state/portool"
    );
    assert!(
        !scratch.join("relative/state").exists(),
        "the relative value must never be used verbatim (e.g. relative to cwd)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ignoring non-absolute"),
        "must warn about ignoring the non-absolute XDG_STATE_HOME, got: {stderr}"
    );
}
