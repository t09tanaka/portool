//! E2E for the P0-2 stale-backup recovery contract and the P0-4 pending-move
//! surfacing. Helpers are copied (not shared) per this repo's convention that
//! integration test binaries cannot share code.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct TestEnv {
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    config: PathBuf,
    _tmp: TempDir,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let config = tmp.path().join("config");
        let root = tmp.path().join("root");
        for d in [&home, &state, &config, &root] {
            std::fs::create_dir_all(d).unwrap();
        }
        TestEnv {
            root,
            home,
            state,
            config,
            _tmp: tmp,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_portool"));
        cmd.env_clear();
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        cmd.env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        cmd
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.command().current_dir(dir).args(args).output().unwrap()
    }

    fn run_with_fault(&self, dir: &Path, args: &[&str], fault: &str) -> Output {
        self.command()
            .current_dir(dir)
            .env("PORTOOL_TEST_FAULT", fault)
            .args(args)
            .output()
            .unwrap()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn registry_path(&self) -> PathBuf {
        self.state.join("portool").join("registry.json")
    }

    fn write_config(&self, contents: &str) {
        let dir = self.config.join("portool");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), contents).unwrap();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed");
}

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "a@b.c"]);
    git(dir, &["config", "user.name", "a"]);
    std::fs::write(dir.join("README.md"), "x\n").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).expect("valid JSON")
}

fn env_block(worktree: &Path) -> (u16, u16) {
    let contents = std::fs::read_to_string(worktree.join(".env.portool")).unwrap();
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("# block: ") {
            let tok = rest.split_whitespace().next().unwrap();
            let (a, b) = tok.split_once('-').unwrap();
            return (a.parse().unwrap(), b.parse().unwrap());
        }
    }
    panic!("no block header");
}

/// P0-4: an interrupted two-phase move leaves `pending_block` set; `ls --json`
/// must expose it (and the derived `state`/`sync_required`), never report a
/// clean state.
#[test]
fn ls_json_exposes_pending_block_and_state() {
    let env = TestEnv::new();
    env.write_config("range = [19600, 19649]\n");
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    // Crash a reallocate right after the pending save, before finalize: the
    // ledger keeps a pending_block.
    let out = env.run_with_fault(&repo, &["reallocate"], "after_pending_save");
    assert!(!out.status.success(), "the faulted reallocate must abort");

    let ls = env.run(&repo, &["ls", "--json"]);
    assert!(ls.status.success());
    let v = json(&ls);
    let alloc = &v["allocations"][0];
    assert!(
        alloc["pending_block"].is_array(),
        "pending_block must be surfaced: {alloc}"
    );
    assert_eq!(alloc["state"], "pending_move");
    assert_eq!(alloc["sync_required"], serde_json::json!(true));
}

/// P0-2: after a corrupt ledger is restored from its backup, both projects'
/// allocations survive -- the backup carried them, and doctor reconciles the
/// current project's env blocks too. Neither project loses its block.
#[test]
fn repair_from_backup_keeps_both_projects() {
    let env = TestEnv::new();
    env.write_config("range = [19500, 19559]\n");
    let a = env.path("a");
    let b = env.path("b");
    init_repo(&a);
    init_repo(&b);
    assert!(env.run(&a, &["sync"]).status.success());
    assert!(env.run(&b, &["sync"]).status.success());
    let a_block = env_block(&a);
    let b_block = env_block(&b);
    assert_ne!(a_block, b_block);

    // The backup now holds both A and B. Corrupt the main ledger.
    std::fs::write(env.registry_path(), b"{ corrupt not json").unwrap();

    // Repair from B restores the whole ledger from backup (A and B both).
    let out = env.run(&b, &["doctor", "--repair"]);
    assert!(
        out.status.success(),
        "doctor --repair must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let ls = env.run(&a, &["ls", "--all", "--json"]);
    let v = json(&ls);
    let blocks: Vec<(u16, u16)> = v["allocations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|al| {
            let bl = al["block"].as_array().unwrap();
            (
                bl[0].as_u64().unwrap() as u16,
                bl[1].as_u64().unwrap() as u16,
            )
        })
        .collect();
    assert!(
        blocks.contains(&a_block),
        "A's block lost in recovery: {blocks:?}"
    );
    assert!(
        blocks.contains(&b_block),
        "B's block lost in recovery: {blocks:?}"
    );
}

/// P0-2 quarantine: a tracked worktree whose env records a newer sequence than
/// a rolled-back ledger blocks new allocation until `doctor --repair`
/// reconciles.
#[test]
fn stale_backup_rollback_quarantines_new_allocation() {
    let env = TestEnv::new();
    env.write_config("range = [19500, 19559]\n");
    let a = env.path("qa");
    init_repo(&a);
    assert!(env.run(&a, &["sync"]).status.success());

    // Advance the ledger a few times so its sequence climbs above the env's.
    for _ in 0..3 {
        assert!(env.run(&a, &["reallocate"]).status.success());
    }
    // The env now records a high sequence. Roll the *ledger* back to a stale
    // snapshot with a low sequence but the same tracked worktree.
    let mut ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.registry_path()).unwrap()).unwrap();
    ledger["sequence"] = serde_json::json!(0);
    std::fs::write(
        env.registry_path(),
        serde_json::to_vec_pretty(&ledger).unwrap(),
    )
    .unwrap();
    // Keep the backup consistent with the rolled-back ledger so this is a
    // pure "ledger regressed below a tracked env" scenario.
    std::fs::write(
        env.state.join("portool").join("registry.json.bak"),
        serde_json::to_vec_pretty(&ledger).unwrap(),
    )
    .unwrap();

    // A fresh worktree's sync must refuse (quarantine), pointing at repair.
    let b = env.path("qb");
    git(&a, &["branch", "feat"]);
    git(&a, &["worktree", "add", b.to_str().unwrap(), "feat"]);
    let out = env.run(&b, &["sync"]);
    assert!(
        !out.status.success(),
        "sync must be quarantined after rollback"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("stale backup"),
        "must point at recovery: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
