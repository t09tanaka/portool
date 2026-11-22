//! Automatic retention: stale allocations, corrupt copies and crash temp files
//! are removed by the commands that already write the same place.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct TestEnv {
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    config: PathBuf,
    _tmp: TempDir,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let config = tmp.path().join("config");
        let root = tmp.path().join("root");
        for d in [&home, &state, &config, &root] {
            std::fs::create_dir_all(d).unwrap();
        }
        TestEnv {
            root,
            home,
            state,
            config,
            _tmp: tmp,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_portool"));
        cmd.env_clear();
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        cmd.env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        cmd
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.command().current_dir(dir).args(args).output().unwrap()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn registry_path(&self) -> PathBuf {
        self.state.join("portool").join("registry.json")
    }

    fn write_config(&self, contents: &str) {
        let dir = self.config.join("portool");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), contents).unwrap();
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

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "a@b.c"]);
    git(dir, &["config", "user.name", "a"]);
    std::fs::write(dir.join("README.md"), "x\n").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn age(path: &Path, secs: u64) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(secs))
        .unwrap();
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn ls_all(env: &TestEnv, dir: &Path) -> serde_json::Value {
    let out = env.run(dir, &["ls", "--all", "--json"]);
    serde_json::from_slice(&out.stdout).expect("json")
}

fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn another_projects_vanished_worktree_is_released_by_any_locking_command() {
    let env = TestEnv::new();
    env.write_config("range = [19700, 19799]\n");
    let a = env.path("a");
    let b = env.path("b");
    init_repo(&a);
    init_repo(&b);
    let a_wt = env.path("a-wt");
    git(
        &a,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            a_wt.to_str().unwrap(),
        ],
    );
    assert!(env.run(&a_wt, &["sync"]).status.success());
    assert!(env.run(&b, &["sync"]).status.success());
    assert_eq!(ls_all(&env, &b)["allocations"].as_array().unwrap().len(), 2);

    // The worktree directory vanishes and git forgets it.
    std::fs::remove_dir_all(&a_wt).unwrap();
    git(&a, &["worktree", "prune"]);

    // A command in an unrelated project releases it (here: pin takes the lock).
    assert!(env.run(&b, &["pin"]).status.success());
    assert_eq!(ls_all(&env, &b)["allocations"].as_array().unwrap().len(), 1);
}

#[test]
fn a_deleted_repository_loses_its_project_entry_automatically() {
    let env = TestEnv::new();
    env.write_config("range = [19800, 19899]\n");
    let a = env.path("a");
    let b = env.path("b");
    init_repo(&a);
    init_repo(&b);
    assert!(env.run(&a, &["sync"]).status.success());
    assert!(env.run(&b, &["sync"]).status.success());
    std::fs::remove_dir_all(&a).unwrap();

    assert!(env.run(&b, &["pin"]).status.success());
    let v = ls_all(&env, &b);
    assert_eq!(v["allocations"].as_array().unwrap().len(), 1, "{v}");
}

#[test]
fn only_the_newest_corrupt_copy_is_kept() {
    let env = TestEnv::new();
    env.write_config("range = [19900, 19949]\n");
    let a = env.path("a");
    init_repo(&a);
    assert!(env.run(&a, &["sync"]).status.success());
    for _ in 0..3 {
        std::fs::write(env.registry_path(), b"{ corrupt").unwrap();
        let out = env.run(&a, &["doctor", "--repair"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let corrupt: Vec<String> = names(&env.state.join("portool"))
        .into_iter()
        .filter(|n| n.contains(".corrupt-"))
        .collect();
    assert_eq!(corrupt.len(), 1, "{corrupt:?}");
}

#[test]
fn a_week_old_corrupt_copy_is_removed_by_the_next_save() {
    let env = TestEnv::new();
    env.write_config("range = [19950, 19999]\n");
    let a = env.path("a");
    init_repo(&a);
    assert!(env.run(&a, &["sync"]).status.success());
    let dir = env.state.join("portool");
    let old = dir.join("registry.json.corrupt-1000000000-1");
    std::fs::write(&old, "x").unwrap();
    assert!(env.run(&a, &["pin"]).status.success());
    assert!(!old.exists());
}

#[test]
fn crash_temp_files_are_swept_but_foreign_and_live_ones_are_kept() {
    let env = TestEnv::new();
    env.write_config("range = [19600, 19649]\n");
    let a = env.path("a");
    init_repo(&a);
    let dead = dead_pid();
    let live = std::process::id();
    let stale = a.join(format!(".portool-tmp-{dead}-AbCdEf"));
    let live_tmp = a.join(format!(".portool-tmp-{live}-AbCdEf"));
    let young = a.join(format!(".portool-tmp-{dead}-young00"));
    let foreign = a.join(".tmpAbCdEf");
    let unknown = a.join(".portool-tmp-notapid-x");
    for p in [&stale, &live_tmp, &young, &foreign, &unknown] {
        std::fs::write(p, "x").unwrap();
    }
    for p in [&stale, &live_tmp, &foreign, &unknown] {
        age(p, 3 * 3600);
    }
    // The state directory holds crash leftovers of any portool version.
    let sdir = env.state.join("portool");
    std::fs::create_dir_all(&sdir).unwrap();
    let legacy = sdir.join(".tmpQwErTy");
    std::fs::write(&legacy, "x").unwrap();
    age(&legacy, 3 * 3600);

    assert!(env.run(&a, &["sync"]).status.success());

    assert!(!stale.exists(), "stale temp of a dead owner must go");
    assert!(!legacy.exists(), "legacy temp in the state dir must go");
    assert!(live_tmp.exists(), "a live owner's temp must stay");
    assert!(young.exists(), "a young temp must stay");
    assert!(foreign.exists(), "foreign names must stay");
    assert!(unknown.exists(), "unparseable names must stay");
}
