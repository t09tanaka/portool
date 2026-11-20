//! The library must never write to standard error: advisory warnings are
//! returned as data. fd 2 is redirected to a file around every library
//! call, and the file must stay empty while the warnings show up in the
//! results. Single `#[test]` because it changes process-wide state.

use portool::{ExecEnv, Worktree};
use std::fs;
use std::os::unix::io::AsRawFd;
use std::process::Command;
use tempfile::TempDir;

fn git(dir: &std::path::Path, args: &[&str]) {
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
    assert!(ok);
}

#[test]
fn library_never_writes_to_stderr() {
    let tmp = TempDir::new().unwrap();
    let (home, state, config) = (
        tmp.path().join("home"),
        tmp.path().join("state"),
        tmp.path().join("config"),
    );
    let repo = tmp.path().join("repo");
    for d in [&home, &state, &config.join("portool"), &repo] {
        fs::create_dir_all(d).unwrap();
    }
    // A deprecated key makes loading the config warn.
    fs::write(config.join("portool/config.toml"), "gc_days = 3\n").unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_STATE_HOME", &state);
    std::env::set_var("XDG_CONFIG_HOME", &config);
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a"), "a\n").unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "i",
        ],
    );

    let log = fs::File::create(tmp.path().join("stderr.log")).unwrap();
    // SAFETY: plain dup/dup2 on this process's own fds.
    let saved = unsafe { libc::dup(2) };
    assert!(saved >= 0);
    assert!(unsafe { libc::dup2(log.as_raw_fd(), 2) } >= 0);

    let wt = Worktree::open(&repo).unwrap();
    let cfg = portool::load_config().unwrap();
    let alloc = wt.allocate().unwrap();
    let env = wt.exec_env(&ExecEnv::default()).unwrap();
    let rel = wt.release().unwrap();

    unsafe {
        libc::dup2(saved, 2);
        libc::close(saved);
    }

    assert_eq!(
        fs::read_to_string(tmp.path().join("stderr.log")).unwrap(),
        ""
    );
    assert!(cfg
        .warnings
        .iter()
        .any(|w| w.message.contains("gc_days") && w.message.starts_with("portool: warning:")));
    // No post-checkout hook is installed, so the CLI's hint comes back as data.
    let hint = "hint: run 'portool init' to install the post-checkout hook";
    assert!(alloc.warnings.iter().any(|w| w.message == hint));
    assert!(env.warnings.iter().any(|w| w.message == hint));
    assert!(rel.warnings.is_empty());
}
