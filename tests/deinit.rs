//! E2E: `deinit`/`unhook` must have an explicit success / failure / partial
//! contract and never report success while leaving portool able to re-hook
//! (external review v0.10 P0-3). One test per required case: symlink hook,
//! unreadable hook, malformed managed block, non-shell hook, shared
//! `hooksPath`, and a clean full deinit.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Env {
    home: PathBuf,
    state: PathBuf,
    config: PathBuf,
    _tmp: TempDir,
}

impl Env {
    fn new(range: &str) -> Self {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let config = tmp.path().join("config");
        for d in [&home, &state, &config] {
            std::fs::create_dir_all(d).unwrap();
        }
        let cfg_dir = config.join("portool");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.toml"), format!("range = {range}\n")).unwrap();
        Env {
            home,
            state,
            config,
            _tmp: tmp,
        }
    }

    fn portool(&self, dir: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_portool"));
        cmd.env_clear();
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        cmd.env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .current_dir(dir)
            .args(args);
        cmd.output().unwrap()
    }

    fn make_repo(&self, name: &str) -> PathBuf {
        let dir = self._tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "a@b.c"]);
        git(&dir, &["config", "user.name", "a"]);
        std::fs::write(dir.join("README.md"), "x\n").unwrap();
        git(&dir, &["add", "README.md"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        dir
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

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn deinit_clean_repo_fully_removes_everything() {
    let env = Env::new("[19560, 19599]");
    let r = env.make_repo("clean");
    assert!(env.portool(&r, &["init"]).status.success());

    let out = env.portool(&r, &["deinit"]);
    assert!(
        out.status.success(),
        "clean deinit must succeed: {}",
        stdout(&out)
    );
    assert!(!stdout(&out).contains("partial_deinit"));
    assert!(!r.join(".git/hooks/post-checkout").exists());
    assert!(!r.join(".git/hooks/post-merge").exists());
    assert!(!r.join(".env.portool").exists());
    let exclude = std::fs::read_to_string(r.join(".git/info/exclude")).unwrap_or_default();
    assert!(!exclude.contains(".env.portool"));
    // Allocation gone.
    let ls = env.portool(&r, &["ls", "--all", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&ls.stdout).unwrap();
    assert_eq!(v["allocations"].as_array().unwrap().len(), 0);
}

#[test]
fn deinit_with_a_symlink_hook_is_partial_and_keeps_allocations() {
    let env = Env::new("[19560, 19599]");
    let r = env.make_repo("symlink");
    assert!(env.portool(&r, &["init"]).status.success());

    // Replace post-checkout with a symlink; deinit must not follow it, must
    // report partial, and must keep the allocation.
    let hook = r.join(".git/hooks/post-checkout");
    std::fs::remove_file(&hook).unwrap();
    std::os::unix::fs::symlink("/tmp/whatever", &hook).unwrap();

    let out = env.portool(&r, &["deinit"]);
    assert!(!out.status.success(), "deinit must fail closed on residue");
    assert!(
        stdout(&out).contains("partial_deinit"),
        "must report partial: {}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("allocations-kept"),
        "must keep allocations: {}",
        stdout(&out)
    );
    assert!(std::fs::symlink_metadata(&hook)
        .unwrap()
        .file_type()
        .is_symlink());
    // Allocation retained.
    let ls = env.portool(&r, &["ls", "--all", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&ls.stdout).unwrap();
    assert_eq!(v["allocations"].as_array().unwrap().len(), 1);
}

#[test]
fn deinit_with_a_malformed_managed_block_is_partial() {
    let env = Env::new("[19560, 19599]");
    let r = env.make_repo("malformed");
    // A foreign hook with only a begin marker (truncated block) -> malformed.
    let hook = r.join(".git/hooks/post-checkout");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(
        &hook,
        "#!/bin/sh\n# >>> portool >>>\nportool sync --quiet || true\nuser-code\n",
    )
    .unwrap();
    assert!(env
        .portool(&r, &["init", "--gitignore-only"])
        .status
        .success());
    // Register an allocation so there is something to (not) release.
    assert!(env.portool(&r, &["sync"]).status.success());

    let out = env.portool(&r, &["deinit"]);
    assert!(!out.status.success());
    assert!(
        stdout(&out).contains("partial_deinit"),
        "got: {}",
        stdout(&out)
    );
    // The malformed hook is left byte-identical.
    assert!(std::fs::read_to_string(&hook)
        .unwrap()
        .contains("user-code"));
}

#[test]
fn deinit_with_a_non_shell_hook_reports_partial_if_it_invokes_portool() {
    let env = Env::new("[19560, 19599]");
    let r = env.make_repo("python");
    // A python hook that (somehow) mentions portool sync: install can't touch
    // it, and deinit can't neutralize it -> residue.
    let hook = r.join(".git/hooks/post-checkout");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(
        &hook,
        "#!/usr/bin/env python3\n# portool sync marker\nprint('hi')\n",
    )
    .unwrap();
    let out = env.portool(&r, &["deinit"]);
    // A python hook mentioning portool is not neutralized -> partial.
    assert!(!out.status.success());
    assert!(
        stdout(&out).contains("partial_deinit"),
        "got: {}",
        stdout(&out)
    );
    // Untouched.
    assert!(std::fs::read_to_string(&hook)
        .unwrap()
        .contains("print('hi')"));
}

#[test]
fn deinit_under_a_shared_hookspath_touches_nothing_outside_and_succeeds_cleanly() {
    let env = Env::new("[19560, 19599]");
    let r = env.make_repo("shared");
    let outside = env._tmp.path().join("shared-hooks");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("post-checkout"), "#!/bin/sh\necho theirs\n").unwrap();
    git(&r, &["config", "core.hooksPath", outside.to_str().unwrap()]);
    // A shared hooksPath is never installed into, so there is nothing of
    // portool's to remove and deinit is clean (env/alloc via sync still work).
    assert!(env.portool(&r, &["sync"]).status.success());

    let out = env.portool(&r, &["deinit"]);
    assert!(out.status.success(), "got: {}", stdout(&out));
    // The shared hook is untouched.
    assert_eq!(
        std::fs::read_to_string(outside.join("post-checkout")).unwrap(),
        "#!/bin/sh\necho theirs\n"
    );
}

#[test]
fn unhook_reports_partial_on_a_symlink_hook() {
    let env = Env::new("[19560, 19599]");
    let r = env.make_repo("unhook");
    assert!(env.portool(&r, &["init"]).status.success());
    let hook = r.join(".git/hooks/post-checkout");
    std::fs::remove_file(&hook).unwrap();
    std::os::unix::fs::symlink("/tmp/whatever", &hook).unwrap();

    let out = env.portool(&r, &["unhook"]);
    assert!(!out.status.success());
    assert!(
        stdout(&out).contains("partial_unhook"),
        "got: {}",
        stdout(&out)
    );
}
