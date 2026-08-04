//! E2E: a linked worktree whose `core.hooksPath` was rewritten to the *main*
//! worktree's absolute hooks directory.
//!
//! Agent harnesses that create worktrees for you (Claude Code's task chips /
//! `EnterWorktree`, which check out under `.claude/worktrees/<name>`) copy the
//! repository's `core.hooksPath` into the new worktree's `config.worktree`,
//! absolutized -- git resolves a *relative* `core.hooksPath` against the
//! process's cwd, so a value like `.husky/tracked` would otherwise miss from a
//! linked worktree.
//!
//! Git happily runs those hooks for the linked worktree, so portool must
//! recognize them (`sync` must not nag, `doctor` must not call it uninstalled,
//! `init` must not fail) -- while still never *writing* into another
//! checkout's files, and still refusing a hooks dir that genuinely escapes the
//! repository.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// A repository plus the isolated `HOME`/XDG state every spawned `git` and
/// `portool` sees, so hooks firing during `git worktree add` can never reach
/// the developer's real ledger.
struct Env {
    _tmp: TempDir,
    home: PathBuf,
    repo: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        let env = Env {
            _tmp: tmp,
            home,
            repo,
        };
        env.git(&env.repo.clone(), &["init", "-q", "-b", "main"]);
        std::fs::write(env.repo.join("README.md"), "hello\n").unwrap();
        env.git(&env.repo.clone(), &["add", "README.md"]);
        env.git(&env.repo.clone(), &["commit", "-q", "-m", "init"]);
        env
    }

    /// A command with this test's isolated environment: portool's ledger goes
    /// to the temp `XDG_STATE_HOME`, and git ignores the machine's global and
    /// system config (which may itself set `core.hooksPath`).
    fn command(&self, program: &Path, dir: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(dir)
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        cmd
    }

    fn git(&self, dir: &Path, args: &[&str]) {
        let out = self
            .command(Path::new("git"), dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn portool(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(Path::new(env!("CARGO_BIN_EXE_portool")), dir)
            .args(args)
            .output()
            .unwrap()
    }

    /// Gives the repo a hook-manager-style *relative* `core.hooksPath`, the
    /// shape (Husky, lefthook, ...) an agent harness has to absolutize.
    fn use_relative_hooks_dir(&self) -> PathBuf {
        let hooks_dir = self.repo.join(".husky/tracked");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        self.git(&self.repo, &["config", "core.hooksPath", ".husky/tracked"]);
        hooks_dir
    }

    /// Creates the linked worktree an agent harness would, at the same
    /// `.claude/worktrees/<name>` location, and pins its `core.hooksPath` to
    /// `hooks_path` in worktree scope -- exactly what the harness writes into
    /// `.git/worktrees/<name>/config.worktree`.
    fn add_agent_worktree(&self, hooks_path: &Path) -> PathBuf {
        let worktree = self.repo.join(".claude/worktrees/agent");
        self.git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--no-track",
                "-b",
                "claude/agent",
                worktree.to_str().unwrap(),
            ],
        );
        self.git(&self.repo, &["config", "extensions.worktreeConfig", "true"]);
        self.git(
            &worktree,
            &[
                "config",
                "--worktree",
                "core.hooksPath",
                hooks_path.to_str().unwrap(),
            ],
        );
        worktree
    }
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The regression this file exists for: with the hook installed in the main
/// worktree and the linked worktree pointing at it absolutely, portool used to
/// classify it as a shared (out-of-repo) hooks dir -- so `sync` nagged to run
/// `init`, `doctor` reported "no installable location", and `init` exited
/// non-zero, all while the hook was in fact installed and running.
#[test]
fn hooks_in_the_main_worktree_are_recognized_from_a_linked_worktree() {
    let env = Env::new();
    let hooks_dir = env.use_relative_hooks_dir();
    assert!(
        env.portool(&env.repo, &["init", "--hook-only"])
            .status
            .success(),
        "setup: init in the main worktree must succeed"
    );
    let worktree = env.add_agent_worktree(&hooks_dir);

    let sync = env.portool(&worktree, &["sync"]);
    assert!(sync.status.success(), "sync failed: {}", stderr_of(&sync));
    assert!(
        !stderr_of(&sync).contains("portool init"),
        "sync must not nag about an already-installed hook, got: {}",
        stderr_of(&sync)
    );

    let doctor = env.portool(&worktree, &["doctor"]);
    assert!(
        !stdout_of(&doctor).contains("no installable location"),
        "doctor must follow the main worktree's hooks, got: {}",
        stdout_of(&doctor)
    );
    assert!(
        !stdout_of(&doctor).contains("not installed"),
        "doctor must see the installed hook, got: {}",
        stdout_of(&doctor)
    );

    let init = env.portool(&worktree, &["init", "--hook-only"]);
    assert!(
        init.status.success(),
        "init must be a no-op success when the hook is already there: {}",
        stderr_of(&init)
    );
    assert!(
        stderr_of(&init).contains("main worktree"),
        "init must say where the hook lives, got: {}",
        stderr_of(&init)
    );
}

/// `init` from the linked worktree must never write the hook into the main
/// worktree's checkout (a tracked `.husky/...` file belongs to whoever is
/// working there); it fails closed and points at that worktree instead.
#[test]
fn init_in_a_linked_worktree_never_writes_into_the_main_worktree() {
    let env = Env::new();
    let hooks_dir = env.use_relative_hooks_dir();
    let worktree = env.add_agent_worktree(&hooks_dir);

    let out = env.portool(&worktree, &["init", "--hook-only"]);

    assert!(
        !hooks_dir.join("post-checkout").exists(),
        "init wrote into the main worktree's hooks dir"
    );
    assert!(
        !out.status.success(),
        "init must fail closed when nothing was installed"
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("main worktree") && stderr.contains("portool init"),
        "init must point at the worktree to run it in, got: {stderr}"
    );
}

/// The relaxation is scoped to *this repository's* main worktree: an absolute
/// `core.hooksPath` pointing anywhere else is still refused, so a linked
/// worktree can't become a way around the shared-scope guard.
#[test]
fn hooks_path_outside_the_repository_is_still_refused_from_a_linked_worktree() {
    let env = Env::new();
    let outside = env.home.join("shared-hooks");
    std::fs::create_dir_all(&outside).unwrap();
    let worktree = env.add_agent_worktree(&outside);

    let out = env.portool(&worktree, &["init", "--hook-only"]);

    assert!(
        std::fs::read_dir(&outside).unwrap().next().is_none(),
        "portool wrote outside the repository"
    );
    assert!(!out.status.success(), "init must fail closed");
    assert!(
        stderr_of(&out).contains("resolves outside this repository"),
        "must keep the shared-scope refusal, got: {}",
        stderr_of(&out)
    );
}

/// Only an *absolute* value is followed into the main worktree. Git resolves a
/// relative `core.hooksPath` against the process's cwd rather than a fixed
/// root, so a `../`-escape has no single settled meaning -- portool refuses
/// rather than guessing which directory git will read.
#[test]
fn relative_hooks_path_escaping_into_the_main_worktree_is_still_refused() {
    let env = Env::new();
    let hooks_dir = env.use_relative_hooks_dir();
    let worktree = env.add_agent_worktree(&hooks_dir);
    // `.claude/worktrees/agent` -> repo root is three levels up.
    env.git(
        &worktree,
        &[
            "config",
            "--worktree",
            "core.hooksPath",
            "../../../.husky/tracked",
        ],
    );

    let out = env.portool(&worktree, &["init", "--hook-only"]);

    assert!(
        !hooks_dir.join("post-checkout").exists(),
        "a relative escape must not be followed into another checkout"
    );
    assert!(!out.status.success(), "init must fail closed");
    assert!(
        stderr_of(&out).contains("resolves outside this repository"),
        "must keep the shared-scope refusal, got: {}",
        stderr_of(&out)
    );
}

/// `unhook` can't remove a hook that lives in another checkout, so it must
/// report the leftover instead of claiming success -- and say where to run it.
#[test]
fn unhook_in_a_linked_worktree_reports_the_main_worktree_hook_as_residue() {
    let env = Env::new();
    let hooks_dir = env.use_relative_hooks_dir();
    assert!(
        env.portool(&env.repo, &["init", "--hook-only"])
            .status
            .success(),
        "setup: init in the main worktree must succeed"
    );
    let worktree = env.add_agent_worktree(&hooks_dir);

    let out = env.portool(&worktree, &["unhook"]);

    assert!(
        hooks_dir.join("post-checkout").exists(),
        "unhook removed another checkout's hook"
    );
    assert!(
        !out.status.success(),
        "unhook must not report success while the hook still runs"
    );
    assert!(
        stdout_of(&out).contains("partial_unhook"),
        "must report the residue, got: {}",
        stdout_of(&out)
    );
    assert!(
        !stdout_of(&out).contains("no portool hooks found"),
        "the summary must not claim nothing was found while reporting residue, got: {}",
        stdout_of(&out)
    );
    assert!(
        stderr_of(&out).contains("main worktree"),
        "must say where to run unhook, got: {}",
        stderr_of(&out)
    );
}

/// `init` deciding "already installed" (exit 0) must use the same line-exact
/// test as a real install, not the loose substring heuristic `sync`'s nag
/// uses -- otherwise a hook that merely *mentions* portool in a comment would
/// make `init` report success while nothing actually invokes portool.
#[test]
fn a_hook_that_only_mentions_portool_does_not_count_as_installed() {
    let env = Env::new();
    let hooks_dir = env.use_relative_hooks_dir();
    std::fs::write(
        hooks_dir.join("post-checkout"),
        "#!/bin/sh\n# TODO: portool sync --quiet here one day\nexit 0\n",
    )
    .unwrap();
    let worktree = env.add_agent_worktree(&hooks_dir);

    let out = env.portool(&worktree, &["init", "--hook-only"]);

    assert!(
        !out.status.success(),
        "a commented-out mention must not read as installed: {}",
        stderr_of(&out)
    );
    assert!(
        stderr_of(&out).contains("main worktree"),
        "must point at the worktree to install in, got: {}",
        stderr_of(&out)
    );
}

/// The relaxation is for a repository's *own* per-repo config. A `global`
/// `core.hooksPath` is a directory every repository on the machine shares, so
/// it keeps the scope-carrying refusal v0.9.0 made scope-independent -- even
/// when it happens to point inside this repository's main worktree.
#[test]
fn a_global_hooks_path_into_the_main_worktree_keeps_the_shared_scope_refusal() {
    let env = Env::new();
    let hooks_dir = env.use_relative_hooks_dir();
    let worktree = env.add_agent_worktree(&hooks_dir);
    // Drop the per-worktree value and set the same path globally instead.
    env.git(
        &worktree,
        &["config", "--worktree", "--unset", "core.hooksPath"],
    );
    env.git(&env.repo, &["config", "--unset", "core.hooksPath"]);
    let global_config = env.home.join("gitconfig");
    std::fs::write(
        &global_config,
        format!("[core]\n\thooksPath = {}\n", hooks_dir.display()),
    )
    .unwrap();

    let out = env
        .command(Path::new(env!("CARGO_BIN_EXE_portool")), &worktree)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .args(["init", "--hook-only"])
        .output()
        .unwrap();

    assert!(
        !hooks_dir.join("post-checkout").exists(),
        "a global hooks dir must never be installed into"
    );
    assert!(!out.status.success(), "init must fail closed");
    assert!(
        stderr_of(&out).contains("resolves outside this repository"),
        "must keep the scope-independent refusal, got: {}",
        stderr_of(&out)
    );
}
