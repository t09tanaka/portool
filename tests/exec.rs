//! End-to-end tests for `portool exec` (docs/spec-v0.4.md).
//!
//! Every test gets its own [`TestEnv`]: a temp directory supplying
//! `HOME`/`XDG_STATE_HOME`/`XDG_CONFIG_HOME` to the spawned binary via
//! [`Command::env`] (never via `std::env::set_var`, which would leak into
//! this process and every other test). The real `~/.local/state` is never
//! touched.
//!
//! Success-path assertions never require an exact stderr match: `exec` runs
//! a sync internally, which may print the hook-missing hint (spec v0.3 §12).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// `TestEnv` and `git()` are duplicated from tests/cli.rs: integration tests
// are separate crates and cannot share code.

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

/// The manifest every exec test repo uses: two contiguous ports.
const MANIFEST: &str = "[ports]\nweb = 0\ndb = 1\n";

/// Creates a git repository with a single empty commit and no manifest.
fn init_bare_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    assert!(git(dir, &["init", "-q", "-b", "main"]).status.success());
    assert!(git(
        dir,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    )
    .status
    .success());
}

/// Creates a git repository carrying the standard two-port manifest.
fn init_repo(dir: &Path) {
    init_bare_repo(dir);
    fs::write(dir.join(".portool.toml"), MANIFEST).unwrap();
}

/// Like [`TestEnv::run`], but with extra parent-process environment
/// variables injected on top of the isolated base environment.
fn run_with_env(env: &TestEnv, dir: &Path, vars: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = env.command();
    cmd.current_dir(dir);
    for (key, value) in vars {
        cmd.env(key, value);
    }
    cmd.args(args).output().expect("failed to spawn portool")
}

/// The first port of the block allocated to `worktree`, read from the
/// registry (the repository's common dir is `repo/.git`).
fn block_start(env: &TestEnv, repo: &Path, worktree: &Path) -> u64 {
    env.registry()["projects"][common_dir_key(repo)]["worktrees"][worktree_key(worktree)]["block"]
        [0]
    .as_u64()
    .expect("block start must be a number")
}

/// Asserts success and returns the child's stdout as a `String`.
fn success_stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "expected success, got {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// --- 1. port injection ------------------------------------------------------

/// Spec §5/§6: `exec` syncs by itself (no prior `portool sync` needed) and
/// injects each `[ports]` entry as `<KEY>_PORT` into the child.
#[test]
fn exec_injects_allocated_ports_into_child() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let output = env.run(
        &repo,
        &[
            "exec",
            "--",
            "sh",
            "-c",
            r#"printf "%s %s" "$WEB_PORT" "$DB_PORT""#,
        ],
    );
    let stdout = success_stdout(&output);

    let start = block_start(&env, &repo, &repo);
    assert_eq!(
        stdout,
        format!("{} {}", start, start + 1),
        "WEB_PORT must be the block start and DB_PORT its offset-1 neighbour"
    );
}

// --- 2. no env files --------------------------------------------------------

/// Spec §8: with no `--env-file`, exec runs on just the parent environment
/// plus the portool allocation.
#[test]
fn exec_runs_without_env_files() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let output = env.run(&repo, &["exec", "--", "true"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// --- 3. single env file -----------------------------------------------------

/// Spec §4/§8: a single `--env-file` provides its variables to the child.
#[test]
fn exec_single_env_file_provides_variables() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join(".env.test"), "FOO=bar\n").unwrap();

    let output = env.run(
        &repo,
        &[
            "exec",
            "--env-file",
            ".env.test",
            "--",
            "sh",
            "-c",
            r#"printf "%s" "$FOO""#,
        ],
    );
    assert_eq!(success_stdout(&output), "bar");
}

// --- 4. multiple env files: last one wins ------------------------------------

/// Spec §6: a later env file overrides an earlier one (also exercises the
/// short `-e` alias).
#[test]
fn exec_later_env_file_overrides_earlier_one() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join("one.env"), "FOO=a\n").unwrap();
    fs::write(repo.join("two.env"), "FOO=b\n").unwrap();

    let output = env.run(
        &repo,
        &[
            "exec",
            "--env-file",
            "one.env",
            "-e",
            "two.env",
            "--",
            "sh",
            "-c",
            r#"printf "%s" "$FOO""#,
        ],
    );
    assert_eq!(success_stdout(&output), "b");
}

// --- 5. parent environment beats env files -----------------------------------

/// Spec §6: the parent process environment overrides env-file values.
#[test]
fn exec_parent_env_overrides_env_file() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join(".env.test"), "FOO=file\n").unwrap();

    let output = run_with_env(
        &env,
        &repo,
        &[("FOO", "parent")],
        &[
            "exec",
            "--env-file",
            ".env.test",
            "--",
            "sh",
            "-c",
            r#"printf "%s" "$FOO""#,
        ],
    );
    assert_eq!(success_stdout(&output), "parent");
}

/// Spec §6: a parent variable whose value is not valid UTF-8 still beats
/// the env file, and the child inherits its exact bytes (exec never
/// re-sets parent-sourced winners).
#[test]
fn exec_non_utf8_parent_value_beats_env_file_and_survives_byte_exact() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join(".env.test"), "FOO=file\n").unwrap();

    let raw: &[u8] = b"caf\xe9-raw"; // invalid UTF-8 (lone 0xE9)
    let mut cmd = env.command();
    cmd.current_dir(&repo)
        .env(OsStr::from_bytes(b"FOO"), OsStr::from_bytes(raw))
        .args([
            "exec",
            "--env-file",
            ".env.test",
            "--",
            "sh",
            "-c",
            r#"printf "%s" "$FOO""#,
        ]);
    let output = cmd.output().expect("failed to spawn portool");

    assert!(output.status.success(), "exec must succeed");
    assert_eq!(output.stdout, raw, "child must inherit the exact bytes");
}

// --- 6. portool-managed variables beat everything ----------------------------

/// Spec §6: portool-managed variables override both a stale parent value
/// and a stale env-file value.
#[test]
fn exec_portool_vars_override_parent_env_and_env_file() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join(".env.test"), "WEB_PORT=8888\n").unwrap();

    let output = run_with_env(
        &env,
        &repo,
        &[("WEB_PORT", "9999")],
        &[
            "exec",
            "--env-file",
            ".env.test",
            "--",
            "sh",
            "-c",
            r#"printf "%s" "$WEB_PORT""#,
        ],
    );
    let stdout = success_stdout(&output);

    let start = block_start(&env, &repo, &repo);
    assert_eq!(
        stdout,
        start.to_string(),
        "the current allocation must win over stale WEB_PORT values"
    );
}

// --- 7. ${NAME} expansion -----------------------------------------------------

/// Spec §7: `${NAME}` in an env-file value expands against the final
/// variable set, so an allocated port can be embedded in a URL.
#[test]
fn exec_expands_braced_variable_references() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(
        repo.join(".env.test"),
        "URL=http://localhost:${WEB_PORT}/x\n",
    )
    .unwrap();

    let output = env.run(
        &repo,
        &[
            "exec",
            "--env-file",
            ".env.test",
            "--",
            "sh",
            "-c",
            r#"printf "%s" "$URL""#,
        ],
    );
    let stdout = success_stdout(&output);

    let start = block_start(&env, &repo, &repo);
    assert_eq!(stdout, format!("http://localhost:{start}/x"));
}

// --- 8. ${NAME:-default} expansion --------------------------------------------

/// Spec §7: `${NAME:-default}` uses the default when NAME is undefined or
/// empty, and the real value when NAME is set.
#[test]
fn exec_default_expansion_covers_defined_undefined_and_empty() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(
        repo.join(".env.test"),
        "DEFINED=real\n\
         EMPTY=\n\
         A=${DEFINED:-fallback}\n\
         B=${NEVER_DEFINED:-fallback}\n\
         C=${EMPTY:-fallback}\n",
    )
    .unwrap();

    let output = env.run(
        &repo,
        &[
            "exec",
            "--env-file",
            ".env.test",
            "--",
            "sh",
            "-c",
            r#"printf "%s %s %s" "$A" "$B" "$C""#,
        ],
    );
    assert_eq!(success_stdout(&output), "real fallback fallback");
}

// --- 9. recursive references inside one env file -------------------------------

/// Spec §7: env-file values can reference other variables from the same
/// file, transitively.
#[test]
fn exec_expands_recursive_references_within_env_file() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join(".env.test"), "A=1\nB=${A}2\nC=${B}3\n").unwrap();

    let output = env.run(
        &repo,
        &[
            "exec",
            "--env-file",
            ".env.test",
            "--",
            "sh",
            "-c",
            r#"printf "%s" "$C""#,
        ],
    );
    assert_eq!(success_stdout(&output), "123");
}

// --- 10. undefined ${NAME} is an error ------------------------------------------

/// Spec §7/§10: an undefined `${NAME}` reference fails the whole exec and
/// the child is never started.
#[test]
fn exec_undefined_variable_errors_and_child_never_runs() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join(".env.test"), "X=${NOPE}\n").unwrap();

    let output = env.run(
        &repo,
        &["exec", "--env-file", ".env.test", "--", "touch", "marker"],
    );

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("undefined variable"),
        "stderr was: {stderr}"
    );
    assert!(
        !repo.join("marker").exists(),
        "the child must not run when env construction fails"
    );
}

// --- 11. circular references are an error ----------------------------------------

/// Spec §7/§10: a reference cycle between env-file variables is an error.
#[test]
fn exec_circular_reference_is_an_error() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join(".env.test"), "A=${B}\nB=${A}\n").unwrap();

    let output = env.run(&repo, &["exec", "--env-file", ".env.test", "--", "true"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("circular"), "stderr was: {stderr}");
}

// --- 12. single quotes suppress expansion ------------------------------------------

/// Spec §7: single-quoted values are passed through without expansion.
#[test]
fn exec_single_quoted_values_are_not_expanded() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join(".env.test"), "X='${WEB_PORT}'\n").unwrap();

    let output = env.run(
        &repo,
        &[
            "exec",
            "--env-file",
            ".env.test",
            "--",
            "sh",
            "-c",
            r#"printf "%s" "$X""#,
        ],
    );
    assert_eq!(success_stdout(&output), "${WEB_PORT}");
}

// --- 13. no shell interpretation ----------------------------------------------------

/// Spec §3/§9: exec never routes through a shell -- argv is handed to the
/// OS verbatim, and command substitution in env values is never executed.
#[test]
fn exec_does_not_evaluate_shell_syntax() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    // Shell metacharacters in an argument arrive at the child as one
    // literal argument.
    let output = env.run(&repo, &["exec", "--", "printf", "%s", "hello && rm -rf /"]);
    assert_eq!(success_stdout(&output), "hello && rm -rf /");

    // `$()` in an env-file value is passed through literally, not run.
    fs::write(repo.join(".env.test"), "X=$(whoami)\n").unwrap();
    let output = env.run(
        &repo,
        &[
            "exec",
            "--env-file",
            ".env.test",
            "--",
            "sh",
            "-c",
            r#"printf "%s" "$X""#,
        ],
    );
    assert_eq!(success_stdout(&output), "$(whoami)");
}

// --- 14. exit code passthrough --------------------------------------------------------

/// Spec §9: the child's exit code is returned unchanged.
#[test]
fn exec_propagates_child_exit_code() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let output = env.run(&repo, &["exec", "--", "sh", "-c", "exit 7"]);
    assert_eq!(output.status.code(), Some(7));
}

// --- 15. command not found = 127 --------------------------------------------------------

/// Spec §9: a nonexistent command exits 127.
#[test]
fn exec_missing_command_exits_127() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let output = env.run(&repo, &["exec", "--", "portool-no-such-command-xyz"]);
    assert_eq!(output.status.code(), Some(127));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("command not found"), "stderr was: {stderr}");
}

// --- 16. permission denied = 126 -----------------------------------------------------------

/// Spec §9: a file without the execute bit exits 126.
#[test]
fn exec_non_executable_file_exits_126() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    let script = repo.join("noexec");
    fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();

    let output = env.run(&repo, &["exec", "--", "./noexec"]);
    assert_eq!(output.status.code(), Some(126));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot execute"), "stderr was: {stderr}");
}

// --- 17. process replacement (exec(2)) --------------------------------------------------------

/// Spec §9: on Unix, portool replaces itself with the child via exec(2),
/// so the pid we spawned IS the child's pid (which is what makes signal
/// passthrough automatic).
#[test]
fn exec_replaces_its_own_process_with_the_child() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let mut cmd = env.command();
    cmd.current_dir(&repo)
        .args(["exec", "--", "sh", "-c", r#"printf "%s" $$"#])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().expect("failed to spawn portool exec");
    let spawned_pid = child.id();
    let output = child.wait_with_output().expect("failed to wait for child");

    let stdout = success_stdout(&output);
    assert_eq!(
        stdout,
        spawned_pid.to_string(),
        "the shell's $$ must equal the pid portool was spawned with"
    );
}

// --- 18. SIGTERM passthrough --------------------------------------------------------------------

/// Spec §9: signals sent to the spawned pid reach the child naturally
/// (because of the exec(2) replacement) -- SIGTERM triggers the child's
/// own trap handler.
#[test]
fn exec_forwards_sigterm_to_the_child() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    let ready = repo.join("ready");

    let mut cmd = env.command();
    cmd.current_dir(&repo)
        .args([
            "exec",
            "--",
            "sh",
            "-c",
            "trap 'exit 42' TERM; : > ready; while :; do sleep 0.05; done",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("failed to spawn portool exec");

    // Poll (rather than sleep a fixed amount) until the shell has installed
    // its trap and signalled readiness by creating the file.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            panic!("portool exec exited before the child was ready: {status:?}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("timed out waiting for the child to create the ready file");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let kill = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .output()
        .expect("failed to run kill");
    assert!(
        kill.status.success(),
        "kill -TERM failed: {}",
        String::from_utf8_lossy(&kill.stderr)
    );

    let status = child.wait().expect("failed to wait for child");
    assert_eq!(
        status.code(),
        Some(42),
        "SIGTERM must reach the child's trap handler"
    );
}

// --- 19. parse errors never leak values -----------------------------------------------------------

/// Spec §8/§10: an env-file parse error reports the file and line number
/// but never echoes the (possibly secret) value.
#[test]
fn exec_env_parse_error_reports_location_without_leaking_values() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join("bad.env"), "SECRET=\"hunter2-super-secret\n").unwrap();

    let output = env.run(&repo, &["exec", "--env-file", "bad.env", "--", "true"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bad.env") && stderr.contains(":1"),
        "the error must point at file and line, stderr was: {stderr}"
    );
    assert!(
        !stderr.contains("hunter2"),
        "the value must never be echoed, stderr was: {stderr}"
    );
}

// --- 20. two real worktrees, two allocations ---------------------------------------------------------

/// Spec §5/§6: two worktrees of one project get distinct blocks, and the
/// same env file expands to a different URL in each of them.
#[test]
fn exec_in_two_worktrees_injects_distinct_ports_and_urls() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

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
    // The manifest is untracked in this fixture, so the linked worktree
    // needs its own copy.
    fs::write(wt.join(".portool.toml"), MANIFEST).unwrap();

    let env_file = "URL=db:${WEB_PORT}\n";
    fs::write(repo.join(".env.test"), env_file).unwrap();
    fs::write(wt.join(".env.test"), env_file).unwrap();

    let args = [
        "exec",
        "--env-file",
        ".env.test",
        "--",
        "sh",
        "-c",
        r#"printf "%s" "$URL""#,
    ];
    let out_main = env.run(&repo, &args);
    let url_main = success_stdout(&out_main);
    let out_wt = env.run(&wt, &args);
    let url_wt = success_stdout(&out_wt);

    let start_main = block_start(&env, &repo, &repo);
    let start_wt = block_start(&env, &repo, &wt);
    assert_ne!(start_main, start_wt, "each worktree must get its own block");
    assert_eq!(url_main, format!("db:{start_main}"));
    assert_eq!(url_wt, format!("db:{start_wt}"));
    assert_ne!(url_main, url_wt);
}

// --- 21. outside a git repository -----------------------------------------------------------------------

/// Spec §10: exec outside any git worktree fails without starting the
/// child.
#[test]
fn exec_outside_git_repo_exits_1() {
    let env = TestEnv::new();
    let dir = env.path("not-a-repo");
    fs::create_dir_all(&dir).unwrap();

    let output = env.run(&dir, &["exec", "--", "true"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not inside a git repository"),
        "stderr was: {stderr}"
    );
}

// --- 22. no .portool.toml is not an error ----------------------------------------------------------------------------

/// P1-5 (external review): exec must work with no .portool.toml, injecting
/// the fallback PORT variable -- the README's no-manifest promise applies
/// to exec too.
#[test]
fn exec_without_manifest_injects_port() {
    let env = TestEnv::new();
    env.write_config("range = [17400, 17499]\n");
    let repo = env.path("app");
    init_bare_repo(&repo); // no .portool.toml written

    let out = env.run(&repo, &["exec", "--", "sh", "-c", "echo PORT=$PORT"]);
    assert!(
        out.status.success(),
        "exec must not require a manifest, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("PORT=174"),
        "PORT must be injected from the allocated block, got: {stdout}"
    );
}

// --- 23. relative env files resolve against the CWD ---------------------------------------------------------

/// Spec §8: a relative `--env-file` path is resolved from the current
/// directory (here a subdirectory), not from the worktree root -- while
/// port injection still works from anywhere inside the worktree.
#[test]
fn exec_resolves_relative_env_file_from_cwd_subdirectory() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);
    let sub = repo.join("sub");
    fs::create_dir_all(&sub).unwrap();
    // A decoy at the worktree root proves the CWD-relative file is chosen.
    fs::write(repo.join("rel.env"), "GREETING=from-root\n").unwrap();
    fs::write(sub.join("rel.env"), "GREETING=from-sub\n").unwrap();

    let output = env.run(
        &sub,
        &[
            "exec",
            "--env-file",
            "rel.env",
            "--",
            "sh",
            "-c",
            r#"printf "%s %s" "$GREETING" "$WEB_PORT""#,
        ],
    );
    let stdout = success_stdout(&output);

    let start = block_start(&env, &repo, &repo);
    assert_eq!(stdout, format!("from-sub {start}"));
}

// --- 24. missing env file is an error -------------------------------------------------------------------------

/// Spec §8/§10: an explicitly named env file that doesn't exist is an
/// error, and the child is never started.
#[test]
fn exec_missing_env_file_exits_1() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let output = env.run(
        &repo,
        &["exec", "--env-file", "no-such.env", "--", "touch", "marker"],
    );

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("env file not found"),
        "stderr was: {stderr}"
    );
    assert!(
        !repo.join("marker").exists(),
        "the child must not run when an env file is missing"
    );
}

// --- 25. missing command is a usage error ------------------------------------------------------------------------

/// Spec §4: `--` is mandatory and a command must follow it; both omissions
/// are clap usage errors. Batch B #15 moved clap usage errors off exit code
/// 2 (which collided with a semantic error) onto the dedicated code 64.
#[test]
fn exec_without_command_is_a_usage_error() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    for args in [&["exec"][..], &["exec", "--"][..]] {
        let output = env.run(&repo, args);
        assert_eq!(
            output.status.code(),
            Some(64),
            "`portool {}` must be a usage error (exit 64)",
            args.join(" ")
        );
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(stderr.contains("usage"), "stderr was: {stderr}");
    }
}

// --- 26. the committed examples/webapp fixture ---------------------------------

/// README "Keep your repo portool-free": the committed example's manifest
/// and `.env.test` really serve both worlds. Through `exec`, DATABASE_URL
/// expands to this worktree's own `test_db` port; and the file keeps a
/// `:-` default so a plain dotenv loader (CI, no portool) still resolves
/// it. The example can't be exec'd in place -- it sits inside the portool
/// repo, whose root has no manifest -- so its files are copied into a
/// scratch repo.
#[test]
fn exec_examples_webapp_env_test_expands_to_allocated_port() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_bare_repo(&repo);

    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/webapp");
    for file in [".portool.toml", ".env.test"] {
        fs::copy(example.join(file), repo.join(file)).expect(file);
    }

    let output = env.run(
        &repo,
        &[
            "exec",
            "--env-file",
            ".env.test",
            "--",
            "sh",
            "-c",
            r#"printf "%s" "$DATABASE_URL""#,
        ],
    );
    let stdout = success_stdout(&output);

    // `test_db` is declared at offset 2 in the example manifest.
    let test_db = block_start(&env, &repo, &repo) + 2;
    assert!(
        stdout.contains(&format!(":{test_db}/")),
        "DATABASE_URL must embed this worktree's test_db port, got: {stdout}"
    );

    // The other half of the contract: without portool the same committed
    // file must keep working as plain dotenv, so the port reference has
    // to carry a fallback default.
    let env_test = fs::read_to_string(example.join(".env.test")).unwrap();
    assert!(
        env_test.contains("${TEST_DB_PORT:-"),
        ".env.test must keep a fallback default for portool-free environments"
    );
}

// --- 27. bind check is opt-in (v0.8.0) --------------------------------------

/// Rewrites the ledger so `repo`'s worktree block is the single-port block
/// `(port, port)`. Combined with a held ephemeral port, this creates a
/// deterministic bind conflict at the execution boundary -- avoiding the race
/// of binding a *predicted* port that a parallel test might grab first.
/// Duplicated from tests/cli.rs: integration tests are separate crates and
/// cannot share code.
fn pin_block_to_port(env: &TestEnv, repo: &Path, port: u16) {
    let mut reg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.registry_path()).unwrap()).unwrap();
    reg["projects"][common_dir_key(repo)]["worktrees"][worktree_key(repo)]["block"] =
        serde_json::json!([port, port]);
    fs::write(env.registry_path(), serde_json::to_string(&reg).unwrap()).unwrap();
}

/// Sets up a repo with a single-port manifest, synced, then pinned onto a
/// held ephemeral port so the block is guaranteed to conflict. Returns the
/// held listener (keep it alive for the duration of the test) and the port.
fn repo_with_pinned_conflict(env: &TestEnv) -> (PathBuf, std::net::TcpListener, u16) {
    env.write_config("range = [19000, 19099]\n");
    let repo = env.path("repo");
    init_bare_repo(&repo);
    fs::write(repo.join(".portool.toml"), "[ports]\nweb = 0\n").unwrap();
    assert!(env.run(&repo, &["sync"]).status.success());

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    pin_block_to_port(env, &repo, port);

    (repo, listener, port)
}

/// v0.8.0: exec no longer bind-checks by default -- running a second
/// process of the same worktree must be silent (external review 3rd round
/// P1-2).
#[test]
fn exec_is_silent_by_default_even_when_block_ports_are_bound() {
    let env = TestEnv::new();
    let (repo, _listener, _port) = repo_with_pinned_conflict(&env);

    let output = env.run(&repo, &["exec", "--", "sh", "-c", "true"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("in use"),
        "default exec must not bind-check, stderr was: {stderr}"
    );
}

/// `--check-ports` opts back into the advisory bind check.
#[test]
fn exec_check_ports_warns_when_block_ports_are_bound() {
    let env = TestEnv::new();
    let (repo, _listener, _port) = repo_with_pinned_conflict(&env);

    let output = env.run(&repo, &["exec", "--check-ports", "--", "sh", "-c", "true"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("in use"),
        "--check-ports must warn on a conflict, stderr was: {stderr}"
    );
}

/// `--strict` implies the bind check and fails hard on a conflict, without
/// needing `--check-ports` alongside it.
#[test]
fn exec_strict_fails_on_conflict_without_needing_check_ports() {
    let env = TestEnv::new();
    let (repo, _listener, _port) = repo_with_pinned_conflict(&env);

    let output = env.run(&repo, &["exec", "--strict", "--", "sh", "-c", "true"]);
    assert!(
        !output.status.success(),
        "--strict must fail on a port conflict"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("in use"), "stderr was: {stderr}");
}

/// `--reallocate-on-conflict` implies the bind check, moves the worktree off
/// the occupied block, and warns about the risk of ending up split across
/// old and new blocks (processes already running keep the old ports).
#[test]
fn exec_reallocate_on_conflict_moves_and_warns_about_split() {
    let env = TestEnv::new();
    let (repo, _listener, port) = repo_with_pinned_conflict(&env);

    let output = env.run(
        &repo,
        &["exec", "--reallocate-on-conflict", "--", "sh", "-c", "true"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("split"),
        "--reallocate-on-conflict must warn about a possible split, stderr was: {stderr}"
    );

    let block = env.registry()["projects"][common_dir_key(&repo)]["worktrees"][worktree_key(&repo)]
        ["block"]
        .clone();
    assert_ne!(
        block,
        serde_json::json!([port, port]),
        "--reallocate-on-conflict must move off the occupied block"
    );
}

// --- 28. --env-file-overrides lets files beat the parent (v0.8.0) -----------

/// External review: `portool exec -e .env.test -- npm test` was silently
/// losing to a stale parent `DATABASE_URL`. `--env-file-overrides` flips the
/// parent/file precedence (parent < files < portool) so an explicitly
/// passed env file wins; without the flag, the parent still wins (spec §6
/// regression check).
#[test]
fn exec_env_file_overrides_beats_the_parent_environment() {
    let env = TestEnv::new();
    env.write_config("range = [19200, 19299]\n");
    let repo = env.path("repo");
    init_repo(&repo);
    fs::write(repo.join(".env.test"), "DATABASE_URL=file-value\n").unwrap();

    let output = run_with_env(
        &env,
        &repo,
        &[("DATABASE_URL", "parent-value")],
        &[
            "exec",
            "--env-file-overrides",
            "--env-file",
            ".env.test",
            "--",
            "sh",
            "-c",
            r#"printf "%s" "$DATABASE_URL""#,
        ],
    );
    assert_eq!(
        success_stdout(&output),
        "file-value",
        "--env-file-overrides must let the env file beat the parent"
    );

    // Regression: without the flag, the parent still wins (current default).
    let output = run_with_env(
        &env,
        &repo,
        &[("DATABASE_URL", "parent-value")],
        &[
            "exec",
            "--env-file",
            ".env.test",
            "--",
            "sh",
            "-c",
            r#"printf "%s" "$DATABASE_URL""#,
        ],
    );
    assert_eq!(
        success_stdout(&output),
        "parent-value",
        "without the flag, the parent must still win"
    );
}

// --- 29. --strict and --reallocate-on-conflict are mutually exclusive --------

/// Task 8: `--strict` and `--reallocate-on-conflict` cannot be used together.
/// This is a clap usage error that exits 64 (EX_USAGE).
#[test]
fn strict_conflicts_with_reallocate_on_conflict() {
    let env = TestEnv::new();
    let repo = env.path("repo");
    init_repo(&repo);

    let output = env.run(
        &repo,
        &["exec", "--strict", "--reallocate-on-conflict", "--", "true"],
    );

    assert_eq!(
        output.status.code(),
        Some(64),
        "using --strict with --reallocate-on-conflict must be a usage error (exit 64)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("conflict") || stderr.contains("cannot"),
        "stderr should mention a conflict, was: {stderr}"
    );
}
