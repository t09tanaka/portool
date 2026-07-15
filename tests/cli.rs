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
    let common_dir = fs::canonicalize(repo.join(".git")).unwrap();
    let worktree_root = fs::canonicalize(&repo).unwrap();
    assert_eq!(
        env_file,
        format!(
            "# generated by portool \u{2014} DO NOT EDIT\n\
             # block: 3000-3004  project: repo  worktree: {}\n\
             PORTOOL_PROJECT_ID={}\n\
             PORTOOL_WORKTREE_ID={}\n\
             PORT=3000\n",
            worktree_key(&repo),
            portool::identity::project_id(&common_dir),
            portool::identity::worktree_id(&common_dir, &worktree_root),
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

// --- 13. exit code 2: no subrange could ever fit even one block -----------

#[test]
fn sync_exits_2_when_block_size_exceeds_subrange_size() {
    let env = TestEnv::new();
    // No manifest -> default block size == block_align == 5, which exceeds
    // the configured subrange_size of 3: no subrange, however many are
    // acquired, could ever hold one block (frozen decision 4).
    env.write_config("range = [3000, 3009]\nsubrange_size = 3\nblock_align = 5\n");
    let repo = env.path("repo");
    init_repo(&repo);

    let output = env.run(&repo, &["sync"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// --- 14. exit code 3: the pool has no room for even one subrange ----------

#[test]
fn sync_exits_3_when_pool_has_no_room_for_a_subrange() {
    let env = TestEnv::new();
    // The pool is only 10 ports wide, far narrower than the requested
    // subrange_size of 500: the very first subrange acquisition fails.
    env.write_config("range = [3000, 3009]\nsubrange_size = 500\n");
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

// --- 15. prune --all reclaims a fully-deleted project's entry + subranges -

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

    // A real `prune --all` removes the dead project entry -- and with it
    // its subranges -- while leaving the surviving repo's entries alone.
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
        "the dead project entry (and its subranges) must be gone"
    );

    let repo_b_project = &registry["projects"][&repo_b_key];
    assert!(
        repo_b_project["worktrees"]
            .get(worktree_key(&repo_b))
            .is_some(),
        "the surviving project's worktree entry must be untouched"
    );
    // repo-b synced second, so its subrange is the pool's second slot
    // (3000-3499 was already claimed by repo-a).
    assert_eq!(
        repo_b_project["subranges"],
        serde_json::json!([[3500, 3999]]),
        "the surviving project's subranges must be untouched"
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
    assert_eq!(
        registry["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]["block"],
        serde_json::json!([3000, 3009]),
        "the expansion must actually reallocate the block"
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
