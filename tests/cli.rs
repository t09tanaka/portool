//! End-to-end tests for the `portool` binary (spec §7-§9).
//!
//! Every test gets its own [`TestEnv`]: a temp directory supplying
//! `HOME`/`XDG_STATE_HOME`/`XDG_CONFIG_HOME` to the spawned binary via
//! [`Command::env`] (never via `std::env::set_var`, which would leak into
//! this process and every other test). The real `~/.local/state` is never
//! touched.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;
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

    /// A not-yet-created path under this test's scratch root.
    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn registry_path(&self) -> PathBuf {
        self.state.join("portool").join("registry.json")
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

// --- 1. sync outside a git repository -----------------------------------

#[test]
fn sync_outside_git_repo_exits_1() {
    let env = TestEnv::new();
    let dir = env.path("not-a-repo");
    fs::create_dir_all(&dir).unwrap();

    let output = env.run(&dir, &["sync"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("portool: error: "),
        "stderr was: {stderr}"
    );
}

// --- 2. manifest-less repo: default PORT block ---------------------------

#[test]
fn sync_without_manifest_allocates_default_block() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let output = env.run(&repo, &["sync"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let env_file = fs::read_to_string(repo.join(".env.portool")).unwrap();
    assert_eq!(
        env_file,
        format!(
            "# generated by portool \u{2014} DO NOT EDIT\n\
             # block: 3000-3004  project: repo  worktree: {}\n\
             PORT=3000\n",
            worktree_key(&repo)
        )
    );

    let registry = env.registry();
    let block =
        &registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"];
    assert_eq!(block, &serde_json::json!([3000, 3004]));
}

// --- 3. manifest with 4 ports ---------------------------------------------

#[test]
fn sync_with_manifest_renders_all_declared_ports() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(
        repo.join(".portool.toml"),
        "[ports]\nweb = 0\napi = 1\nhmr = 2\ndb = 3\n",
    )
    .unwrap();

    let output = env.run(&repo, &["sync"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let env_file = fs::read_to_string(repo.join(".env.portool")).unwrap();
    assert!(env_file.contains("WEB_PORT=3000"));
    assert!(env_file.contains("API_PORT=3001"));
    assert!(env_file.contains("HMR_PORT=3002"));
    assert!(env_file.contains("DB_PORT=3003"));
    assert_eq!(env_file.lines().filter(|l| l.contains("_PORT=")).count(), 4);

    let registry = env.registry();
    let block =
        &registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"];
    assert_eq!(block, &serde_json::json!([3000, 3004]));
}

// --- 4. second sync is a pure no-op ---------------------------------------

#[test]
fn second_sync_is_a_pure_noop() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    assert!(env.run(&repo, &["sync"]).status.success());

    let registry_mtime_1 = fs::metadata(env.registry_path())
        .unwrap()
        .modified()
        .unwrap();
    let env_mtime_1 = fs::metadata(repo.join(".env.portool"))
        .unwrap()
        .modified()
        .unwrap();

    // mtime resolution can be coarse on some filesystems; sleep past it so
    // an unwanted write would definitely be observable.
    std::thread::sleep(Duration::from_millis(1100));

    let output = env.run(&repo, &["sync"]);
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "fast-path sync must not print anything: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );

    let registry_mtime_2 = fs::metadata(env.registry_path())
        .unwrap()
        .modified()
        .unwrap();
    let env_mtime_2 = fs::metadata(repo.join(".env.portool"))
        .unwrap()
        .modified()
        .unwrap();

    assert_eq!(
        registry_mtime_1, registry_mtime_2,
        "fast-path sync must not rewrite registry.json"
    );
    assert_eq!(
        env_mtime_1, env_mtime_2,
        "fast-path sync must not rewrite .env.portool"
    );
}

// --- 5. a second (linked) worktree gets a different block -----------------

#[test]
fn linked_worktree_gets_a_different_block_main_keeps_slot_zero() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    let wt = env.path("repo-wt");
    assert!(git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/api",
            wt.to_str().unwrap()
        ],
    )
    .status
    .success());

    let output = env.run(&wt, &["sync"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let registry = env.registry();
    let project = &registry["projects"][common_dir_key(&repo)];
    let main_block = &project["worktrees"][worktree_key(&repo)]["block"];
    let wt_block = &project["worktrees"][worktree_key(&wt)]["block"];

    assert_eq!(main_block, &serde_json::json!([3000, 3004]));
    assert_ne!(main_block, wt_block);
}

// --- 6/7. manifest resize: expand reallocates, shrink settles -------------

#[test]
fn manifest_expansion_reallocates_a_larger_block() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(
        repo.join(".portool.toml"),
        "[ports]\nweb = 0\napi = 1\nhmr = 2\ndb = 3\n",
    )
    .unwrap();
    assert!(env.run(&repo, &["sync"]).status.success());
    let registry = env.registry();
    assert_eq!(
        registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"],
        serde_json::json!([3000, 3004])
    );

    // Add offset 7: 5 declared ports, max offset 7 -> raw 8 -> rounds up to
    // block_align(5)'s next multiple, 10.
    fs::write(
        repo.join(".portool.toml"),
        "[ports]\nweb = 0\napi = 1\nhmr = 2\ndb = 3\nextra = 7\n",
    )
    .unwrap();

    let output = env.run(&repo, &["sync"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let registry = env.registry();
    let block =
        &registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"];
    assert_eq!(block, &serde_json::json!([3000, 3009]));

    let env_file = fs::read_to_string(repo.join(".env.portool")).unwrap();
    assert!(env_file.contains("EXTRA_PORT=3007"));
}

#[test]
fn manifest_shrink_keeps_the_block_and_updates_only_env() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(
        repo.join(".portool.toml"),
        "[ports]\nweb = 0\napi = 1\nhmr = 2\ndb = 3\nextra = 7\n",
    )
    .unwrap();
    assert!(env.run(&repo, &["sync"]).status.success());
    let registry = env.registry();
    assert_eq!(
        registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"],
        serde_json::json!([3000, 3009]),
        "precondition: the block should already be widened to 10"
    );

    // Shrink back down to 2 ports: block_size(5) still fits inside the
    // existing 10-wide block, so the block must be left in place.
    fs::write(repo.join(".portool.toml"), "[ports]\nweb = 0\napi = 1\n").unwrap();

    let output = env.run(&repo, &["sync"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let registry = env.registry();
    let block =
        &registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"];
    assert_eq!(
        block,
        &serde_json::json!([3000, 3009]),
        "block must be kept in place"
    );

    let env_file = fs::read_to_string(repo.join(".env.portool")).unwrap();
    assert!(env_file.contains("WEB_PORT=3000"));
    assert!(env_file.contains("API_PORT=3001"));
    assert!(!env_file.contains("HMR_PORT"));
    assert!(!env_file.contains("EXTRA_PORT"));
}

// --- 8. init: hook install, .gitignore, idempotency -----------------------

#[test]
fn init_installs_hook_and_gitignore_and_is_idempotent() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let output = env.run(&repo, &["init"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let hook_path = repo.join(".git/hooks/post-checkout");
    let expected_hook = "#!/bin/sh\n\
# installed by portool\n\
command -v portool >/dev/null 2>&1 && portool sync --quiet\n";
    let hook_content_1 = fs::read_to_string(&hook_path).unwrap();
    assert_eq!(hook_content_1, expected_hook);
    let mode = fs::metadata(&hook_path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "hook must be executable");

    let gitignore_1 = fs::read_to_string(repo.join(".gitignore")).unwrap();
    assert!(gitignore_1.lines().any(|l| l == ".env.portool"));

    // init also runs sync once.
    assert!(repo.join(".env.portool").exists());

    // Second init must be a no-op on both files' contents.
    let output = env.run(&repo, &["init"]);
    assert!(output.status.success());

    let hook_content_2 = fs::read_to_string(&hook_path).unwrap();
    assert_eq!(hook_content_2, hook_content_1);
    let gitignore_2 = fs::read_to_string(repo.join(".gitignore")).unwrap();
    assert_eq!(gitignore_2, gitignore_1);
    assert_eq!(
        gitignore_2.lines().filter(|l| *l == ".env.portool").count(),
        1,
        "the .gitignore line must not be duplicated"
    );
}

// --- 9. ls / ls --json ------------------------------------------------------

#[test]
fn ls_table_and_json_shapes() {
    let env = TestEnv::new();
    let repo_a = env.path("repo-a");
    init_repo(&repo_a);
    assert!(env.run(&repo_a, &["sync"]).status.success());

    let repo_b = env.path("repo-b");
    init_repo(&repo_b);
    assert!(env.run(&repo_b, &["sync"]).status.success());

    // Default: current project only.
    let output = env.run(&repo_a, &["ls"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let header = lines.next().unwrap();
    assert_eq!(
        header.split_whitespace().collect::<Vec<_>>(),
        vec!["PROJECT", "WORKTREE", "BRANCH", "BLOCK", "STATUS"]
    );
    let data_lines: Vec<&str> = lines.collect();
    assert_eq!(data_lines.len(), 1, "only repo-a's row should be shown");
    assert!(data_lines[0].contains("repo-a"));
    assert!(data_lines[0].contains("main"));
    assert!(data_lines[0].contains("3000-3004"));
    assert!(data_lines[0].contains("active"));
    assert!(!data_lines[0].contains("repo-b"));

    // --all: both projects.
    let output = env.run(&repo_a, &["ls", "--all"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("repo-a"));
    assert!(stdout.contains("repo-b"));

    // --json, current project only.
    let output = env.run(&repo_a, &["ls", "--json"]);
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
        .expect("ls --json must emit valid JSON");
    assert_eq!(json["version"], serde_json::json!(1));
    assert_eq!(json["range"], serde_json::json!([3000, 9999]));
    let projects = json["projects"].as_object().unwrap();
    assert_eq!(projects.len(), 1);
    assert!(projects.contains_key(&common_dir_key(&repo_a)));

    // --json --all: both projects.
    let output = env.run(&repo_a, &["ls", "--json", "--all"]);
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let projects = json["projects"].as_object().unwrap();
    assert_eq!(projects.len(), 2);

    // Outside a repo: `--all` is fine, plain `ls` is exit 1.
    let outside = env.path("outside");
    fs::create_dir_all(&outside).unwrap();
    let output = env.run(&outside, &["ls"]);
    assert_eq!(output.status.code(), Some(1));
    let output = env.run(&outside, &["ls", "--all", "--json"]);
    assert!(output.status.success());
}

// --- 10. deleted worktree is reclaimed by prune; --dry-run doesn't touch it -

#[test]
fn prune_reclaims_a_deleted_worktree_and_dry_run_does_not() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    let wt = env.path("repo-wt");
    assert!(git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap()
        ],
    )
    .status
    .success());
    assert!(env.run(&wt, &["sync"]).status.success());

    let wt_key = worktree_key(&wt);
    let registry = env.registry();
    assert!(
        registry["projects"][common_dir_key(&repo)]["worktrees"]
            .get(&wt_key)
            .is_some(),
        "precondition: the linked worktree must be registered"
    );

    // Simulate an out-of-band deletion, then let git notice it's gone so
    // `git worktree list --porcelain` stops reporting it (spec §8.1
    // condition 2 requires both).
    fs::remove_dir_all(&wt).unwrap();
    assert!(git(&repo, &["worktree", "prune"]).status.success());

    // --dry-run must report it but not touch the ledger.
    let output = env.run(&repo, &["prune", "--dry-run"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("would prune"), "stdout was: {stdout}");

    let registry = env.registry();
    assert!(
        registry["projects"][common_dir_key(&repo)]["worktrees"]
            .get(&wt_key)
            .is_some(),
        "--dry-run must not remove the entry"
    );

    // A real prune reclaims it.
    let output = env.run(&repo, &["prune"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("pruned"), "stdout was: {stdout}");

    let registry = env.registry();
    assert!(
        registry["projects"][common_dir_key(&repo)]["worktrees"]
            .get(&wt_key)
            .is_none(),
        "the real prune must remove the reclaimed entry"
    );
    // The main worktree's own entry must be untouched.
    assert!(registry["projects"][common_dir_key(&repo)]["worktrees"]
        .get(worktree_key(&repo))
        .is_some());
}

// --- 11. detached HEAD --------------------------------------------------

#[test]
fn detached_head_records_a_null_branch() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let head = git(&repo, &["rev-parse", "HEAD"]);
    let sha = String::from_utf8(head.stdout).unwrap();
    assert!(git(&repo, &["checkout", "-q", sha.trim()]).status.success());

    let output = env.run(&repo, &["sync"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let registry = env.registry();
    let branch =
        &registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["branch"];
    assert!(branch.is_null());
}
