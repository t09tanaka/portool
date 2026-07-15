//! Concurrency integration test (spec §11): spawns 8 real `portool sync`
//! processes truly concurrently -- one per worktree, from 8 different
//! worktrees of a single shared git repository -- all pointed at the same
//! (isolated) `XDG_STATE_HOME`, and verifies the resulting ledger is valid
//! JSON with 8 distinct, non-overlapping block allocations.
//!
//! Mirrors the isolation discipline established in `tests/cli.rs`: a
//! per-test temp `HOME`/`XDG_STATE_HOME`/`XDG_CONFIG_HOME`, an `env_clear`-ed
//! `Command`, and git invocations pinned away from the host's real
//! global/system config. The real `~/.local/state` is never touched.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use tempfile::TempDir;

/// An isolated `HOME` / `XDG_STATE_HOME` / `XDG_CONFIG_HOME` shared by every
/// `portool sync` process spawned in this test, plus a scratch area for the
/// git repository and its worktrees.
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
    /// isolated (`env_clear`-ed) environment -- identical shape to
    /// `tests/cli.rs`'s helper of the same name.
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

    fn registry_path(&self) -> PathBuf {
        self.state.join("portool").join("registry.json")
    }
}

/// Runs a git command isolated from the host machine's real global/system
/// config, mirroring `tests/cli.rs`'s own helper.
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

/// Sets up one shared repo with 8 worktrees (the main worktree plus 7
/// linked worktrees on distinct branches), spawns `portool sync` in every
/// one of them truly concurrently (spawn all 8 first, then wait on all 8),
/// and asserts: every process exits 0, the ledger ends up as valid JSON,
/// and the 8 resulting blocks are pairwise non-overlapping.
fn run_eight_concurrent_syncs() {
    let env = TestEnv::new();
    let repo = env.root.join("repo");
    init_repo(&repo);

    let mut worktrees: Vec<PathBuf> = vec![repo.clone()];
    for i in 0..7 {
        let wt = env.root.join(format!("repo-wt-{i}"));
        assert!(git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("feature/{i}"),
                wt.to_str().unwrap(),
            ],
        )
        .status
        .success());
        worktrees.push(wt);
    }
    assert_eq!(worktrees.len(), 8);

    // Spawn every `portool sync` first (no waiting in between), so all 8
    // run genuinely concurrently and actually contend for the registry
    // lock, then wait for all of them.
    let children: Vec<Child> = worktrees
        .iter()
        .map(|wt| {
            env.command()
                .current_dir(wt)
                .arg("sync")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("failed to spawn portool sync")
        })
        .collect();

    let outputs: Vec<Output> = children
        .into_iter()
        .map(|c| {
            c.wait_with_output()
                .expect("failed to wait for portool sync")
        })
        .collect();

    for (wt, output) in worktrees.iter().zip(outputs.iter()) {
        assert!(
            output.status.success(),
            "portool sync in {} exited with {:?}; stderr: {}",
            wt.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let contents = fs::read_to_string(env.registry_path()).expect("registry.json missing");
    let registry: serde_json::Value =
        serde_json::from_str(&contents).expect("registry.json is not valid JSON");

    let common_dir_key = canon(&repo.join(".git"));
    let worktrees_obj = registry["projects"][&common_dir_key]["worktrees"]
        .as_object()
        .expect("worktrees object missing from the ledger");

    assert_eq!(
        worktrees_obj.len(),
        8,
        "all 8 worktrees must be registered; registry: {registry:#}"
    );

    // Every worktree we created must actually be present under its own key.
    for wt in &worktrees {
        assert!(
            worktrees_obj.contains_key(&canon(wt)),
            "{} missing from the ledger",
            wt.display()
        );
    }

    let mut blocks: Vec<(u64, u64)> = worktrees_obj
        .values()
        .map(|w| {
            let block = w["block"].as_array().expect("block must be an array");
            (
                block[0].as_u64().expect("block start must be a number"),
                block[1].as_u64().expect("block end must be a number"),
            )
        })
        .collect();
    blocks.sort_unstable();

    assert_eq!(blocks.len(), 8);
    for &(start, end) in &blocks {
        assert!(start <= end, "block ({start}, {end}) has start > end");
    }
    for pair in blocks.windows(2) {
        let (prev_start, prev_end) = pair[0];
        let (next_start, next_end) = pair[1];
        assert!(
            prev_end < next_start,
            "blocks ({prev_start}, {prev_end}) and ({next_start}, {next_end}) overlap"
        );
    }
}

#[test]
fn eight_concurrent_syncs_get_distinct_non_overlapping_blocks() {
    run_eight_concurrent_syncs();
}
