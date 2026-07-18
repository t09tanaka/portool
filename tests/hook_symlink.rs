//! Adversarial E2E: portool must never write a hook outside the repository,
//! however a symlink is planted along the hook path (external review v0.10
//! P0-1). Covers a symlinked `.husky` dir, a symlinked `core.hooksPath` dir,
//! a symlinked hook file, and a check-then-swap TOCTOU race.

use std::path::Path;
use std::process::Command;

fn portool() -> Command {
    Command::new(env!("CARGO_BIN_EXE_portool"))
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed");
}

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "a@b.c"]);
    git(dir, &["config", "user.name", "a"]);
}

fn is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).unwrap().next().is_none()
}

#[test]
fn husky_dir_symlink_escaping_the_repo_writes_nothing_outside() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let outside = tmp.path().join("outside");
    init_repo(&repo);
    std::fs::create_dir_all(&outside).unwrap();
    // .husky -> ../outside (escapes the repo)
    std::os::unix::fs::symlink("../outside", repo.join(".husky")).unwrap();
    git(&repo, &["config", "core.hooksPath", ".husky/_"]);

    let out = portool()
        .current_dir(&repo)
        .args(["init", "--hook-only"])
        .output()
        .unwrap();

    assert!(
        is_empty(&outside),
        "portool wrote outside the repo (.husky symlink)"
    );
    assert!(!out.status.success(), "init must fail closed, not exit 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("outside this repository") || stderr.contains("resolves outside"),
        "must explain the refusal, got: {stderr}"
    );
}

#[test]
fn hooks_dir_symlink_escaping_the_repo_writes_nothing_outside() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let outside = tmp.path().join("outside");
    init_repo(&repo);
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("ci-hooks")).unwrap();
    git(&repo, &["config", "core.hooksPath", "ci-hooks"]);

    let out = portool()
        .current_dir(&repo)
        .args(["init", "--hook-only"])
        .output()
        .unwrap();

    assert!(is_empty(&outside), "wrote through a symlinked hooks dir");
    assert!(!out.status.success());
}

#[test]
fn hook_file_itself_a_symlink_is_left_untouched() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let victim = tmp.path().join("victim");
    init_repo(&repo);
    std::fs::write(&victim, "original\n").unwrap();
    let hooks = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::os::unix::fs::symlink(&victim, hooks.join("post-checkout")).unwrap();

    portool()
        .current_dir(&repo)
        .args(["init", "--hook-only"])
        .output()
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "original\n",
        "a symlinked hook file must never be followed/overwritten"
    );
    assert!(
        std::fs::symlink_metadata(hooks.join("post-checkout"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the symlink itself must be left in place"
    );
}

#[test]
fn check_then_swap_race_never_writes_outside() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let outside = tmp.path().join("outside");
    init_repo(&repo);
    std::fs::create_dir_all(&outside).unwrap();
    git(&repo, &["config", "core.hooksPath", "ci-hooks"]);

    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let dir = repo.join("ci-hooks");
    let outside2 = outside.clone();
    let swapper = std::thread::spawn(move || {
        while !s2.load(Ordering::Relaxed) {
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_file(&dir);
            let _ = std::fs::create_dir(&dir); // a real directory
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::os::unix::fs::symlink(&outside2, &dir); // an escape
        }
    });

    for _ in 0..150 {
        let _ = portool()
            .current_dir(&repo)
            .args(["init", "--hook-only"])
            .output();
        assert!(is_empty(&outside), "race wrote outside the repo");
    }

    stop.store(true, Ordering::Relaxed);
    swapper.join().unwrap();
    assert!(is_empty(&outside));
}
