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

    /// Like [`Self::run`], but with additional environment variables set on
    /// top of the isolated base environment -- for simulating a parent
    /// process (e.g. a git hook) that exports repo-pinning `GIT_*`
    /// variables into `portool`'s environment.
    fn run_with_env(&self, dir: &Path, args: &[&str], extra_env: &[(&str, &Path)]) -> Output {
        let mut cmd = self.command();
        cmd.current_dir(dir).args(args);
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        cmd.output().expect("failed to spawn portool")
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

/// Runs git with this test's fully isolated environment AND the portool
/// binary prepended to `PATH`, so hooks spawned by git (post-checkout)
/// can themselves run `portool sync` against the test's isolated state.
fn git_with_portool(env: &TestEnv, dir: &Path, args: &[&str]) -> Output {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_portool"));
    let bin_dir = bin.parent().expect("binary has a parent dir");
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .env_clear()
        .env("PATH", path)
        .env("HOME", &env.home)
        .env("XDG_STATE_HOME", &env.state)
        .env("XDG_CONFIG_HOME", &env.config)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.output().expect("failed to run git")
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

/// Reads a `"block": [start, end]` JSON value as a `(u16, u16)` tuple.
fn block_tuple(block: &serde_json::Value) -> (u16, u16) {
    (
        block[0].as_u64().unwrap() as u16,
        block[1].as_u64().unwrap() as u16,
    )
}

/// Extracts the `PORTOOL_PROJECT_ID` / `PORTOOL_WORKTREE_ID` values from a
/// worktree's `.env.portool`.
fn read_ids(worktree: &Path) -> (String, String) {
    let contents = fs::read_to_string(worktree.join(".env.portool")).expect(".env.portool missing");
    let value_of = |key: &str| {
        contents
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_else(|| panic!("{key} line missing in: {contents}"))
            .to_string()
    };
    (
        value_of("PORTOOL_PROJECT_ID="),
        value_of("PORTOOL_WORKTREE_ID="),
    )
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

/// P1 (external review v0.9): `discover`'s rev-parse failure must keep the
/// legacy "not inside a git repository" wording (existing behavior other
/// tests/users depend on) while also surfacing git's own stderr, instead of
/// collapsing every possible failure into that one generic sentence.
#[test]
fn discover_error_reports_git_stderr() {
    let env = TestEnv::new();
    let dir = env.path("not-a-repo");
    fs::create_dir_all(&dir).unwrap();

    let output = env.run(&dir, &["sync"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not inside a git repository"),
        "must keep the legacy wording, stderr was: {stderr}"
    );
    assert!(
        stderr.contains("not a git repository"),
        "must surface git's real stderr, not just the generic sentence, stderr was: {stderr}"
    );
}

/// P1 (external review v0.9): a parent git process (most notably a git
/// hook, per githooks(5)) exports repo-pinning `GIT_DIR`/`GIT_WORK_TREE` in
/// its own environment. If `portool` (spawned from inside that hook)
/// inherits them, `git -C <repo-a>` would silently operate on repo B
/// instead -- every git spawn must scrub those variables so `-C` is the
/// sole source of truth.
#[test]
fn git_dir_environment_does_not_override_dash_c_target() {
    let env = TestEnv::new();
    let repo_a = env.path("repo-a");
    init_repo(&repo_a);
    let repo_b = env.path("repo-b");
    init_repo(&repo_b);

    let output = env.run_with_env(
        &repo_a,
        &["sync"],
        &[
            ("GIT_DIR", &repo_b.join(".git")),
            ("GIT_WORK_TREE", &repo_b),
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // A clean run (no inherited GIT_* vars) must see repo A's allocation --
    // if the bug were present, `sync` would have written repo B's identity
    // into the registry instead.
    let ls = env.run(&repo_a, &["ls", "--json"]);
    assert!(ls.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&ls.stdout))
        .expect("ls --json must emit valid JSON");
    let allocations = json["allocations"].as_array().unwrap();
    assert_eq!(allocations.len(), 1, "allocations: {allocations:?}");
    assert_eq!(
        allocations[0]["project_key"],
        serde_json::json!(common_dir_key(&repo_a))
    );
    assert_eq!(
        allocations[0]["path"],
        serde_json::json!(worktree_key(&repo_a))
    );

    // And repo B, which was never actually touched by the sync above, must
    // have no allocation of its own.
    let registry = env.registry();
    assert!(
        registry["projects"].get(common_dir_key(&repo_b)).is_none(),
        "repo B must not have been allocated a block: {registry:?}"
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

    // Task 8: main no longer special-cases to the pool start -- it hashes
    // like any other branch -- so the block is read back instead of assumed.
    let registry = env.registry();
    let block =
        &registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"];
    let start = block[0].as_u64().unwrap() as u16;
    let end = block[1].as_u64().unwrap() as u16;
    assert!(
        start >= 3000 && end <= 9999 && end - start == 4,
        "block ({start}, {end}) must be a 5-wide block inside the default pool"
    );

    let env_file = fs::read_to_string(repo.join(".env.portool")).unwrap();
    let common_dir = fs::canonicalize(repo.join(".git")).unwrap();
    let worktree_root = fs::canonicalize(&repo).unwrap();
    assert_eq!(
        env_file,
        format!(
            "# generated by portool \u{2014} DO NOT EDIT\n\
             # block: {start}-{end}  generation: 1  project: repo  worktree: {}\n\
             PORTOOL_PROJECT_ID={}\n\
             PORTOOL_WORKTREE_ID={}\n\
             PORT={start}\n",
            worktree_key(&repo),
            portool::identity::project_id(&common_dir),
            portool::identity::worktree_id(&common_dir, &worktree_root),
        )
    );
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

    let registry = env.registry();
    let block =
        &registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"];
    let start = block[0].as_u64().unwrap() as u16;
    let end = block[1].as_u64().unwrap() as u16;
    assert!(
        start >= 3000 && end <= 9999 && end - start == 4,
        "block ({start}, {end}) must be a 5-wide block inside the default pool"
    );

    let env_file = fs::read_to_string(repo.join(".env.portool")).unwrap();
    assert!(env_file.contains(&format!("WEB_PORT={start}")));
    assert!(env_file.contains(&format!("API_PORT={}", start + 1)));
    assert!(env_file.contains(&format!("HMR_PORT={}", start + 2)));
    assert!(env_file.contains(&format!("DB_PORT={}", start + 3)));
    assert_eq!(env_file.lines().filter(|l| l.contains("_PORT=")).count(), 4);
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

/// Task 8: `main` no longer special-cases to the pool start -- both blocks
/// only need to land inside the pool and stay clear of each other.
#[test]
fn linked_worktree_gets_a_different_block() {
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
    let main_block = block_tuple(&project["worktrees"][worktree_key(&repo)]["block"]);
    let wt_block = block_tuple(&project["worktrees"][worktree_key(&wt)]["block"]);

    for block in [main_block, wt_block] {
        assert!(
            block.0 >= 3000 && block.1 <= 9999,
            "block {block:?} must be within the default pool"
        );
    }
    assert_ne!(main_block, wt_block);
    assert!(
        !portool::registry::overlaps(main_block, wt_block),
        "the two worktrees' blocks must not overlap"
    );
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
    let initial_block = block_tuple(
        &registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"],
    );
    assert_eq!(
        initial_block.1 - initial_block.0,
        4,
        "initial block must be 5-wide"
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
    let block = block_tuple(
        &registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"],
    );
    assert_eq!(block.1 - block.0, 9, "expansion must widen the block to 10");
    assert!(
        block.0 >= 3000 && block.1 <= 9999,
        "block {block:?} must be within the default pool"
    );

    let env_file = fs::read_to_string(repo.join(".env.portool")).unwrap();
    assert!(env_file.contains(&format!("EXTRA_PORT={}", block.0 + 7)));
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
    let widened_block = block_tuple(
        &registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"],
    );
    assert_eq!(
        widened_block.1 - widened_block.0,
        9,
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
    let block = block_tuple(
        &registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"],
    );
    assert_eq!(block, widened_block, "block must be kept in place");

    let env_file = fs::read_to_string(repo.join(".env.portool")).unwrap();
    assert!(env_file.contains(&format!("WEB_PORT={}", widened_block.0)));
    assert!(env_file.contains(&format!("API_PORT={}", widened_block.0 + 1)));
    assert!(!env_file.contains("HMR_PORT"));
    assert!(!env_file.contains("EXTRA_PORT"));
}

// --- 8. init: hook install, info/exclude, idempotency ----------------------

#[test]
fn init_installs_hook_and_exclude_and_is_idempotent() {
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
    let hook_content_1 = fs::read_to_string(&hook_path).unwrap();
    assert!(hook_content_1.starts_with("#!/bin/sh\n# installed by portool\n"));
    assert!(portool::hooks::contains_portool_invocation(&hook_content_1));
    assert!(
        hook_content_1.contains("|| echo 'portool: sync failed; Git was not blocked' >&2"),
        "must report sync failure without propagating it"
    );
    assert!(hook_content_1.trim_end().ends_with("exit 0"));
    // Task 5: the hook embeds the running binary's absolute, canonicalized
    // path -- so it still works from a GUI client whose PATH lacks it.
    let expected_bin = fs::canonicalize(env!("CARGO_BIN_EXE_portool")).unwrap();
    assert!(
        hook_content_1.contains(&format!("PORTOOL_BIN=\"{}\"", expected_bin.display())),
        "hook must embed the absolute portool binary path, got: {hook_content_1}"
    );
    let mode = fs::metadata(&hook_path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "hook must be executable");

    // Batch A #5: post-merge is installed alongside post-checkout.
    let post_merge = repo.join(".git/hooks/post-merge");
    assert_eq!(fs::read_to_string(&post_merge).unwrap(), hook_content_1);
    assert_eq!(
        fs::metadata(&post_merge).unwrap().permissions().mode() & 0o777,
        0o755,
        "post-merge hook must be executable"
    );

    // Task 6: init no longer touches the tracked .gitignore -- it writes a
    // managed pair to $GIT_COMMON_DIR/info/exclude instead.
    assert!(
        !repo.join(".gitignore").exists(),
        "init must not create/modify the tracked .gitignore"
    );
    let exclude_1 = fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
    assert!(exclude_1.contains("# managed by portool"));
    assert!(exclude_1.lines().any(|l| l == ".env.portool"));

    // init also runs sync once.
    assert!(repo.join(".env.portool").exists());

    // .env.portool is actually ignored via info/exclude.
    let status = git(&repo, &["status", "--porcelain"]);
    assert!(
        !String::from_utf8_lossy(&status.stdout).contains(".env.portool"),
        ".env.portool must be ignored via info/exclude"
    );

    // Second init must be a no-op on both files' contents.
    let output = env.run(&repo, &["init"]);
    assert!(output.status.success());

    let hook_content_2 = fs::read_to_string(&hook_path).unwrap();
    assert_eq!(hook_content_2, hook_content_1);
    let exclude_2 = fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude_2, exclude_1);
    assert_eq!(
        exclude_2.lines().filter(|l| *l == ".env.portool").count(),
        1,
        "the info/exclude line must not be duplicated"
    );
}

// --- 8b. init: core.hooksPath / Husky support ------------------------------

/// A plain repo (no core.hooksPath): `git worktree add` runs the installed
/// post-checkout hook, so the brand-new worktree gets its `.env.portool`
/// with no manual sync.
#[test]
fn worktree_add_first_checkout_generates_env_via_hook() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["init"]).status.success());

    let wt = env.path("repo-wt");
    let output = git_with_portool(
        &env,
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        wt.join(".env.portool").exists(),
        "the post-checkout hook must have synced the new worktree"
    );
}

#[test]
fn init_with_custom_hookspath_installs_there_not_git_hooks() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::create_dir_all(repo.join("ci-hooks")).unwrap();
    assert!(git(&repo, &["config", "core.hooksPath", "ci-hooks"])
        .status
        .success());

    let output = env.run(&repo, &["init"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let hook_path = repo.join("ci-hooks/post-checkout");
    let content_1 = fs::read_to_string(&hook_path).unwrap();
    assert!(portool::hooks::contains_portool_invocation(&content_1));
    let mode = fs::metadata(&hook_path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "hook must be executable");
    assert!(
        !repo.join(".git/hooks/post-checkout").exists(),
        "the unused default location must be left alone"
    );

    // Idempotent across re-runs.
    assert!(env.run(&repo, &["init"]).status.success());
    assert_eq!(fs::read_to_string(&hook_path).unwrap(), content_1);

    // The hook actually fires from the custom location.
    fs::remove_file(repo.join(".env.portool")).unwrap();
    let output = git_with_portool(&env, &repo, &["checkout", "-q", "-b", "feature"]);
    assert!(output.status.success());
    assert!(repo.join(".env.portool").exists());
}

#[test]
fn init_with_custom_hookspath_appends_to_existing_hook() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::create_dir_all(repo.join("ci-hooks")).unwrap();
    fs::write(
        repo.join("ci-hooks/post-checkout"),
        "#!/bin/sh\necho existing\n",
    )
    .unwrap();
    assert!(git(&repo, &["config", "core.hooksPath", "ci-hooks"])
        .status
        .success());

    assert!(env.run(&repo, &["init"]).status.success());
    assert!(env.run(&repo, &["init"]).status.success());

    let content = fs::read_to_string(repo.join("ci-hooks/post-checkout")).unwrap();
    assert!(
        content.starts_with("#!/bin/sh\n# >>> portool >>>\n"),
        "the managed block must be inserted right after the shebang, not appended at EOF, got: {content}"
    );
    assert!(
        content.ends_with("echo existing\n"),
        "the pre-existing hook body must be preserved, got: {content}"
    );
    assert!(portool::hooks::contains_portool_invocation(&content));
    assert_eq!(
        content.matches(portool::hooks::HOOK_BLOCK_BEGIN).count(),
        1,
        "re-running init must not duplicate the managed block"
    );
}

/// Regression (external review): a pre-existing hook ending in a top-level
/// `exit 0` used to swallow portool's block, since it was appended at EOF --
/// unreachable code after the `exit`. The block must be inserted right after
/// the shebang instead, so it still runs.
#[test]
fn existing_hook_exit_before_managed_block_regression() {
    let env = TestEnv::new();
    let repo = env.path("app");
    init_repo(&repo);
    let hooks_dir = repo.join(".git/hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::write(hooks_dir.join("post-checkout"), "#!/bin/sh\nexit 0\n").unwrap();

    assert!(env.run(&repo, &["init"]).status.success());

    let content = fs::read_to_string(hooks_dir.join("post-checkout")).unwrap();
    let block_pos = content
        .find(portool::hooks::HOOK_BLOCK_BEGIN)
        .expect("managed block must be present");
    let exit_pos = content
        .find("exit 0")
        .expect("the original 'exit 0' must be preserved");
    assert!(
        block_pos < exit_pos,
        "the managed block must sit before the pre-existing 'exit 0', got: {content}"
    );

    // The hook must actually fire despite the trailing 'exit 0': remove
    // .env.portool, then let a real `git checkout` re-run the hook.
    fs::remove_file(repo.join(".env.portool")).unwrap();
    let output = git_with_portool(&env, &repo, &["checkout", "-q", "-b", "feature"]);
    assert!(output.status.success());
    assert!(
        repo.join(".env.portool").exists(),
        "sync must have run via the post-checkout hook"
    );
}

/// External review: a hook location that resolves to a symlink means
/// nothing was actually installed there; `init` must not exit 0 as if it had
/// succeeded, even though the exclude update and `sync` already ran.
#[test]
fn init_exits_nonzero_when_hook_cannot_be_installed() {
    let env = TestEnv::new();
    let repo = env.path("app");
    init_repo(&repo);
    let hooks_dir = repo.join(".git/hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    std::os::unix::fs::symlink("/nonexistent", hooks_dir.join("post-checkout")).unwrap();

    let out = env.run(&repo, &["init"]);
    assert!(
        !out.status.success(),
        "init must fail when the post-checkout hook could not be installed"
    );
    assert!(
        repo.join(".env.portool").exists(),
        "the exclude update and sync must still have run before the hook failure is reported"
    );
}

/// External review: a hook that can't be read (e.g. non-UTF-8 content) used
/// to make `unhook` silently do nothing while still exiting 0.
#[test]
fn unhook_unreadable_hook_exits_nonzero() {
    let env = TestEnv::new();
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["init"]).status.success());

    fs::write(repo.join(".git/hooks/post-checkout"), [0xFF, 0xFE]).unwrap();

    let out = env.run(&repo, &["unhook"]);
    assert!(
        !out.status.success(),
        "unhook must fail when a hook can't be read, not silently succeed while doing nothing"
    );
}

/// Batch A #4: an absolute `core.hooksPath` configured in *global* scope is a
/// hooks dir shared across unrelated repos; `init` must refuse to auto-install
/// there (it would run portool's hook on every repo's checkout) and print the
/// manual line instead.
#[test]
fn init_refuses_global_scope_shared_hookspath() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let shared = env.path("shared-hooks");
    fs::create_dir_all(&shared).unwrap();
    let global_config = env.path("gitconfig-global");
    fs::write(
        &global_config,
        format!("[core]\n\thooksPath = {}\n", shared.display()),
    )
    .unwrap();

    // Run with GIT_CONFIG_GLOBAL pointing at that file, so core.hooksPath
    // resolves in `global` scope.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_portool"));
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("HOME", &env.home)
        .env("XDG_STATE_HOME", &env.state)
        .env("XDG_CONFIG_HOME", &env.config)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .current_dir(&repo)
        .args(["init", "--hook-only"]);
    let output = cmd.output().expect("failed to spawn portool");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // External review P1-4: "installed nothing" must not look like success.
    assert!(
        !output.status.success(),
        "init must fail when no hook location is installable, stderr: {stderr}"
    );
    assert!(
        stderr.contains("outside this repository") && stderr.contains("global"),
        "must warn about the global-scope hooksPath resolving outside the repo, got: {stderr}"
    );
    assert!(
        stderr.contains("portool sync --quiet || true"),
        "must print the safe manual line, got: {stderr}"
    );
    assert!(
        !shared.join("post-checkout").exists(),
        "must not write into a shared global hooks dir"
    );
    assert!(
        !repo.join(".git/hooks/post-checkout").exists(),
        "must not fall back to the ignored default hooks dir"
    );
}

/// External review P1 (v0.9.0): a *relative* `core.hooksPath` configured in
/// global scope (`../shared-hooks`) resolves against the worktree root, not
/// the config file's location -- so it can escape the repository just like
/// an absolute shared path. `init` must refuse it regardless of scope.
#[test]
fn relative_global_hooks_path_escaping_repo_is_refused() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let shared = env.path("shared-hooks");
    fs::create_dir_all(&shared).unwrap();
    let global_config = env.path("gitconfig-global");
    fs::write(&global_config, "[core]\n\thooksPath = ../shared-hooks\n").unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_portool"));
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("HOME", &env.home)
        .env("XDG_STATE_HOME", &env.state)
        .env("XDG_CONFIG_HOME", &env.config)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .current_dir(&repo)
        .args(["init", "--hook-only"]);
    let output = cmd.output().expect("failed to spawn portool");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "init must fail when core.hooksPath escapes the repository, stderr: {stderr}"
    );
    assert!(
        stderr.contains("outside"),
        "must warn that the resolved hooksPath is outside the repo, got: {stderr}"
    );
    assert!(
        stderr.contains("portool sync --quiet || true"),
        "must print the safe manual line, got: {stderr}"
    );
    assert!(
        !shared.join("post-checkout").exists(),
        "must not write into the repo's parent directory"
    );
    assert!(
        !repo.join(".git/hooks/post-checkout").exists(),
        "must not fall back to the ignored default hooks dir"
    );
}

/// External review P1 (v0.9.0): a `core.hooksPath` set at `command` scope
/// (highest-precedence `-c`/`GIT_CONFIG_*` env override, not `global` or
/// `system`) that resolves outside the repository must be refused too --
/// the old check only looked at `global`/`system` scope.
#[test]
fn command_scoped_hooks_path_is_refused() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let outside = env.path("outside-hooks");
    fs::create_dir_all(&outside).unwrap();

    let output = env
        .command()
        .current_dir(&repo)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", outside.to_str().unwrap())
        .args(["init", "--hook-only"])
        .output()
        .expect("failed to spawn portool");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "init must fail when a command-scoped core.hooksPath escapes the repository, stderr: {stderr}"
    );
    assert!(
        !outside.join("post-checkout").exists(),
        "must not write into a command-scoped hooksPath outside the repo"
    );
    assert!(
        !repo.join(".git/hooks/post-checkout").exists(),
        "must not fall back to the ignored default hooks dir"
    );
}

/// Batch A #2: `sync` hints to re-run `init` when the installed post-checkout
/// hook uses an old, unsafe form that can fail `git checkout`.
#[test]
fn sync_hints_when_hook_uses_unsafe_legacy_form() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join(".portool.toml"), "[ports]\nweb = 0\n").unwrap();

    // Plant a legacy unsafe hook (portool <= 0.2 shape) directly.
    let hook_dir = repo.join(".git/hooks");
    fs::create_dir_all(&hook_dir).unwrap();
    fs::write(
        hook_dir.join("post-checkout"),
        "#!/bin/sh\n# installed by portool\ncommand -v portool >/dev/null 2>&1 && portool sync --quiet\n",
    )
    .unwrap();

    let output = env.run(&repo, &["sync"]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("old form that can fail"),
        "sync must hint about the unsafe hook, got: {stderr}"
    );
}

/// A Husky-style repo: `core.hooksPath=.husky/_`, with a minimal stand-in
/// for Husky's generated bootstrap that delegates to the user-managed
/// `.husky/<hook>` (as Husky's `_/h` does). `portool init` must install
/// into `.husky/post-checkout` -- never into `.husky/_` or `.git/hooks`.
#[test]
fn init_with_husky_hookspath_installs_user_managed_hook_and_chains() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let husky_shim = "#!/bin/sh\nh=\"$(dirname \"$0\")/../post-checkout\"\n[ -f \"$h\" ] && sh -e \"$h\" \"$@\"\n";
    fs::create_dir_all(repo.join(".husky/_")).unwrap();
    fs::write(repo.join(".husky/_/post-checkout"), husky_shim).unwrap();
    let mut perms = fs::metadata(repo.join(".husky/_/post-checkout"))
        .unwrap()
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(repo.join(".husky/_/post-checkout"), perms).unwrap();
    assert!(git(&repo, &["config", "core.hooksPath", ".husky/_"])
        .status
        .success());

    let output = env.run(&repo, &["init"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Husky detected"),
        "init must explain the Husky integration, got: {stderr}"
    );

    let user_hook = fs::read_to_string(repo.join(".husky/post-checkout")).unwrap();
    assert!(portool::hooks::contains_portool_invocation(&user_hook));
    assert_eq!(
        fs::read_to_string(repo.join(".husky/_/post-checkout")).unwrap(),
        husky_shim,
        "Husky's generated runtime dir must be left untouched"
    );
    assert!(
        !repo.join(".git/hooks/post-checkout").exists(),
        "the unused default location must be left alone"
    );

    // Idempotent across re-runs.
    assert!(env.run(&repo, &["init"]).status.success());
    assert_eq!(
        fs::read_to_string(repo.join(".husky/post-checkout")).unwrap(),
        user_hook
    );

    // A checkout chains through the Husky-style bootstrap into portool.
    fs::remove_file(repo.join(".env.portool")).unwrap();
    let output = git_with_portool(&env, &repo, &["checkout", "-q", "-b", "feature"]);
    assert!(output.status.success());
    assert!(
        repo.join(".env.portool").exists(),
        "sync must run via .husky/_ -> .husky/post-checkout"
    );

    // And sync no longer nags about a missing hook.
    let output = env.run(&repo, &["sync"]);
    assert!(output.status.success());
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("hint: run 'portool init'"),
        "the hook-missing hint must respect core.hooksPath"
    );
}

/// The user-managed Husky hook exits 0 when portool isn't on PATH, even
/// under Husky's `sh -e` + exit-code propagation.
#[test]
fn husky_hook_is_harmless_when_portool_is_not_installed() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::create_dir_all(repo.join(".husky/_")).unwrap();
    assert!(git(&repo, &["config", "core.hooksPath", ".husky/_"])
        .status
        .success());
    assert!(env.run(&repo, &["init"]).status.success());

    let output = Command::new("/bin/sh")
        .arg("-e")
        .arg(repo.join(".husky/post-checkout"))
        .env_clear()
        .env("PATH", "/nonexistent") // no portool (and no git) available
        .output()
        .expect("failed to run sh");
    assert!(
        output.status.success(),
        "hook must exit 0 without portool, got {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `core.hooksPath` pointing at a directory that doesn't exist (and isn't
/// Husky's): init must warn with instructions instead of silently
/// installing somewhere git will never look.
#[test]
fn init_with_missing_hookspath_warns_and_installs_nothing() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(git(&repo, &["config", "core.hooksPath", "generated/hooks"])
        .status
        .success());

    let output = env.run(&repo, &["init"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // External review P1-4: "installed nothing" must not look like success.
    assert!(
        !output.status.success(),
        "init must fail when no hook location is installable, stderr: {stderr}"
    );
    assert!(
        stderr.contains("core.hooksPath") && stderr.contains("is not an existing directory"),
        "expected a warning about the unusable hooksPath, got: {stderr}"
    );
    assert!(
        stderr.contains("portool init --hook-only"),
        "the warning must include concrete recovery instructions, got: {stderr}"
    );
    assert!(!repo.join(".git/hooks/post-checkout").exists());
    assert!(!repo.join("generated").exists());
}

// --- 9. ls / ls --json ------------------------------------------------------

#[test]
fn ls_table_and_json_shapes() {
    let env = TestEnv::new();
    // An isolated range: this test asserts the exact block value, which
    // parallel tests' transient bind checks on the default 3000+ pool can
    // otherwise push off its expected slot.
    env.write_config("range = [4300, 4319]\nblock_align = 5\n");
    let repo_a = env.path("repo-a");
    init_repo(&repo_a);
    assert!(env.run(&repo_a, &["sync"]).status.success());

    let repo_b = env.path("repo-b");
    init_repo(&repo_b);
    assert!(env.run(&repo_b, &["sync"]).status.success());

    // Task 8: main no longer special-cases to the pool start, so the exact
    // block is read back rather than assumed.
    let registry = env.registry();
    let block_a = block_tuple(
        &registry["projects"][common_dir_key(&repo_a)]["worktrees"][worktree_key(&repo_a)]["block"],
    );
    assert!(
        block_a.0 >= 4300 && block_a.1 <= 4319,
        "block {block_a:?} must be in the pool"
    );

    // Default: current project only.
    let output = env.run(&repo_a, &["ls"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let header = lines.next().unwrap();
    assert_eq!(
        header.split_whitespace().collect::<Vec<_>>(),
        vec!["PROJECT", "WORKTREE", "BRANCH", "BLOCK", "STATUS", "LABEL"]
    );
    let data_lines: Vec<&str> = lines.collect();
    assert_eq!(data_lines.len(), 1, "only repo-a's row should be shown");
    assert!(data_lines[0].contains("repo-a"));
    assert!(data_lines[0].contains("main"));
    assert!(data_lines[0].contains(&format!("{}-{}", block_a.0, block_a.1)));
    assert!(data_lines[0].contains("active"));
    assert!(!data_lines[0].contains("repo-b"));

    // --all: both projects.
    let output = env.run(&repo_a, &["ls", "--all"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("repo-a"));
    assert!(stdout.contains("repo-b"));

    // --json, current project only. Task 10: a versioned envelope, not the
    // raw ledger shape.
    let output = env.run(&repo_a, &["ls", "--json"]);
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
        .expect("ls --json must emit valid JSON");
    assert_eq!(json["format_version"], serde_json::json!(1));
    assert_eq!(json["ok"], true);
    assert_eq!(json["registry_schema_version"], serde_json::json!(3));
    assert_eq!(
        json["effective_config"]["range"],
        serde_json::json!([4300, 4319])
    );
    let allocations = json["allocations"].as_array().unwrap();
    assert_eq!(allocations.len(), 1);
    assert_eq!(
        allocations[0]["project_key"],
        serde_json::json!(common_dir_key(&repo_a))
    );

    // --json --all: both projects.
    let output = env.run(&repo_a, &["ls", "--json", "--all"]);
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let allocations = json["allocations"].as_array().unwrap();
    assert_eq!(allocations.len(), 2);

    // Outside a repo: `--all` is fine, plain `ls` is exit 1.
    let outside = env.path("outside");
    fs::create_dir_all(&outside).unwrap();
    let output = env.run(&outside, &["ls"]);
    assert_eq!(output.status.code(), Some(1));
    let output = env.run(&outside, &["ls", "--all", "--json"]);
    assert!(output.status.success());
}

/// Task 10 (external review P1-7): `ls --json` emits a versioned envelope
/// decoupled from the ledger's storage schema, with derived `ports`.
#[test]
fn ls_json_emits_format_v1_envelope() {
    let env = TestEnv::new();
    env.write_config("range = [18300, 18399]\n");
    let repo = env.path("app");
    init_repo(&repo);
    fs::write(repo.join(".portool.toml"), "[ports]\nweb = 0\napi = 1\n").unwrap();
    assert!(env.run(&repo, &["sync"]).status.success());

    let out = env.run(&repo, &["ls", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["format_version"], 1);
    assert_eq!(v["ok"], true);
    assert_eq!(
        v["effective_config"]["range"],
        serde_json::json!([18300, 18399])
    );
    let alloc = &v["allocations"][0];
    assert_eq!(alloc["branch"], "main");
    assert_eq!(alloc["status"], "active");
    assert_eq!(alloc["ports"]["WEB_PORT"], alloc["block"][0]);
    assert!(alloc["project_id"].is_string());
    // Storage-schema internals must NOT leak.
    assert!(v.get("projects").is_none());
    assert!(alloc.get("pending_block").is_none());
}

/// Missing ledger -> `ok: true`, empty allocations, and the *real* config
/// (fixing the fabricated-default-range bug: it must not report
/// `Config::default()` when the caller configured a different pool).
#[test]
fn ls_json_missing_ledger_reports_real_config() {
    let env = TestEnv::new();
    env.write_config("range = [18400, 18499]\n");
    let repo = env.path("app");
    init_repo(&repo);

    let out = env.run(&repo, &["ls", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(
        v["effective_config"]["range"],
        serde_json::json!([18400, 18499]),
        "must reflect the real config, not Config::default()"
    );
    assert_eq!(v["allocations"].as_array().unwrap().len(), 0);
}

/// A corrupt ledger is the versioned error envelope, not the old bare
/// `{"error": ...}` shape.
#[test]
fn ls_json_corrupt_ledger_is_an_error_envelope() {
    let env = TestEnv::new();
    let repo = env.path("app");
    init_repo(&repo);
    fs::create_dir_all(env.registry_path().parent().unwrap()).unwrap();
    fs::write(env.registry_path(), "{ nope").unwrap();

    let out = env.run(&repo, &["ls", "--json"]);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["format_version"], 1);
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("corrupt"));
}

/// P2 (external review indication 14): `--json` is a machine interface, so
/// stdout must be the versioned error envelope on *every* failure path, not
/// just a corrupt ledger. Covers a broken config and a missing git repo
/// (both of which used to `?`-out before any JSON was ever printed).
#[test]
fn ls_json_wraps_config_and_git_errors() {
    let env = TestEnv::new();
    let repo = env.path("app");
    init_repo(&repo);
    env.write_config("range = \"oops\"\n");

    let out = env.run(&repo, &["ls", "--json"]);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout was not JSON: {e}; stdout: {:?}", out.stdout));
    assert_eq!(v["format_version"], 1);
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().is_some());

    let env2 = TestEnv::new();
    let dir = env2.path("not-a-repo");
    fs::create_dir_all(&dir).unwrap();

    let out2 = env2.run(&dir, &["ls", "--json"]);
    assert!(!out2.status.success());
    let v2: serde_json::Value = serde_json::from_slice(&out2.stdout)
        .unwrap_or_else(|e| panic!("stdout was not JSON: {e}; stdout: {:?}", out2.stdout));
    assert_eq!(v2["format_version"], 1);
    assert_eq!(v2["ok"], false);
    assert!(v2["error"].as_str().is_some());
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

// --- 12. hook-missing hint: exact stderr wording (frozen decision 10) -----

#[test]
fn sync_without_installed_hook_prints_the_exact_hint_line() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    // No `portool init` has been run, so the post-checkout hook is not
    // installed and sync must warn on stderr with this exact line (no
    // `portool: ` prefix -- that prefix is reserved for `error:`).
    let output = env.run(&repo, &["sync"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr,
        "hint: run 'portool init' to install the post-checkout hook\n"
    );
}

// --- 14. exit code 3: the pool has no room for even one block -------------
// (Batch C: exit code 2 / the per-project subrange model is retired; a block
// that can't fit anywhere in the pool now fails with PoolExhausted = 3.)

#[test]
fn sync_exits_3_when_pool_cannot_fit_a_block() {
    let env = TestEnv::new();
    // The pool is only 4 ports wide, narrower than the default 5-wide block:
    // no block fits anywhere in the pool.
    env.write_config("range = [3000, 3003]\nblock_align = 5\n");
    let repo = env.path("repo");
    init_repo(&repo);

    let output = env.run(&repo, &["sync"]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// --- 15. prune --all reclaims a fully-deleted project's entry -------------

#[test]
fn prune_all_reclaims_a_fully_deleted_project_and_leaves_others_untouched() {
    let env = TestEnv::new();

    let repo_a = env.path("repo-a");
    init_repo(&repo_a);
    assert!(env.run(&repo_a, &["sync"]).status.success());

    let repo_b = env.path("repo-b");
    init_repo(&repo_b);
    assert!(env.run(&repo_b, &["sync"]).status.success());

    // Keys must be captured before the directory is deleted below --
    // canonicalize needs the path to still exist.
    let repo_a_key = common_dir_key(&repo_a);
    let repo_b_key = common_dir_key(&repo_b);

    // Simulate the whole repo-a repository being deleted (not just a
    // worktree within it), which is what makes `prune --all`'s
    // project-entry-removal branch (as opposed to its per-worktree branch)
    // fire.
    fs::remove_dir_all(&repo_a).unwrap();

    // `prune --all` must work from outside any git repository.
    let outside = env.path("outside");
    fs::create_dir_all(&outside).unwrap();

    // --dry-run reports the dead project but must not touch the ledger.
    let output = env.run(&outside, &["prune", "--all", "--dry-run"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("would prune project repo-a ({repo_a_key})")),
        "stdout was: {stdout}"
    );

    let registry = env.registry();
    assert!(
        registry["projects"].get(&repo_a_key).is_some(),
        "--dry-run must not remove the dead project entry"
    );
    assert!(
        registry["projects"].get(&repo_b_key).is_some(),
        "--dry-run must not touch the surviving project either"
    );

    // A real `prune --all` removes the dead project entry while leaving the
    // surviving repo's entries alone.
    let output = env.run(&outside, &["prune", "--all"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("pruned project repo-a ({repo_a_key})")),
        "stdout was: {stdout}"
    );

    let registry = env.registry();
    assert!(
        registry["projects"].get(&repo_a_key).is_none(),
        "the dead project entry must be gone"
    );

    let repo_b_project = &registry["projects"][&repo_b_key];
    assert!(
        repo_b_project["worktrees"]
            .get(worktree_key(&repo_b))
            .is_some(),
        "the surviving project's worktree entry must be untouched"
    );
    // The v2 ledger no longer carries per-project subranges.
    assert!(
        repo_b_project.get("subranges").is_none(),
        "v2 ledger must not have a subranges field"
    );
}

/// External review P1 (Task 4): the pin contract says pinned entries are
/// exempt from every GC path -- including the `prune --all` branch that
/// handles a *whole repository* having vanished (not just one worktree).
/// A pinned worktree's entry, and therefore its project entry, must
/// survive even when `common_dir` no longer exists.
#[test]
fn prune_all_preserves_pinned_entries_when_common_dir_is_gone() {
    let env = TestEnv::new();
    env.write_config("range = [19400, 19409]\n");

    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    assert!(env.run(&repo, &["pin"]).status.success());

    let repo_key = common_dir_key(&repo);
    let wt_key = worktree_key(&repo);

    fs::remove_dir_all(&repo).unwrap();

    let outside = env.path("outside");
    fs::create_dir_all(&outside).unwrap();

    let output = env.run(&outside, &["prune", "--all"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let out = env.run(&outside, &["ls", "--json", "--all"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .expect("ls --json must emit valid JSON");
    let allocations = json["allocations"].as_array().unwrap();
    let pinned_alloc = allocations
        .iter()
        .find(|a| a["project_key"] == serde_json::json!(repo_key))
        .expect("the pinned project's entry must survive prune --all");
    assert_eq!(pinned_alloc["pinned"], serde_json::json!(true));

    let registry = env.registry();
    assert!(
        registry["projects"][&repo_key]["worktrees"]
            .get(&wt_key)
            .is_some(),
        "the pinned worktree entry must still be in the ledger"
    );
}

/// Companion regression check for the fix above: with no pin in place, the
/// pre-existing behavior (reclaim the whole project once its repository is
/// gone and its ports are free) must still hold.
#[test]
fn prune_all_reclaims_unpinned_when_common_dir_is_gone() {
    let env = TestEnv::new();
    env.write_config("range = [19400, 19409]\n");

    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    let repo_key = common_dir_key(&repo);

    fs::remove_dir_all(&repo).unwrap();

    let outside = env.path("outside");
    fs::create_dir_all(&outside).unwrap();

    let output = env.run(&outside, &["prune", "--all"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let registry = env.registry();
    assert!(
        registry["projects"].get(&repo_key).is_none(),
        "an unpinned project's entry must still be reclaimed once its repo is gone"
    );
}

/// P0-2 (external review): `prune --all` must NOT reclaim a project's
/// entries when `git worktree list` fails for it (e.g. the common-dir
/// path exists but is not a git repository). Enumeration failure is not
/// "zero worktrees".
#[test]
fn prune_all_keeps_entries_when_worktree_enumeration_fails() {
    let env = TestEnv::new();
    env.write_config("range = [17300, 17399]\n");

    // A fake project whose common_dir exists but is NOT a git repo, with
    // a worktree path that no longer exists and a block whose ports are
    // free -- the exact conditions under which the old fail-open code
    // would have dropped the entry.
    let fake_common = env.path("broken/.git");
    fs::create_dir_all(&fake_common).unwrap();
    let registry = serde_json::json!({
        "version": 3,
        "range": [17300, 17399],
        "projects": {
            fake_common.to_str().unwrap(): {
                "name": "broken",
                "worktrees": {
                    "/no/such/worktree": {
                        "block": [17300, 17304],
                        "generation": 1,
                        "pending_block": null,
                        "branch": "main",
                        "manifest_hash": null,
                        "pinned": false,
                        "label": null,
                        "allocated_at": "2026-07-15T10:00:00+09:00",
                        "last_seen_at": "2026-07-15T10:00:00+09:00"
                    }
                }
            }
        },
        "reservations": []
    });
    fs::create_dir_all(env.registry_path().parent().unwrap()).unwrap();
    fs::write(
        env.registry_path(),
        serde_json::to_string_pretty(&registry).unwrap(),
    )
    .unwrap();

    // Run prune --all from a real repo (prune --all doesn't need one, but
    // running from the scratch root is fine).
    let out = env.run(&env.root, &["prune", "--all"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("skipping project"),
        "must report the skipped project, got: {stderr}"
    );

    // The entry must still be there.
    let reg = env.registry();
    assert!(
        reg["projects"][fake_common.to_str().unwrap()]["worktrees"]["/no/such/worktree"]
            .is_object(),
        "enumeration failure must not reclaim the entry"
    );
}

// --- 16. worktree identity: stable PROJECT_ID / WORKTREE_ID ----------------

#[test]
fn linked_worktree_shares_project_id_but_not_worktree_id() {
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
    assert!(env.run(&wt, &["sync"]).status.success());

    let (main_project_id, main_worktree_id) = read_ids(&repo);
    let (wt_project_id, wt_worktree_id) = read_ids(&wt);

    assert_eq!(
        main_project_id, wt_project_id,
        "worktrees of the same project must share a PROJECT_ID"
    );
    assert_ne!(
        main_worktree_id, wt_worktree_id,
        "each worktree must have its own WORKTREE_ID"
    );
}

#[test]
fn worktree_id_survives_branch_checkout() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    let ids_before = read_ids(&repo);

    assert!(git(&repo, &["checkout", "-q", "-b", "feature/other"])
        .status
        .success());
    assert!(env.run(&repo, &["sync"]).status.success());

    assert_eq!(
        read_ids(&repo),
        ids_before,
        "IDs must not change on branch checkout"
    );
}

#[test]
fn worktree_id_survives_detached_head() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    let ids_before = read_ids(&repo);

    let head = git(&repo, &["rev-parse", "HEAD"]);
    let sha = String::from_utf8(head.stdout).unwrap();
    assert!(git(&repo, &["checkout", "-q", sha.trim()]).status.success());
    assert!(env.run(&repo, &["sync"]).status.success());

    assert_eq!(
        read_ids(&repo),
        ids_before,
        "IDs must not change on detached HEAD"
    );
}

#[test]
fn ids_survive_registry_deletion_and_resync() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    let ids_before = read_ids(&repo);

    fs::remove_file(env.registry_path()).unwrap();
    assert!(env.run(&repo, &["sync"]).status.success());

    assert_eq!(
        read_ids(&repo),
        ids_before,
        "IDs must not depend on the registry's contents"
    );
}

#[test]
fn ids_survive_manifest_expansion_reallocation() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(
        repo.join(".portool.toml"),
        "[ports]\nweb = 0\napi = 1\nhmr = 2\ndb = 3\n",
    )
    .unwrap();
    assert!(env.run(&repo, &["sync"]).status.success());
    let ids_before = read_ids(&repo);

    // Same expansion as test 6: max offset 7 forces a wider block, so the
    // worktree is reallocated -- the IDs must not move with the block.
    fs::write(
        repo.join(".portool.toml"),
        "[ports]\nweb = 0\napi = 1\nhmr = 2\ndb = 3\nextra = 7\n",
    )
    .unwrap();
    assert!(env.run(&repo, &["sync"]).status.success());

    let registry = env.registry();
    let block = block_tuple(
        &registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"],
    );
    assert_eq!(
        block.1 - block.0,
        9,
        "the expansion must actually reallocate to a 10-wide block"
    );
    assert!(
        block.0 >= 3000 && block.1 <= 9999,
        "block {block:?} must be within the default pool"
    );
    assert_eq!(
        read_ids(&repo),
        ids_before,
        "IDs must not change when the port block is reallocated"
    );
}

#[test]
fn sync_upgrades_an_old_format_env_file_then_second_sync_is_a_noop() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    // Rewrite .env.portool in the pre-identity format (no ID lines).
    let contents = fs::read_to_string(repo.join(".env.portool")).unwrap();
    let old_format: String = contents
        .lines()
        .filter(|l| !l.starts_with("PORTOOL_PROJECT_ID=") && !l.starts_with("PORTOOL_WORKTREE_ID="))
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(repo.join(".env.portool"), &old_format).unwrap();

    // Sync must notice the mismatch and add the ID lines back.
    assert!(env.run(&repo, &["sync"]).status.success());
    let upgraded = fs::read_to_string(repo.join(".env.portool")).unwrap();
    assert_eq!(
        upgraded, contents,
        "sync must restore the ID lines to an old-format file"
    );

    // The follow-up sync must be a complete no-op (mirrors test 4).
    let registry_mtime_1 = fs::metadata(env.registry_path())
        .unwrap()
        .modified()
        .unwrap();
    let env_mtime_1 = fs::metadata(repo.join(".env.portool"))
        .unwrap()
        .modified()
        .unwrap();

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
        "the post-upgrade sync must not rewrite registry.json"
    );
    assert_eq!(
        env_mtime_1, env_mtime_2,
        "the post-upgrade sync must not rewrite .env.portool"
    );
}

// --- Batch B: fail-closed & honesty ---------------------------------------

/// `ls --json` must not disguise a corrupt ledger as an empty-but-valid one:
/// it exits non-zero and emits an explicit error object on stdout (batch B
/// #10).
#[test]
fn ls_json_reports_corrupt_ledger_and_exits_nonzero() {
    let env = TestEnv::new();
    let dir = env.registry_path().parent().unwrap().to_path_buf();
    fs::create_dir_all(&dir).unwrap();
    fs::write(env.registry_path(), b"{ this is not valid json").unwrap();

    let output = env.run(&env.root, &["ls", "--json", "--all"]);
    assert!(
        !output.status.success(),
        "ls --json on a corrupt ledger must exit non-zero"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"error\""),
        "ls --json must emit an explicit error object, got stdout: {stdout}"
    );
}

/// A clap usage error (unknown subcommand) exits with the dedicated usage
/// code 64, not clap's default 2 (which used to collide with a real
/// allocation error) (batch B #15).
#[test]
fn usage_error_exits_64() {
    let env = TestEnv::new();
    let output = env.run(&env.root, &["definitely-not-a-subcommand"]);
    assert_eq!(
        output.status.code(),
        Some(64),
        "a clap usage error must exit 64, got: {:?}",
        output.status.code()
    );
}

/// A malformed global config is fail-closed: portool exits with a general
/// error (1) rather than silently reverting to defaults (batch B #8).
#[test]
fn malformed_config_is_fatal() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    env.write_config("range = [oops not a list\n");

    let output = env.run(&repo, &["sync"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a malformed config.toml must be a hard error (exit 1), stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// --- Batch C: allocation from the pool, reallocate ------------------------

/// The core fix for the 14-repository exhaustion: blocks come straight from
/// the pool, so a tiny pool holds exactly as many blocks as it has slots --
/// not zero (which the old 500-wide subrange model produced for any pool
/// under 500 ports).
#[test]
fn many_projects_share_the_pool_without_subrange_exhaustion() {
    let env = TestEnv::new();
    // A 15-port pool == exactly three 5-wide blocks. Under the old model a
    // sub-500 pool could not place even one project. An isolated high range
    // keeps this test's bind-checks clear of the port-binding tests below,
    // which run in parallel on the shared 127.0.0.1 space.
    env.write_config("range = [3900, 3914]\nblock_align = 5\n");

    for i in 0..3 {
        let repo = env.path(&format!("repo-{i}"));
        init_repo(&repo);
        let output = env.run(&repo, &["sync"]);
        assert!(
            output.status.success(),
            "project {i} must allocate from the shared pool; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // The fourth project exhausts the pool -> exit 3 (not the old 2).
    let repo = env.path("repo-3");
    init_repo(&repo);
    assert_eq!(env.run(&repo, &["sync"]).status.code(), Some(3));
}

/// Rewrites the ledger so `repo`'s worktree block is the single-port block
/// `(port, port)`. Combined with a held ephemeral port, this creates a
/// deterministic bind conflict at the execution boundary -- avoiding the race
/// of binding a *predicted* port that a parallel test might grab first.
fn pin_block_to_port(env: &TestEnv, repo: &Path, port: u16) {
    let mut reg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.registry_path()).unwrap()).unwrap();
    reg["projects"][common_dir_key(repo)]["worktrees"][worktree_key(repo)]["block"] =
        serde_json::json!([port, port]);
    fs::write(env.registry_path(), serde_json::to_string(&reg).unwrap()).unwrap();
}

/// `portool reallocate` moves a worktree off a block whose port something
/// else now holds.
#[test]
fn reallocate_moves_off_a_block_whose_port_is_in_use() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join(".portool.toml"), "[ports]\nweb = 0\n").unwrap();
    assert!(env.run(&repo, &["sync"]).status.success());

    // Hold a guaranteed-free ephemeral port and pin the worktree onto it, so
    // reallocate must move elsewhere.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    pin_block_to_port(&env, &repo, port);

    let output = env.run(&repo, &["reallocate"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let block1 = env.registry()["projects"][common_dir_key(&repo)]["worktrees"]
        [worktree_key(&repo)]["block"]
        .clone();
    assert_ne!(
        block1,
        serde_json::json!([port, port]),
        "reallocate must move off the occupied block"
    );
}

/// `portool reallocate` errors when the worktree has no allocation yet.
#[test]
fn reallocate_without_allocation_errors() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let output = env.run(&repo, &["reallocate"]);
    assert_eq!(output.status.code(), Some(1));
}

// v0.8.0 (external review 3rd round P1-2): exec's bind check moved from
// default-on to opt-in via --check-ports/--strict/--reallocate-on-conflict.
// The exec bind-recheck coverage now lives in tests/exec.rs, alongside the
// rest of exec's behavior.

// --- Batch D: check / release / deinit / doctor ---------------------------

/// `portool check` succeeds on a healthy setup and fails (non-zero) on a
/// corrupt ledger.
#[test]
fn check_reports_health_and_corruption() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    assert!(
        env.run(&repo, &["check"]).status.success(),
        "check must pass on a healthy ledger"
    );

    // Corrupt the ledger; check must now fail.
    fs::write(env.registry_path(), b"{ not json").unwrap();
    assert!(
        !env.run(&repo, &["check"]).status.success(),
        "check must fail on a corrupt ledger"
    );
}

/// 指摘13: a backup refresh failure only warns (`store::save`), so
/// `registry.json.bak` can silently go stale while `registry.json` itself
/// stays healthy. `check` must surface that with a stderr warning -- but
/// staleness heals on the next save, so exit code stays 0 (non-zero exits
/// are reserved for real corruption).
#[test]
fn check_warns_when_backup_is_stale() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    let bak_path = env.state.join("portool").join("registry.json.bak");
    let original_bak = fs::read(&bak_path).unwrap();
    fs::write(&bak_path, b"stale backup contents").unwrap();

    let out = env.run(&repo, &["check"]);
    assert!(
        out.status.success(),
        "stale backup must still be exit 0 -- it heals on the next save"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("backup"),
        "stderr must warn about the stale backup: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Restore the backup to match; the warning must disappear.
    fs::write(&bak_path, &original_bak).unwrap();
    let out = env.run(&repo, &["check"]);
    assert!(out.status.success());
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("backup"),
        "no warning once the backup matches again: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `portool release` removes the worktree's entry and `.env.portool`.
#[test]
fn release_frees_the_block_and_removes_env_file() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    assert!(repo.join(".env.portool").exists());

    assert!(env.run(&repo, &["release"]).status.success());
    assert!(!repo.join(".env.portool").exists(), "env file must be gone");
    let registry = env.registry();
    let project = registry["projects"].get(common_dir_key(&repo));
    // Either the project entry is gone, or it has no worktree entry.
    let has_entry = project
        .and_then(|p| p["worktrees"].get(worktree_key(&repo)))
        .is_some();
    assert!(!has_entry, "the worktree entry must be released");
}

/// `portool deinit` releases all of this project's allocations, and removes
/// portool's hooks, env files, and `info/exclude` entry.
#[test]
fn deinit_releases_allocations_removes_env_hooks_and_exclude() {
    let env = TestEnv::new();
    env.write_config("range = [18000, 18099]\n");
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["init"]).status.success());
    assert!(repo.join(".env.portool").exists());

    let out = env.run(&repo, &["deinit"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !repo.join(".env.portool").exists(),
        "env file must be removed"
    );
    let reg = env.registry();
    assert!(
        reg["projects"].as_object().unwrap().is_empty(),
        "the project's allocations must be released"
    );
    let hook = fs::read_to_string(repo.join(".git/hooks/post-checkout")).ok();
    assert!(
        hook.is_none_or(|c| !c.contains("portool")),
        "hooks must be gone"
    );
    let exclude = fs::read_to_string(repo.join(".git/info/exclude")).unwrap_or_default();
    assert!(!exclude.contains(".env.portool"));
}

/// `portool deinit --keep-allocations` only removes hooks and the
/// `info/exclude` entry -- the ledger and `.env.portool` are left alone.
#[test]
fn deinit_keep_allocations_leaves_the_ledger_alone() {
    let env = TestEnv::new();
    env.write_config("range = [18100, 18199]\n");
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["init"]).status.success());

    let out = env.run(&repo, &["deinit", "--keep-allocations"]);
    assert!(out.status.success());
    assert!(repo.join(".env.portool").exists());
    assert!(!env.registry()["projects"].as_object().unwrap().is_empty());
}

/// `deinit` never edits a tracked `.gitignore`, even when it carries a bare
/// `.env.portool` line (added by portool <= 0.6, or by hand) -- ownership of
/// that line is unknowable, so it's only hinted about.
#[test]
fn deinit_never_edits_a_user_gitignore() {
    let env = TestEnv::new();
    let repo = env.path("app");
    init_repo(&repo);
    // A user-owned .gitignore that happens to carry the line (e.g. from
    // portool <= 0.6, or hand-written).
    fs::write(repo.join(".gitignore"), "node_modules\n.env.portool\n").unwrap();
    assert!(env.run(&repo, &["init"]).status.success());

    let out = env.run(&repo, &["deinit"]);
    assert!(out.status.success());
    assert_eq!(
        fs::read_to_string(repo.join(".gitignore")).unwrap(),
        "node_modules\n.env.portool\n",
        ".gitignore must be byte-identical after deinit"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(".gitignore")
            || String::from_utf8_lossy(&out.stderr).contains(".gitignore"),
        "must hint about the leftover line"
    );
}

/// P2 (external review,指摘8): a lost ledger must not leave `.env.portool`
/// behind. `deinit` unions git's live worktree list with the ledger's
/// recorded paths, so even with no `registry.json`/`.bak` at all it still
/// finds and removes the env file via `git worktree list`.
#[test]
fn deinit_without_registry_removes_live_env_files() {
    let env = TestEnv::new();
    env.write_config("range = [19000, 19009]\n");
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["init"]).status.success());
    assert!(env.run(&repo, &["sync"]).status.success());
    assert!(repo.join(".env.portool").exists());

    fs::remove_file(env.registry_path()).unwrap();
    fs::remove_file(env.state.join("portool").join("registry.json.bak")).unwrap();

    let out = env.run(&repo, &["deinit"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !repo.join(".env.portool").exists(),
        "env file must be removed even with no ledger at all"
    );
}

/// A linked worktree's entry can go missing from the ledger (e.g. hand-
/// edited, or a prior partial failure) while its `.env.portool` still sits
/// on disk. `deinit` must still find and remove it via the live worktree
/// union, alongside the worktree that IS still tracked in the ledger.
#[test]
fn deinit_removes_env_files_missing_from_registry() {
    let env = TestEnv::new();
    env.write_config("range = [19010, 19029]\n");
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["init"]).status.success());
    assert!(env.run(&repo, &["sync"]).status.success());
    assert!(repo.join(".env.portool").exists());

    let wt = env.path("app-wt");
    git(&repo, &["branch", "feature-x"]);
    git(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feature-x"],
    );
    assert!(env.run(&wt, &["sync"]).status.success());
    assert!(wt.join(".env.portool").exists());

    // Hand-remove just the linked worktree's ledger entry, simulating a
    // lost/corrupted-and-partially-repaired entry -- its env file is still
    // on disk and git still reports it as a live worktree.
    let mut registry = env.registry();
    registry["projects"][common_dir_key(&repo)]["worktrees"]
        .as_object_mut()
        .unwrap()
        .remove(&worktree_key(&wt));
    fs::write(
        env.registry_path(),
        serde_json::to_vec_pretty(&registry).unwrap(),
    )
    .unwrap();

    let out = env.run(&repo, &["deinit"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !repo.join(".env.portool").exists(),
        "the main worktree's env file (still in the ledger) must be removed"
    );
    assert!(
        !wt.join(".env.portool").exists(),
        "the linked worktree's env file (missing from the ledger) must be removed too"
    );
}

/// `portool unhook` removes only the hooks -- the ledger, env file, and
/// `info/exclude` entry are all left in place.
#[test]
fn unhook_removes_hooks_but_keeps_everything_else() {
    let env = TestEnv::new();
    env.write_config("range = [18200, 18299]\n");
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["init"]).status.success());

    let out = env.run(&repo, &["unhook"]);
    assert!(out.status.success());
    assert!(!repo.join(".git/hooks/post-checkout").exists());
    assert!(repo.join(".env.portool").exists(), "env kept");
    assert!(
        !env.registry()["projects"].as_object().unwrap().is_empty(),
        "ledger kept"
    );
    let exclude = fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
    assert!(exclude.contains(".env.portool"), "exclude kept");
}

/// `portool doctor` rebuilds a ledger entry from a live worktree's
/// `.env.portool` after the ledger has lost it.
#[test]
fn doctor_rebuilds_a_lost_entry_from_the_env_file() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    let block_before = env.registry()["projects"][common_dir_key(&repo)]["worktrees"]
        [worktree_key(&repo)]["block"]
        .clone();

    // Simulate a corruption reset: wipe the ledger, but the worktree still
    // has its .env.portool.
    fs::remove_file(env.registry_path()).unwrap();
    assert!(repo.join(".env.portool").exists());

    assert!(
        env.run(&repo, &["doctor"]).status.success(),
        "doctor must succeed"
    );

    let block_after = env.registry()["projects"][common_dir_key(&repo)]["worktrees"]
        [worktree_key(&repo)]["block"]
        .clone();
    assert_eq!(
        block_before, block_after,
        "doctor must re-import the same block recorded in .env.portool"
    );
}

// --- v0.5.1: fail-closed ledger, doctor --repair, and contract fixes -------

/// A corrupt ledger makes `sync` fail (exit 1) instead of being silently
/// moved aside and replaced with an empty one (external review P1 #1).
#[test]
fn corrupt_ledger_makes_sync_fail_and_is_left_in_place() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let dir = env.registry_path().parent().unwrap().to_path_buf();
    fs::create_dir_all(&dir).unwrap();
    let garbage = b"{ this is not valid json".to_vec();
    fs::write(env.registry_path(), &garbage).unwrap();

    let output = env.run(&repo, &["sync"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "sync on a corrupt ledger must fail; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("doctor --repair"),
        "the error must point at the recovery command, got: {stderr}"
    );
    assert_eq!(
        fs::read(env.registry_path()).unwrap(),
        garbage,
        "the corrupt ledger must be left byte-identical in place"
    );
    let siblings: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains("corrupt"))
        .collect();
    assert!(
        siblings.is_empty(),
        "sync must not move the ledger aside, found: {siblings:?}"
    );
    assert!(
        !repo.join(".env.portool").exists(),
        "no allocation may be handed out from a corrupt ledger"
    );
}

/// A ledger written by a *newer* portool (future schema version) is neither
/// treated as corrupt nor auto-reset: sync fails and tells the user to
/// upgrade, and the file is untouched.
#[test]
fn future_schema_ledger_is_not_auto_reset() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let dir = env.registry_path().parent().unwrap().to_path_buf();
    fs::create_dir_all(&dir).unwrap();
    let future = br#"{"version":4,"range":[3000,9999],"projects":{},"reservations":[]}"#.to_vec();
    fs::write(env.registry_path(), &future).unwrap();

    let output = env.run(&repo, &["sync"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "sync must fail on a future-schema ledger; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("upgrade portool"),
        "the error must steer toward upgrading portool, got: {stderr}"
    );
    assert_eq!(
        fs::read(env.registry_path()).unwrap(),
        future,
        "the newer ledger must be left byte-identical in place"
    );
}

/// The corrupt-ledger recovery path with no usable backup: `doctor` alone
/// refuses, `doctor --repair` alone also refuses (no valid backup to
/// restore from), and only `doctor --repair --abandon-other-projects` moves
/// the file aside and rebuilds this project's entries from live worktrees'
/// `.env.portool`.
#[test]
fn doctor_repair_moves_corrupt_ledger_aside_and_rebuilds() {
    let env = TestEnv::new();
    // An isolated range keeps this test's bind checks (and block values)
    // clear of the parallel tests sharing the default 3000+ pool.
    env.write_config("range = [4200, 4209]\nblock_align = 5\n");
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    let block_before = env.registry()["projects"][common_dir_key(&repo)]["worktrees"]
        [worktree_key(&repo)]["block"]
        .clone();

    fs::write(env.registry_path(), b"{ not json").unwrap();
    // Remove the backup to simulate "nothing to restore", so this test
    // exercises the destructive abandon path, not the restore-from-backup
    // path (covered separately).
    fs::remove_file(env.state.join("portool").join("registry.json.bak")).unwrap();

    // Without --repair: hard error, file untouched.
    let output = env.run(&repo, &["doctor"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "doctor without --repair must fail on a corrupt ledger"
    );
    assert_eq!(fs::read(env.registry_path()).unwrap(), b"{ not json");

    // With --repair alone: no valid backup to restore from, so it refuses.
    let output = env.run(&repo, &["doctor", "--repair"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "doctor --repair without a valid backup must refuse"
    );
    assert_eq!(fs::read(env.registry_path()).unwrap(), b"{ not json");

    // With --repair --abandon-other-projects: file moved aside, entry
    // rebuilt from .env.portool.
    let output = env.run(&repo, &["doctor", "--repair", "--abandon-other-projects"]);
    assert!(
        output.status.success(),
        "doctor --repair --abandon-other-projects must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dir = env.registry_path().parent().unwrap().to_path_buf();
    let moved: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("registry.json.corrupt-"))
        .collect();
    assert_eq!(moved.len(), 1, "the corrupt file must be moved aside");

    let block_after = env.registry()["projects"][common_dir_key(&repo)]["worktrees"]
        [worktree_key(&repo)]["block"]
        .clone();
    assert_eq!(
        block_before, block_after,
        "doctor --repair --abandon-other-projects must re-import the block recorded in \
         .env.portool"
    );
}

/// P0-1: doctor --repair restores the whole ledger from the backup, so a
/// corrupt registry never silently drops *other* projects' allocations.
#[test]
fn doctor_repair_restores_other_projects_from_backup() {
    let env = TestEnv::new();
    env.write_config("range = [17500, 17599]\n");

    // Project A syncs (this also writes registry.json.bak).
    let repo_a = env.path("aaa");
    init_repo(&repo_a);
    assert!(env.run(&repo_a, &["sync"]).status.success());

    // Project B syncs too; the backup now contains both.
    let repo_b = env.path("bbb");
    init_repo(&repo_b);
    assert!(env.run(&repo_b, &["sync"]).status.success());

    // Corrupt the live ledger.
    fs::write(env.registry_path(), "{ not json").unwrap();

    // Repair from project A.
    let out = env.run(&repo_a, &["doctor", "--repair"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Both projects' entries survive.
    let reg = env.registry();
    let projects = reg["projects"].as_object().unwrap();
    assert_eq!(
        projects.len(),
        2,
        "backup restore must keep project B: {projects:?}"
    );
}

/// Without a valid backup, plain --repair refuses; only the explicit
/// destructive flag abandons other projects.
#[test]
fn doctor_repair_without_backup_requires_abandon_flag() {
    let env = TestEnv::new();
    env.write_config("range = [17600, 17699]\n");
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    fs::write(env.registry_path(), "{ not json").unwrap();
    // Remove the backup to simulate "nothing to restore".
    fs::remove_file(env.state.join("portool").join("registry.json.bak")).unwrap();

    let out = env.run(&repo, &["doctor", "--repair"]);
    assert!(!out.status.success(), "must refuse without a backup");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--abandon-other-projects"), "got: {stderr}");
    // The corrupt file was NOT moved aside by the refusal.
    assert!(env.registry_path().exists());

    // The explicit flag performs the old destructive rebuild.
    let out = env.run(&repo, &["doctor", "--repair", "--abandon-other-projects"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An UnsupportedVersion (newer-schema) ledger is never repaired by plain
/// `--repair` -- the fix is upgrading portool -- and is never auto-restored
/// from backup (that would silently roll back a newer binary's ledger).
/// Only the explicit `--abandon-other-projects` flag may discard it.
#[test]
fn doctor_repair_on_future_schema_requires_abandon_flag() {
    let env = TestEnv::new();
    env.write_config("range = [17700, 17799]\n");
    let repo = env.path("app");
    init_repo(&repo);
    // A prior successful sync leaves a valid registry.json.bak behind, so
    // this test also proves the backup is NOT used to "restore over" a
    // newer-schema ledger.
    assert!(env.run(&repo, &["sync"]).status.success());
    assert!(env.state.join("portool").join("registry.json.bak").exists());

    let future = br#"{"version":999,"range":[17700,17799],"projects":{},"reservations":[]}"#;
    fs::write(env.registry_path(), future).unwrap();

    // (a) Plain --repair fails, steers toward upgrading, leaves the file.
    let out = env.run(&repo, &["doctor", "--repair"]);
    assert!(
        !out.status.success(),
        "--repair alone must refuse a future-schema ledger"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("upgrade portool"),
        "the error must steer toward upgrading, got: {stderr}"
    );
    assert_eq!(
        fs::read(env.registry_path()).unwrap(),
        future.to_vec(),
        "the newer-schema ledger must be left byte-identical in place"
    );

    // (b) Only the explicit destructive flag discards it, moving it aside.
    let out = env.run(&repo, &["doctor", "--repair", "--abandon-other-projects"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dir = env.registry_path().parent().unwrap().to_path_buf();
    let moved: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("registry.json.corrupt-"))
        .collect();
    assert_eq!(
        moved.len(),
        1,
        "the abandoned ledger must be moved aside: {moved:?}"
    );
}

/// `doctor` must not import a nonsense block (port 0, reversed) from a
/// hand-edited `.env.portool` header into the ledger (external review P2 #7).
#[test]
fn doctor_skips_invalid_block_headers() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(
        repo.join(".env.portool"),
        "# generated by portool \u{2014} DO NOT EDIT\n# block: 0-0  generation: 1  project: p  worktree: /w\nPORT=0\n",
    )
    .unwrap();

    let output = env.run(&repo, &["doctor"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("skipping re-import"),
        "doctor must report the skipped invalid block, got: {stderr}"
    );
    assert!(
        !env.registry_path().exists(),
        "nothing valid was imported, so no ledger may be written"
    );
}

/// `doctor` must not import a block from a `.env.portool` whose
/// `PORTOOL_PROJECT_ID`/`PORTOOL_WORKTREE_ID` don't match the IDs derived
/// from the current project/worktree paths -- e.g. a file copied in from
/// another worktree (external review P2 #7).
#[test]
fn doctor_skips_env_file_with_foreign_identity() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    // Simulate a `.env.portool` copied in from a different worktree: valid
    // shape, but its identity lines belong to someone else.
    let contents = fs::read_to_string(repo.join(".env.portool")).unwrap();
    let foreign: String = contents
        .lines()
        .map(|line| {
            if line.starts_with("PORTOOL_PROJECT_ID=") {
                "PORTOOL_PROJECT_ID=0000000000000000".to_string()
            } else if line.starts_with("PORTOOL_WORKTREE_ID=") {
                "PORTOOL_WORKTREE_ID=1111111111111111".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(repo.join(".env.portool"), foreign).unwrap();

    // Delete the ledger entry doctor would otherwise consider already
    // present, forcing it down the rebuild/import path.
    fs::remove_file(env.registry_path()).unwrap();

    let output = env.run(&repo, &["doctor"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("identifies a different project/worktree"),
        "doctor must report the foreign identity, got: {stdout}"
    );
    assert!(
        !env.registry_path().exists(),
        "nothing valid was imported, so no ledger may be written"
    );
}

/// `doctor` diagnoses hook-effectiveness problems (external review P1-4):
/// "installed" must mean "will actually run". Covers a missing hook and a
/// hook with a dead embedded `PORTOOL_BIN` path.
#[test]
fn doctor_reports_hook_problems() {
    let env = TestEnv::new();
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    // No hook installed at all.
    let out = env.run(&repo, &["doctor"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("not installed"), "got: {stdout}");

    // A hook with a dead embedded path.
    let hooks = repo.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(
        hooks.join("post-checkout"),
        "#!/bin/sh\n# installed by portool\nPORTOOL_BIN=\"/no/such/portool\"\nif ! [ -x \"$PORTOOL_BIN\" ]; then PORTOOL_BIN=portool; fi\nif command -v \"$PORTOOL_BIN\" >/dev/null 2>&1; then\n  \"$PORTOOL_BIN\" sync --quiet || true\nfi\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(
        hooks.join("post-checkout"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let out = env.run(&repo, &["doctor"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("/no/such/portool"), "got: {stdout}");
}

/// Task 3 (external review, doctor's unreachable-hook detection): a hook
/// installed by portool <= 0.8 appended the managed block at EOF, after a
/// top-level `exit 0` git never reaches past. Such a hook still "invokes
/// portool" and is executable, so the pre-existing checks call it healthy;
/// `doctor` must additionally flag it as unreachable. Built by hand (not via
/// `init`, which now inserts after the shebang and would never reproduce
/// this legacy layout).
#[test]
fn existing_hook_exit_before_managed_block_is_not_reported_healthy() {
    let env = TestEnv::new();
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    let hooks = repo.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let legacy_layout = format!(
        "#!/bin/sh\nexit 0\n{}\nPORTOOL_BIN=portool\nif command -v \"$PORTOOL_BIN\" >/dev/null 2>&1; then \"$PORTOOL_BIN\" sync --quiet || true; fi\n{}\n",
        portool::hooks::HOOK_BLOCK_BEGIN,
        portool::hooks::HOOK_BLOCK_END,
    );
    for name in ["post-checkout", "post-merge"] {
        fs::write(hooks.join(name), &legacy_layout).unwrap();
        fs::set_permissions(hooks.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }

    let out = env.run(&repo, &["doctor"]);
    assert!(
        out.status.success(),
        "doctor's hook-health report is advisory only; exit code must stay 0"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("top-level exit/exec before portool's block"),
        "got: {stdout}"
    );
}

/// Task 3: a hook with a malformed managed block (duplicate begin markers)
/// still contains a portool invocation, so `doctor` must go on to flag the
/// malformed layout specifically rather than calling the hook healthy.
#[test]
fn doctor_reports_a_malformed_managed_block() {
    let env = TestEnv::new();
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    let hooks = repo.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let malformed = format!(
        "#!/bin/sh\n{}\nportool sync --quiet || true\n{}\n{}\nother\n{}\n",
        portool::hooks::HOOK_BLOCK_BEGIN,
        portool::hooks::HOOK_BLOCK_END,
        portool::hooks::HOOK_BLOCK_BEGIN,
        portool::hooks::HOOK_BLOCK_END,
    );
    fs::write(hooks.join("post-checkout"), &malformed).unwrap();
    fs::set_permissions(
        hooks.join("post-checkout"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let out = env.run(&repo, &["doctor"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("managed block is malformed"),
        "got: {stdout}"
    );
}

/// The config is validated before the lock-free fast path: a config broken
/// *after* a successful sync still fails the next sync, instead of being
/// skipped on the fast path and only surfacing days later (external review
/// P1 #4).
#[test]
fn fast_path_rejects_newly_malformed_config() {
    let env = TestEnv::new();
    env.write_config("range = [4210, 4219]\nblock_align = 5\n");
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    env.write_config("range = [oops not a list\n");

    let output = env.run(&repo, &["sync"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a no-op (fast-path) sync must still fail on a malformed config, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `reallocate` must always move to a *different* block, per its CLI
/// contract, even when the current block is perfectly free and bindable
/// (external review P2 #5).
#[test]
fn reallocate_moves_even_when_current_block_is_free() {
    let env = TestEnv::new();
    env.write_config("range = [4220, 4239]\nblock_align = 5\n");
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    let block_before = env.registry()["projects"][common_dir_key(&repo)]["worktrees"]
        [worktree_key(&repo)]["block"]
        .clone();

    let output = env.run(&repo, &["reallocate"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let block_after = env.registry()["projects"][common_dir_key(&repo)]["worktrees"]
        [worktree_key(&repo)]["block"]
        .clone();
    assert_ne!(
        block_before, block_after,
        "reallocate must never re-select the current block"
    );
}

/// A global-scope `core.hooksPath` pointing at a Husky-shaped dir
/// (`.../.husky/_`) is still a shared hooks dir: `init` must refuse it, not
/// classify it as Husky and write into it (external review P1 #3).
#[test]
fn init_refuses_global_husky_shaped_hookspath() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let shared = env.path("shared/.husky/_");
    fs::create_dir_all(&shared).unwrap();
    let global_config = env.path("gitconfig-global");
    fs::write(
        &global_config,
        format!("[core]\n\thooksPath = {}\n", shared.display()),
    )
    .unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_portool"));
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("HOME", &env.home)
        .env("XDG_STATE_HOME", &env.state)
        .env("XDG_CONFIG_HOME", &env.config)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .current_dir(&repo)
        .args(["init", "--hook-only"]);
    let output = cmd.output().expect("failed to spawn portool");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // External review P1-4: "installed nothing" must not look like success.
    assert!(
        !output.status.success(),
        "init must fail when no hook location is installable, stderr: {stderr}"
    );
    assert!(
        stderr.contains("outside this repository") && stderr.contains("global"),
        "must warn about the global-scope hooksPath resolving outside the repo, got: {stderr}"
    );
    assert!(
        !shared.parent().unwrap().join("post-checkout").exists()
            && !shared.join("post-checkout").exists(),
        "must not write anywhere under a global Husky-shaped hooksPath"
    );
    assert!(
        !repo.join(".git/hooks/post-checkout").exists(),
        "must not fall back to the ignored default hooks dir"
    );
}

/// If removing `.env.portool` fails, `release` must keep the ledger entry
/// (block still reserved) and fail -- never free the block while the old
/// env file keeps handing out its ports (external review P1 #2).
#[test]
fn release_env_delete_failure_keeps_the_ledger_entry() {
    let env = TestEnv::new();
    env.write_config("range = [4240, 4249]\nblock_align = 5\n");
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    assert!(repo.join(".env.portool").exists());

    // A read-only worktree dir makes the env-file unlink fail (EACCES).
    let readonly = fs::Permissions::from_mode(0o555);
    fs::set_permissions(&repo, readonly).unwrap();
    let output = env.run(&repo, &["release"]);
    fs::set_permissions(&repo, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(
        output.status.code(),
        Some(1),
        "release must fail when the env file can't be removed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        repo.join(".env.portool").exists(),
        "the env file is still there"
    );
    let registry = env.registry();
    assert!(
        registry["projects"][common_dir_key(&repo)]["worktrees"]
            .get(worktree_key(&repo))
            .is_some(),
        "the ledger entry must be kept while the env file still exists"
    );
}

/// A manifest whose required block size cannot be represented in a u16 is
/// rejected outright, instead of being clamped so that two declared offsets
/// silently share one port (external review P2 #6).
#[test]
fn sync_rejects_manifest_wider_than_u16() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(
        repo.join(".portool.toml"),
        "[ports]\na = 65534\nb = 65535\n",
    )
    .unwrap();

    let output = env.run(&repo, &["sync"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid manifest"),
        "the error must name the manifest, got: {stderr}"
    );
}

/// A manifest with an unrecognized top-level table (e.g. a typo'd `[ports]`)
/// is rejected fail-closed rather than silently ignored (P1-6).
#[test]
fn sync_rejects_manifest_with_unknown_table() {
    let env = TestEnv::new();
    let repo = env.path("app");
    init_repo(&repo);
    fs::write(
        repo.join(".portool.toml"),
        "[ports]\nweb = 0\n[bogus]\nx = 1\n",
    )
    .unwrap();
    let out = env.run(&repo, &["sync"]);
    assert!(!out.status.success());
}

// --- v0.6: schema v3, two-phase moves, generation --------------------------

/// Rewrites the on-disk ledger with a mutation applied to its JSON value.
fn edit_registry(env: &TestEnv, mutate: impl FnOnce(&mut serde_json::Value)) {
    let mut reg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.registry_path()).unwrap()).unwrap();
    mutate(&mut reg);
    fs::write(env.registry_path(), serde_json::to_string(&reg).unwrap()).unwrap();
}

/// A v2 (0.5.x) ledger is read via in-memory migration: blocks are kept
/// verbatim and the new v3 fields are filled in.
#[test]
fn v2_ledger_is_migrated_with_blocks_preserved() {
    let env = TestEnv::new();
    env.write_config("range = [4250, 4259]\nblock_align = 5\n");
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    let block_before = block_tuple(
        &env.registry()["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]
            ["block"],
    );

    // Downgrade the on-disk ledger to the v2 shape (no generation /
    // pending_block, version 2), exactly what a 0.5.x binary wrote.
    edit_registry(&env, |reg| {
        reg["version"] = serde_json::json!(2);
        let projects = reg["projects"].as_object_mut().unwrap();
        for project in projects.values_mut() {
            for worktree in project["worktrees"].as_object_mut().unwrap().values_mut() {
                let obj = worktree.as_object_mut().unwrap();
                obj.remove("generation");
                obj.remove("pending_block");
            }
        }
    });

    let output = env.run(&repo, &["ls", "--json"]);
    assert!(
        output.status.success(),
        "a v2 ledger must load via migration; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    assert_eq!(json["registry_schema_version"], serde_json::json!(3));
    let allocations = json["allocations"].as_array().unwrap();
    assert_eq!(allocations.len(), 1);
    let entry = &allocations[0];
    assert_eq!(
        entry["project_key"],
        serde_json::json!(common_dir_key(&repo))
    );
    assert_eq!(entry["path"], serde_json::json!(worktree_key(&repo)));
    assert_eq!(
        block_tuple(&entry["block"]),
        block_before,
        "the block must be preserved verbatim across the migration"
    );
    assert_eq!(entry["generation"], serde_json::json!(1));
    // Storage-schema internals (the v2->v3 migration's `pending_block`) must
    // not leak into the stable envelope.
    assert!(entry.get("pending_block").is_none());
}

/// Crash recovery, forward direction: the ledger carries a pending move
/// target and the env file already points at it (the crash happened after
/// the env write) -- the next sync finalizes the move.
#[test]
fn interrupted_move_rolls_forward_when_env_carries_pending() {
    let env = TestEnv::new();
    env.write_config("range = [4260, 4269]\nblock_align = 5\n");
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    // Task 8: main no longer special-cases to slot 0, so the initial block
    // is read back and the pending target is the pool's *other* 5-wide slot.
    let own_block = block_tuple(
        &env.registry()["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]
            ["block"],
    );
    let pending: (u16, u16) = if own_block == (4260, 4264) {
        (4265, 4269)
    } else {
        (4260, 4264)
    };
    edit_registry(&env, |reg| {
        reg["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["pending_block"] =
            serde_json::json!([pending.0, pending.1]);
    });
    // Simulate the phase-2 env write having completed: rewrite the header
    // to the pending block with the post-move generation.
    let env_file = fs::read_to_string(repo.join(".env.portool")).unwrap();
    let moved = env_file
        .replace(
            &format!("# block: {}-{}  generation: 1", own_block.0, own_block.1),
            &format!("# block: {}-{}  generation: 2", pending.0, pending.1),
        )
        .replace(
            &format!("PORT={}", own_block.0),
            &format!("PORT={}", pending.0),
        );
    fs::write(repo.join(".env.portool"), moved).unwrap();

    let output = env.run(&repo, &["sync"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let entry =
        env.registry()["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)].clone();
    assert_eq!(
        block_tuple(&entry["block"]),
        pending,
        "the move must be rolled forward to the block the env already uses"
    );
    assert_eq!(entry["generation"], serde_json::json!(2));
    assert_eq!(entry["pending_block"], serde_json::Value::Null);
}

/// Crash recovery, backward direction: the ledger carries a pending target
/// but the env still points at the old block (the crash happened before
/// the env write) -- the next sync releases the reservation and keeps the
/// old block.
#[test]
fn interrupted_move_rolls_back_when_env_still_has_old_block() {
    let env = TestEnv::new();
    env.write_config("range = [4270, 4279]\nblock_align = 5\n");
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    // Task 8: main no longer special-cases to slot 0, so the initial block
    // is read back and the pending target is the pool's *other* 5-wide slot.
    let own_block = block_tuple(
        &env.registry()["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]
            ["block"],
    );
    let pending: (u16, u16) = if own_block == (4270, 4274) {
        (4275, 4279)
    } else {
        (4270, 4274)
    };
    edit_registry(&env, |reg| {
        reg["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["pending_block"] =
            serde_json::json!([pending.0, pending.1]);
    });

    let output = env.run(&repo, &["sync"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let entry =
        env.registry()["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)].clone();
    assert_eq!(
        block_tuple(&entry["block"]),
        own_block,
        "the never-completed move must be rolled back"
    );
    assert_eq!(entry["generation"], serde_json::json!(1));
    assert_eq!(entry["pending_block"], serde_json::Value::Null);
}

/// A pending block is occupied: allocation for another worktree must not
/// be given a block that overlaps someone's in-flight move target.
#[test]
fn pending_block_is_excluded_from_allocation() {
    let env = TestEnv::new();
    // A pool with room for exactly two 5-wide blocks.
    env.write_config("range = [3800, 3809]\nblock_align = 5\n");
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    // The main worktree owns one block and has a pending move onto the
    // other -- the whole pool is now spoken for.
    let own_block = env.registry()["projects"][common_dir_key(&repo)]["worktrees"]
        [worktree_key(&repo)]["block"]
        .clone();
    let pending: (u16, u16) = if own_block == serde_json::json!([3800, 3804]) {
        (3805, 3809)
    } else {
        (3800, 3804)
    };
    edit_registry(&env, |reg| {
        reg["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["pending_block"] =
            serde_json::json!([pending.0, pending.1]);
    });

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

    let output = env.run(&wt, &["sync"]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "the pending block must count as occupied (pool exhausted); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `reallocate` bumps the generation counter, and the env header records it.
#[test]
fn reallocate_bumps_the_generation() {
    let env = TestEnv::new();
    env.write_config("range = [4280, 4299]\nblock_align = 5\n");
    let repo = env.path("repo");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    let output = env.run(&repo, &["reallocate"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let entry =
        env.registry()["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)].clone();
    assert_eq!(entry["generation"], serde_json::json!(2));
    assert_eq!(entry["pending_block"], serde_json::Value::Null);
    let env_file = fs::read_to_string(repo.join(".env.portool")).unwrap();
    assert!(
        env_file.contains("generation: 2"),
        "the env header must record the new generation: {env_file}"
    );
}

// --- Task 8: allocation stability -- project+branch hashing, GC before ----
// --- allocation -------------------------------------------------------------

/// P1-8: deleting a worktree and re-creating one on the same branch (at a
/// different path) must return to the same block: stale GC runs before
/// allocation and the preferred slot hashes (project, branch).
#[test]
fn recreated_worktree_on_same_branch_reclaims_its_block() {
    let env = TestEnv::new();
    // 18000-18099 is already used by a Task 6 test; this is an isolated
    // range of its own.
    env.write_config("range = [18500, 18599]\n");
    let repo = env.path("app");
    init_repo(&repo);
    git(&repo, &["branch", "feature-x"]);

    let wt1 = env.path("wt1");
    git(
        &repo,
        &["worktree", "add", wt1.to_str().unwrap(), "feature-x"],
    );
    assert!(env.run(&wt1, &["sync"]).status.success());
    let block1 = env.registry()["projects"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()["worktrees"]
        .as_object()
        .unwrap()
        .values()
        .find(|w| w["branch"] == "feature-x")
        .unwrap()["block"]
        .clone();

    git(
        &repo,
        &["worktree", "remove", "--force", wt1.to_str().unwrap()],
    );

    let wt2 = env.path("wt2");
    git(
        &repo,
        &["worktree", "add", wt2.to_str().unwrap(), "feature-x"],
    );
    assert!(env.run(&wt2, &["sync"]).status.success());
    let block2 = env.registry()["projects"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()["worktrees"]
        .as_object()
        .unwrap()
        .values()
        .find(|w| w["branch"] == "feature-x")
        .unwrap()["block"]
        .clone();

    assert_eq!(
        block1, block2,
        "same project+branch must return to the same block"
    );
}

// --- Task 9: reserve/unreserve, pin/unpin -----------------------------------

/// Ranges 18000-18299 are used by Task 6 tests, and 18500-18599 by Task 8;
/// this test's 10-wide pool lives just above the Task 8 range.
#[test]
fn reserve_blocks_allocation_and_unreserve_frees_it() {
    let env = TestEnv::new();
    env.write_config("range = [18600, 18609]\n");
    let repo = env.path("app");
    init_repo(&repo);

    // Reserve the entire first half of the pool.
    let out = env.run(&repo, &["reserve", "18600-18604", "--label", "postgres"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Sync must land on the second half.
    assert!(env.run(&repo, &["sync"]).status.success());
    let reg = env.registry();
    let block = &reg["projects"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()["worktrees"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()["block"];
    assert_eq!(
        block[0].as_u64().unwrap(),
        18605,
        "allocation must avoid the reservation"
    );

    // Idempotent re-reserve succeeds; overlapping reserve fails.
    assert!(env.run(&repo, &["reserve", "18600-18604"]).status.success());
    assert!(!env.run(&repo, &["reserve", "18604-18606"]).status.success());

    // Idempotent re-reserve with a new --label updates the stored label
    // instead of silently discarding it.
    let out = env.run(&repo, &["reserve", "18600-18604", "--label", "pg"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        env.registry()["reservations"][0]["label"].as_str(),
        Some("pg"),
        "idempotent re-reserve with --label must update the stored label"
    );

    // Single-port unreserve removes the containing reservation.
    assert!(env.run(&repo, &["unreserve", "18602"]).status.success());
    assert!(env.registry()["reservations"]
        .as_array()
        .unwrap()
        .is_empty());

    // Unreserving again is an error.
    assert!(!env.run(&repo, &["unreserve", "18602"]).status.success());

    // Range-spec unreserve requires an EXACT block match: a sub-range must
    // fail (exit 1) and leave the reservation in place; the exact range
    // then removes it.
    assert!(env.run(&repo, &["reserve", "18600-18604"]).status.success());
    let out = env.run(&repo, &["unreserve", "18601-18604"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a non-exact range spec must not match; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        env.registry()["reservations"].as_array().unwrap().len(),
        1,
        "the failed unreserve must leave the reservation in place"
    );

    // A degenerate range spec (START == END) must NOT containment-match: it
    // has the same syntax shape as a range, so it requires an exact block
    // match just like any other range, even though its parsed value equals
    // a single port inside the wider reservation.
    let out = env.run(&repo, &["unreserve", "18602-18602"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a degenerate range spec must not containment-match; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        env.registry()["reservations"].as_array().unwrap().len(),
        1,
        "the failed degenerate-range unreserve must leave the reservation in place"
    );

    assert!(env
        .run(&repo, &["unreserve", "18600-18604"])
        .status
        .success());
    assert!(env.registry()["reservations"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn pin_protects_a_stale_entry_from_prune() {
    let env = TestEnv::new();
    env.write_config("range = [18700, 18799]\n");
    let repo = env.path("app");
    init_repo(&repo);
    git(&repo, &["branch", "feature-y"]);
    let wt = env.path("wt");
    git(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feature-y"],
    );
    assert!(env.run(&wt, &["sync"]).status.success());
    assert!(env
        .run(&wt, &["pin", "--label", "keep-me"])
        .status
        .success());

    // Verify the pin took effect before the worktree disappears.
    let reg = env.registry();
    let entry = reg["projects"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()["worktrees"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    assert_eq!(entry["pinned"], serde_json::json!(true));
    assert_eq!(entry["label"], serde_json::json!("keep-me"));

    git(
        &repo,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    assert!(env.run(&repo, &["prune"]).status.success());
    let reg = env.registry();
    let worktrees = reg["projects"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()["worktrees"]
        .as_object()
        .unwrap();
    // Only `wt` ever synced (the main worktree never ran `sync`/`init` in
    // this test), so the ledger has exactly one entry -- the pinned `wt` --
    // which must survive an otherwise-eligible prune (gone directory, free
    // ports).
    assert_eq!(worktrees.len(), 1, "pinned entry must survive prune");
    assert!(worktrees.values().next().unwrap()["pinned"]
        .as_bool()
        .unwrap());
}

/// External review P2 (indication 10): `pin --label` claims (help + README)
/// the label shows up in `ls`, but the human table had no LABEL column.
/// This asserts the header includes `LABEL` and the labeled row shows it.
#[test]
fn human_ls_displays_pin_label() {
    let env = TestEnv::new();
    env.write_config("range = [18810, 18819]\n");
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    assert!(env
        .run(&repo, &["pin", "--label", "webapp"])
        .status
        .success());

    let output = env.run(&repo, &["ls"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let header = lines.next().unwrap();
    assert_eq!(
        header.split_whitespace().collect::<Vec<_>>(),
        vec!["PROJECT", "WORKTREE", "BRANCH", "BLOCK", "STATUS", "LABEL"]
    );
    let data_line = lines.next().unwrap();
    assert!(
        data_line.contains("webapp"),
        "labeled row must show the label in the human table, got: {data_line}"
    );
}

/// A label's lifetime is the pin's lifetime (external review P2, indication
/// 10): `unpin` must clear the label so a later label-less `pin` cannot
/// resurrect a stale name. Runs from a live worktree, since `unpin`
/// discovers the current worktree via git.
#[test]
fn unpin_clears_label() {
    let env = TestEnv::new();
    env.write_config("range = [18800, 18809]\n");
    let repo = env.path("app");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());

    let entry = |env: &TestEnv| {
        env.registry()["projects"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()["worktrees"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone()
    };

    assert!(env
        .run(&repo, &["pin", "--label", "keep-me"])
        .status
        .success());
    let pinned = entry(&env);
    assert_eq!(pinned["pinned"], serde_json::json!(true));
    assert_eq!(pinned["label"], serde_json::json!("keep-me"));

    let out = env.run(&repo, &["unpin"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let unpinned = entry(&env);
    assert_eq!(unpinned["pinned"], serde_json::json!(false));
    assert_eq!(
        unpinned["label"],
        serde_json::json!(null),
        "unpin must clear the label so a later label-less pin can't resurrect it"
    );
}

#[test]
fn pin_without_allocation_is_an_error() {
    let env = TestEnv::new();
    let repo = env.path("app");
    init_repo(&repo);
    let out = env.run(&repo, &["pin"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("sync"));
}

/// Rewrites the ledger so `repo`'s worktree block is exactly `block`,
/// making grow/move scenarios deterministic despite hash-based placement.
fn pin_block(env: &TestEnv, repo: &Path, block: (u16, u16)) {
    let mut reg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.registry_path()).unwrap()).unwrap();
    reg["projects"][common_dir_key(repo)]["worktrees"][worktree_key(repo)]["block"] =
        serde_json::json!([block.0, block.1]);
    fs::write(env.registry_path(), serde_json::to_string(&reg).unwrap()).unwrap();
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

/// External review 3rd round P1-1: growing the manifest extends the current
/// block in place instead of hopping to a new hash-preferred slot.
#[test]
fn sync_grows_the_block_in_place_when_the_manifest_expands() {
    let env = TestEnv::new();
    env.write_config("range = [18900, 18919]\n");
    let repo = env.path("grow");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    pin_block(&env, &repo, (18900, 18904));

    // Grow to 10 ports (offset 9) -- must extend in place, keeping 18900.
    fs::write(repo.join(".portool.toml"), "[ports]\nweb = 0\nworker = 9\n").unwrap();
    let out = env.run(&repo, &["sync"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        block_of(&env, &repo),
        (18900, 18909),
        "in-place growth keeps the block start"
    );
}

/// When the extension is blocked by a neighbor and nothing is listening on
/// the current block, sync falls back to a clean move.
#[test]
fn sync_moves_when_growth_is_blocked_and_the_block_is_idle() {
    let env = TestEnv::new();
    env.write_config("range = [18920, 18939]\n");
    let repo = env.path("blocked");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    pin_block(&env, &repo, (18920, 18924));
    // 18925-18929 を予約して in-place 拡張を塞ぐ。
    assert!(env.run(&repo, &["reserve", "18925-18929"]).status.success());

    fs::write(repo.join(".portool.toml"), "[ports]\nweb = 0\nworker = 9\n").unwrap();
    let out = env.run(&repo, &["sync"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 10-wide, 5-aligned candidates: 18920 (reservation overlap), 18925
    // (overlap), 18930 -- the only clean landing spot.
    assert_eq!(
        block_of(&env, &repo),
        (18930, 18939),
        "blocked growth falls back to a move"
    );
}

/// Moving with a live listener on the current block is refused with an
/// actionable error (user decision: refuse + explicit action).
#[test]
fn sync_refuses_to_move_while_the_current_block_is_in_use() {
    let env = TestEnv::new();
    env.write_config("range = [18940, 18959]\n");
    let repo = env.path("busy");
    init_repo(&repo);
    assert!(env.run(&repo, &["sync"]).status.success());
    pin_block(&env, &repo, (18940, 18944));
    assert!(env.run(&repo, &["reserve", "18945-18949"]).status.success());
    // 現 block の先頭ポートで listener を立てて「稼働中」を再現。
    let _listener = std::net::TcpListener::bind(("127.0.0.1", 18940)).unwrap();

    fs::write(repo.join(".portool.toml"), "[ports]\nweb = 0\nworker = 9\n").unwrap();
    let out = env.run(&repo, &["sync"]);
    assert!(!out.status.success(), "must refuse the implicit move");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("portool reallocate"),
        "must name the explicit way out: {stderr}"
    );
    assert_eq!(
        block_of(&env, &repo),
        (18940, 18944),
        "the ledger must be unchanged after a refusal"
    );
}

/// External review 3rd round P1-4: a hostile label must not smuggle ANSI
/// escapes or extra lines into `ls` output.
#[test]
fn ls_sanitizes_control_characters_in_labels() {
    let env = TestEnv::new();
    env.write_config("range = [19300, 19399]\n");
    let repo = env.path("app");
    init_repo(&repo);

    let out = env.run(
        &repo,
        &["reserve", "19390", "--label", "evil\u{1b}[31m\nINJECTED"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.run(&repo, &["ls"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "no raw ESC may survive: {stdout:?}"
    );
    assert!(
        stdout.contains("evil\u{FFFD}[31m\\nINJECTED"),
        "controls must be visibly escaped: {stdout:?}"
    );
}
