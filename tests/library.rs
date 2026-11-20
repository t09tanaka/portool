//! Coexistence of the embedded library and the standalone CLI (WP25b).
//!
//! Both sides are pointed at one isolated state directory (the library
//! reads the process environment, the CLI child gets the same variables).
//! Every worktree is allocated at the same time -- half through the
//! library on threads, half by spawning `portool sync` -- and the resulting
//! ports must be pairwise distinct. Releases must be visible to the other
//! side.
//!
//! This file holds a single `#[test]` because it sets process-wide
//! environment variables.

use portool::{ExecEnv, Released, Worktree};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Barrier};
use tempfile::TempDir;

const WORKTREES: usize = 24;

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

fn cli(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_portool"));
    cmd.current_dir(dir).args(args);
    // Inherits the process environment set in `main_test`.
    cmd
}

fn registry_worktrees(state: &Path) -> serde_json::Map<String, serde_json::Value> {
    let text = fs::read_to_string(state.join("portool/registry.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut all = serde_json::Map::new();
    for project in json["projects"].as_object().unwrap().values() {
        for (k, v) in project["worktrees"].as_object().unwrap() {
            all.insert(k.clone(), v.clone());
        }
    }
    all
}

fn block_of(entry: &serde_json::Value) -> (u16, u16) {
    let b = entry["block"].as_array().unwrap();
    (b[0].as_u64().unwrap() as u16, b[1].as_u64().unwrap() as u16)
}

#[test]
fn library_and_cli_share_one_ledger_safely() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let state = tmp.path().join("state");
    let config = tmp.path().join("config");
    let root = tmp.path().join("root");
    for d in [&home, &state, &config, &root] {
        fs::create_dir_all(d).unwrap();
    }
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_STATE_HOME", &state);
    std::env::set_var("XDG_CONFIG_HOME", &config);
    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");

    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    assert!(git(&repo, &["init", "-q", "-b", "main"]).status.success());
    fs::write(repo.join("README.md"), "x\n").unwrap();
    fs::write(repo.join(".portool.toml"), "[ports]\nweb = 0\ndb = 1\n").unwrap();
    assert!(git(&repo, &["add", "."]).status.success());
    assert!(git(
        &repo,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init"
        ]
    )
    .status
    .success());
    let mut trees: Vec<PathBuf> = vec![repo.clone()];
    for i in 0..WORKTREES - 1 {
        let wt = root.join(format!("wt-{i}"));
        assert!(git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("f/{i}"),
                wt.to_str().unwrap()
            ]
        )
        .status
        .success());
        trees.push(wt);
    }

    // Even indices go through the library (threads), odd through the CLI
    // (processes); everything starts at once.
    let barrier = Arc::new(Barrier::new(WORKTREES / 2 + 1));
    let mut children = Vec::new();
    let mut threads = Vec::new();
    for (i, wt) in trees.iter().enumerate() {
        if i % 2 == 1 {
            children.push(
                cli(wt, &["sync", "--quiet"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        } else {
            let wt = wt.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                let handle = Worktree::open(&wt).unwrap();
                barrier.wait();
                handle.allocate().unwrap()
            }));
        }
    }
    barrier.wait();
    let lib_allocs: Vec<portool::Allocation> =
        threads.into_iter().map(|t| t.join().unwrap()).collect();
    for c in children {
        let out = c.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Every worktree is in the shared ledger, and no port is handed out
    // twice across both sides.
    let entries = registry_worktrees(&state);
    assert_eq!(entries.len(), WORKTREES);
    let mut seen = BTreeSet::new();
    for entry in entries.values() {
        let (start, end) = block_of(entry);
        for port in start..=end {
            assert!(seen.insert(port), "port {port} allocated twice");
        }
    }
    // What the library returned matches the ledger and `.env.portool`.
    for alloc in &lib_allocs {
        assert!(entries.values().any(|e| block_of(e) == alloc.block));
        let env = fs::read_to_string(&alloc.env_file).unwrap();
        assert!(env.contains(&format!("WEB_PORT={}\n", alloc.ports[0].1)));
        assert_eq!(alloc.ports[0].0, "web");
        assert_eq!(alloc.ports[0].1, alloc.block.0);
    }

    // exec_env produces the same ports as the allocation.
    let first = Worktree::open(&trees[0]).unwrap();
    let env = first.exec_env(&ExecEnv::default()).unwrap().vars;
    assert_eq!(env["WEB_PORT"], lib_allocs[0].block.0.to_string());
    assert_eq!(env["PORTOOL_WORKTREE_ID"], first.worktree_id());

    // Release through the library is visible to the CLI ...
    let key = |p: &Path| fs::canonicalize(p).unwrap().to_string_lossy().into_owned();
    assert_eq!(first.release().unwrap().released, Released::Freed);
    assert!(!registry_worktrees(&state).contains_key(&key(&trees[0])));
    let ls = cli(&trees[1], &["ls", "--json"]).output().unwrap();
    assert!(ls.status.success());
    // Exact (quoted) match: the project's common dir merely starts with
    // the main worktree's path.
    assert!(!String::from_utf8_lossy(&ls.stdout).contains(&format!("\"{}\"", key(&trees[0]))));
    assert_eq!(
        first.release().unwrap().released,
        Released::NothingToRelease
    );

    // ... and release through the CLI is visible to the library.
    assert!(cli(&trees[2], &["release"])
        .output()
        .unwrap()
        .status
        .success());
    let second = Worktree::open(&trees[2]).unwrap();
    assert_eq!(
        second.release().unwrap().released,
        Released::NothingToRelease
    );
    assert!(!registry_worktrees(&state).contains_key(&key(&trees[2])));

    // Allocation by the CLI is visible to a library release, and a worktree
    // removed from disk can still be released by path.
    assert!(cli(&trees[3], &["sync", "--quiet"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(registry_worktrees(&state).contains_key(&key(&trees[3])));
    let doomed = key(&trees[3]);
    assert!(git(
        &repo,
        &["worktree", "remove", "--force", trees[3].to_str().unwrap()]
    )
    .status
    .success());
    assert_eq!(
        portool::release_removed(&repo, &trees[3]).unwrap().released,
        Released::Freed
    );
    assert!(!registry_worktrees(&state).contains_key(&doomed));
    assert_eq!(
        portool::release_removed(&repo, &trees[3]).unwrap().released,
        Released::NothingToRelease
    );

    // A freed block can be allocated again (by either side).
    let again = first.allocate().unwrap();
    assert!(registry_worktrees(&state).contains_key(&key(&trees[0])));
    let _ = again;
}
