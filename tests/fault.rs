//! Crash-consistency E2E: PORTOOL_TEST_FAULT aborts the (debug) binary at
//! a specific boundary; a follow-up sync/doctor must recover to a valid,
//! consistent state. Verifies the trust contract the two-phase update and
//! atomic backup are supposed to provide.
//!
//! Helpers below are copied (not shared) from `tests/cli.rs`, per this
//! repo's convention that integration test binaries cannot share code.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// An isolated `HOME` / `XDG_STATE_HOME` / `XDG_CONFIG_HOME` for one test,
/// plus a scratch area (`root`) for throwaway git repositories.
struct TestEnv {
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    config: PathBuf,
    _tmp: TempDir,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let config = tmp.path().join("config");
        let root = tmp.path().join("root");
        for dir in [&home, &state, &config, &root] {
            fs::create_dir_all(dir).unwrap();
        }
        TestEnv {
            root,
            home,
            state,
            config,
            _tmp: tmp,
        }
    }

    /// A `Command` for the `portool` binary under test, with a fully
    /// isolated (`env_clear`-ed) environment: only `PATH` (needed to spawn
    /// `git`), `HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME`, and the two
    /// `GIT_CONFIG_*` overrides that keep it from ever reading the host's
    /// real git config.
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_portool"));
        cmd.env_clear();
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        cmd.env("HOME", &self.home);
        cmd.env("XDG_STATE_HOME", &self.state);
        cmd.env("XDG_CONFIG_HOME", &self.config);
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
        cmd.env("GIT_CONFIG_SYSTEM", "/dev/null");
        cmd
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.command()
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn portool")
    }

    /// Runs portool with a fault point armed.
    fn run_with_fault(&self, dir: &Path, args: &[&str], fault: &str) -> Output {
        self.command()
            .current_dir(dir)
            .env("PORTOOL_TEST_FAULT", fault)
            .args(args)
            .output()
            .expect("failed to spawn portool")
    }

    /// A not-yet-created path under this test's scratch root.
    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn registry_path(&self) -> PathBuf {
        self.state.join("portool").join("registry.json")
    }

    /// Writes `config.toml` into this test's isolated `XDG_CONFIG_HOME`.
    fn write_config(&self, contents: &str) {
        let dir = self.config.join("portool");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), contents).unwrap();
    }

    fn registry(&self) -> serde_json::Value {
        let contents = fs::read_to_string(self.registry_path()).expect("registry.json missing");
        serde_json::from_str(&contents).expect("registry.json is not valid JSON")
    }
}

/// Runs a git command isolated from the host machine's real global/system
/// config, mirroring `src/gitctx.rs`'s own test helper.
fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("failed to run git")
}

fn init_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    assert!(git(dir, &["init", "-q", "-b", "main"]).status.success());
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    assert!(git(dir, &["add", "README.md"]).status.success());
    assert!(git(
        dir,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    )
    .status
    .success());
}

fn canon(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn common_dir_key(repo: &Path) -> String {
    canon(&repo.join(".git"))
}

fn worktree_key(worktree: &Path) -> String {
    canon(worktree)
}

/// The (start, end) block of `repo`'s worktree entry in the registry.
fn block_of(env: &TestEnv, repo: &Path) -> (u16, u16) {
    let block = env.registry()["projects"][common_dir_key(repo)]["worktrees"][worktree_key(repo)]
        ["block"]
        .clone();
    (
        block[0].as_u64().unwrap() as u16,
        block[1].as_u64().unwrap() as u16,
    )
}

/// 共通シナリオ: repo を作って sync → fault を仕込んで reallocate（必ず
/// 二相 move が走る）→ abort を確認 → fault 無しで sync → 完全回復を検証。
fn crash_then_recover(fault: &str) {
    let env = TestEnv::new();
    env.write_config("range = [19100, 19199]\n");
    let repo = env.path("crash");
    init_repo(&repo);
    let out = env.run(&repo, &["sync"]);
    assert!(out.status.success());
    let before = env.registry();

    let out = env.run_with_fault(&repo, &["reallocate"], fault);
    assert!(
        !out.status.success(),
        "{fault}: the armed binary must abort, got {:?}",
        out.status
    );

    // Recovery: a plain sync must resolve any pending state.
    let out = env.run(&repo, &["sync"]);
    assert!(out.status.success(), "{fault}: recovery sync failed");

    // Invariants after recovery:
    let registry = env.registry();
    // 1. check passes (validate: no overlaps, well-formed).
    let out = env.run(&repo, &["check"]);
    assert!(out.status.success(), "{fault}: check failed after recovery");
    // 2. no pending move survives.
    for project in registry["projects"].as_object().unwrap().values() {
        for entry in project["worktrees"].as_object().unwrap().values() {
            assert!(
                entry["pending_block"].is_null(),
                "{fault}: pending must be resolved"
            );
        }
    }
    // 3. env matches the ledger block.
    let entry_block = block_of(&env, &repo);
    let env_text = fs::read_to_string(repo.join(".env.portool")).unwrap();
    let header_block = env_text
        .lines()
        .find_map(|l| l.strip_prefix("# block: "))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("env header has a block");
    assert_eq!(
        header_block,
        format!("{}-{}", entry_block.0, entry_block.1),
        "{fault}: env and ledger must agree after recovery"
    );
    // 4. the backup is itself a loadable ledger (atomic backup guarantee).
    let bak = env.registry_path().with_file_name("registry.json.bak");
    let bak_text = fs::read_to_string(&bak).expect("backup exists");
    serde_json::from_str::<serde_json::Value>(&bak_text).expect("backup parses as JSON");
    let _ = before; // silence if unused
}

#[test]
fn recovers_from_a_crash_after_the_pending_save() {
    crash_then_recover("after_pending_save");
}

#[test]
fn recovers_from_a_crash_before_the_env_write() {
    crash_then_recover("before_env_write");
}

#[test]
fn recovers_from_a_crash_after_the_env_write() {
    crash_then_recover("after_env_write");
}

#[test]
fn recovers_from_a_crash_after_the_registry_write() {
    crash_then_recover("after_registry_write");
}

#[test]
fn recovers_from_a_crash_during_the_backup_rename() {
    crash_then_recover("during_backup");
}

/// during_backup 固有: 旧バックアップが部分破壊されないこと。
#[test]
fn a_backup_crash_leaves_the_previous_backup_intact() {
    let env = TestEnv::new();
    env.write_config("range = [19100, 19199]\n");
    let repo = env.path("bak");
    init_repo(&repo);
    // sync で backup 生成 → その内容を控える
    let out = env.run(&repo, &["sync"]);
    assert!(out.status.success());
    let bak = env.registry_path().with_file_name("registry.json.bak");
    let before = fs::read(&bak).unwrap();
    // during_backup で abort（最初の save = pending save のバックアップで死ぬ）
    let out = env.run_with_fault(&repo, &["reallocate"], "during_backup");
    assert!(!out.status.success());
    assert_eq!(
        fs::read(&bak).unwrap(),
        before,
        "a crash before the backup rename must leave the old backup untouched"
    );
}
