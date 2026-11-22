//! Library calls given an explicit `Context` use the ledger and config the
//! context names, not the ones of the process environment.
//!
//! The process environment points at a decoy directory that must stay
//! empty; the context points at another. A CLI child run with the
//! context's variables must see the same ledger. A ledger written by a
//! newer portool must come back as `Error::UnsupportedRegistryVersion`.
//!
//! Single `#[test]` because it sets process-wide environment variables.

use portool::{Context, Error, Released, Worktree};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

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

fn commit_repo(repo: &Path) {
    fs::create_dir_all(repo).unwrap();
    assert!(git(repo, &["init", "-q", "-b", "main"]).status.success());
    fs::write(repo.join("README.md"), "x\n").unwrap();
    assert!(git(repo, &["add", "."]).status.success());
    let commit = [
        "-c",
        "user.email=t@example.com",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "-m",
        "init",
    ];
    assert!(git(repo, &commit).status.success());
}

/// Every file under `dir`, recursively.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(files(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn explicit_context_selects_the_ledger_and_config() {
    let tmp = TempDir::new().unwrap();
    let decoy = tmp.path().join("decoy");
    let home = tmp.path().join("home");
    let state = tmp.path().join("state");
    let config = tmp.path().join("config");
    for d in [&decoy, &home, &state, &config.join("portool")] {
        fs::create_dir_all(d).unwrap();
    }
    // The process environment: the decoy, which nothing may touch.
    std::env::set_var("HOME", &decoy);
    std::env::set_var("XDG_STATE_HOME", decoy.join("state"));
    std::env::set_var("XDG_CONFIG_HOME", decoy.join("config"));
    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");

    // The context's config narrows the pool, so allocating proves it was
    // read too.
    fs::write(
        config.join("portool/config.toml"),
        "range = [4400, 4409]\nblock_align = 5\n",
    )
    .unwrap();
    let env = vec![
        ("HOME".to_string(), home.display().to_string()),
        ("XDG_STATE_HOME".to_string(), state.display().to_string()),
        ("XDG_CONFIG_HOME".to_string(), config.display().to_string()),
        ("PATH".to_string(), std::env::var("PATH").unwrap()),
    ];
    let ctx = Context::from_env(env.clone());

    let loaded = portool::load_config_in(&ctx).unwrap();
    assert_eq!(loaded.config.range, (4400, 4409));

    let repo = tmp.path().join("repo");
    commit_repo(&repo);
    let wt_path = tmp.path().join("wt");
    assert!(git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "f",
            wt_path.to_str().unwrap()
        ]
    )
    .status
    .success());

    // Allocate through the library with the context.
    let main = Worktree::open_in(&ctx, &repo).unwrap();
    let alloc = main.allocate().unwrap();
    assert!((4400..=4409).contains(&alloc.block.0), "{:?}", alloc.block);
    let registry = state.join("portool/registry.json");
    assert!(registry.exists());

    // The CLI run with the same variables sees that ledger and releases
    // the allocation; the library observes it gone.
    let out = Command::new(env!("CARGO_BIN_EXE_portool"))
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(&repo)
        .arg("release")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(main.release().unwrap().released, Released::NothingToRelease);

    // release_removed_in frees a deleted worktree's entry in the context's
    // ledger.
    Worktree::open_in(&ctx, &wt_path)
        .unwrap()
        .allocate()
        .unwrap();
    fs::remove_dir_all(&wt_path).unwrap();
    let unrelated = repo.parent().unwrap().join("unrelated");
    fs::create_dir(&unrelated).unwrap();
    fs::write(unrelated.join(".env.portool"), "keep").unwrap();
    std::os::unix::fs::symlink(&unrelated, &wt_path).unwrap();
    assert!(portool::release_removed_in(&ctx, &repo, &wt_path).is_err());
    assert_eq!(
        fs::read_to_string(unrelated.join(".env.portool")).unwrap(),
        "keep"
    );
    fs::remove_file(&wt_path).unwrap();
    let outcome = portool::release_removed_in(&ctx, &repo, &wt_path).unwrap();
    assert_eq!(outcome.released, Released::Freed);

    assert!(
        files(&decoy).is_empty(),
        "the process environment's ledger must not be touched: {:?}",
        files(&decoy)
    );

    // A ledger from a newer portool: typed error, file left as it was.
    let future = br#"{"version":999,"range":[3000,9999],"projects":{},"reservations":[]}"#;
    fs::write(&registry, future).unwrap();
    assert!(matches!(
        portool::check_registry_in(&ctx),
        Err(Error::UnsupportedRegistryVersion { found: 999, .. })
    ));
    match main.allocate() {
        Err(Error::UnsupportedRegistryVersion {
            found: 999, path, ..
        }) => assert_eq!(path.as_deref(), Some(registry.as_path())),
        other => panic!("expected UnsupportedRegistryVersion, got {other:?}"),
    }
    match portool::release_removed_in(&ctx, &repo, &wt_path) {
        Err(Error::UnsupportedRegistryVersion { found: 999, .. }) => {}
        other => panic!("expected UnsupportedRegistryVersion, got {other:?}"),
    }
    assert_eq!(fs::read(&registry).unwrap(), future);
    assert!(files(&decoy).is_empty());
}
