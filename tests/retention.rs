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
fn corrupt_copies_are_kept_per_repair_not_only_the_newest() {
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
    assert_eq!(corrupt.len(), 3, "{corrupt:?}");
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
    let young = dir.join("registry.json.corrupt-1000000000-2");
    std::fs::write(&young, "x").unwrap();
    age(&old, 7 * 24 * 3600 + 60);
    age(&young, 7 * 24 * 3600 - 60);
    assert!(env.run(&a, &["pin"]).status.success());
    assert!(!old.exists(), "older than a week by modification time");
    assert!(young.exists(), "the time in the name does not count");
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

// ---------------------------------------------------------------------------
// Evidence rules, bounded checks, save points and the port probe.
// ---------------------------------------------------------------------------

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn ok(out: Output) -> Output {
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    out
}

/// Every allocation in the ledger: (worktree path, block).
fn allocations(env: &TestEnv) -> Vec<(String, (u16, u16))> {
    let text = std::fs::read_to_string(env.registry_path()).unwrap();
    let reg: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut out = Vec::new();
    for project in reg["projects"].as_object().unwrap().values() {
        for (path, entry) in project["worktrees"].as_object().unwrap() {
            let b = &entry["block"];
            out.push((
                path.clone(),
                (b[0].as_u64().unwrap() as u16, b[1].as_u64().unwrap() as u16),
            ));
        }
    }
    out
}

fn block_of(env: &TestEnv, worktree: &Path) -> Option<(u16, u16)> {
    let name = worktree.file_name().unwrap().to_string_lossy().into_owned();
    allocations(env)
        .into_iter()
        .find(|(path, _)| Path::new(path).file_name().unwrap().to_string_lossy() == name)
        .map(|(_, block)| block)
}

/// The ports inside `range` that no allocation covers, highest first.
fn unallocated_ports(env: &TestEnv, range: (u16, u16)) -> Vec<String> {
    let taken = allocations(env);
    (range.0..=range.1)
        .rev()
        .filter(|p| taken.iter().all(|(_, b)| p < &b.0 || p > &b.1))
        .map(|p| p.to_string())
        .collect()
}

/// Rewrites the worktree's ledger block (the allocator places by hash, so
/// tests that need room next to a block put it at a known place).
fn set_block(env: &TestEnv, worktree: &Path, block: (u16, u16)) {
    let name = worktree.file_name().unwrap().to_string_lossy().into_owned();
    let text = std::fs::read_to_string(env.registry_path()).unwrap();
    let mut reg: serde_json::Value = serde_json::from_str(&text).unwrap();
    for project in reg["projects"].as_object_mut().unwrap().values_mut() {
        for (path, entry) in project["worktrees"].as_object_mut().unwrap() {
            if Path::new(path).file_name().unwrap().to_string_lossy() == name {
                entry["block"] = serde_json::json!([block.0, block.1]);
            }
        }
    }
    std::fs::write(env.registry_path(), serde_json::to_string(&reg).unwrap()).unwrap();
}

fn add_worktree(repo: &Path, worktree: &Path, branch: &str) {
    git(
        repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            worktree.to_str().unwrap(),
        ],
    );
}

/// Repositories `a` (with a synced linked worktree `a-wt`) and `b` (synced).
fn two_projects(env: &TestEnv) -> (PathBuf, PathBuf, PathBuf) {
    let (a, b, a_wt) = (env.path("a"), env.path("b"), env.path("a-wt"));
    init_repo(&a);
    init_repo(&b);
    add_worktree(&a, &a_wt, "feat");
    ok(env.run(&a_wt, &["sync"]));
    ok(env.run(&b, &["sync"]));
    assert_eq!(allocations(env).len(), 2);
    (a, b, a_wt)
}

#[test]
fn removed_with_rm_and_never_pruned_in_git_is_released() {
    let env = TestEnv::new();
    env.write_config("range = [19000, 19049]\n");
    let (_a, b, a_wt) = two_projects(&env);
    // The most common deletion: no `git worktree remove`, no `prune`. Git
    // keeps listing the worktree, as `prunable`.
    std::fs::remove_dir_all(&a_wt).unwrap();

    ok(env.run(&b, &["pin"]));
    assert!(block_of(&env, &a_wt).is_none(), "{:?}", allocations(&env));
    assert!(block_of(&env, &b).is_some());
}

#[test]
fn removed_with_rm_is_released_by_a_sync_in_its_own_project() {
    let env = TestEnv::new();
    env.write_config("range = [19050, 19099]\n");
    let (a, _b, a_wt) = two_projects(&env);
    std::fs::remove_dir_all(&a_wt).unwrap();

    ok(env.run(&a, &["sync"])); // first sync of the main checkout: slow path
    assert!(block_of(&env, &a_wt).is_none(), "{:?}", allocations(&env));
    assert!(block_of(&env, &a).is_some());
}

#[test]
fn a_locked_worktree_is_kept_even_when_its_directory_is_missing() {
    let env = TestEnv::new();
    env.write_config("range = [19100, 19149]\n");
    let (a, b, a_wt) = two_projects(&env);
    git(&a, &["worktree", "lock", a_wt.to_str().unwrap()]);
    std::fs::remove_dir_all(&a_wt).unwrap();

    ok(env.run(&b, &["pin"]));
    assert!(block_of(&env, &a_wt).is_some());
}

#[test]
fn repository_gone_but_worktree_directory_present_releases_nothing() {
    let env = TestEnv::new();
    env.write_config("range = [19150, 19199]\n");
    let (a, b, a_wt) = two_projects(&env);
    // The main clone is renamed: the common dir the ledger knows is gone,
    // the linked worktree and its .env.portool are still there.
    std::fs::rename(&a, env.path("a-renamed")).unwrap();
    assert!(a_wt.join(".env.portool").exists());

    ok(env.run(&b, &["pin"]));
    ok(env.run(&b, &["prune", "--all"]));
    assert!(block_of(&env, &a_wt).is_some(), "{:?}", allocations(&env));

    // Once the worktree directory itself goes, so do the entry and the project.
    std::fs::remove_dir_all(&a_wt).unwrap();
    ok(env.run(&b, &["unpin"]));
    assert_eq!(allocations(&env).len(), 1);
    assert_eq!(ls_all(&env, &b)["allocations"].as_array().unwrap().len(), 1);
}

#[test]
fn a_gone_worktree_whose_ports_are_busy_on_any_address_is_kept() {
    let env = TestEnv::new();
    env.write_config("range = [19200, 19249]\n");
    let (a, b, a_wt) = two_projects(&env);
    let block = block_of(&env, &a_wt).unwrap();
    std::fs::remove_dir_all(&a_wt).unwrap();
    git(&a, &["worktree", "prune"]);

    // A published container port: 0.0.0.0, not 127.0.0.1.
    let published = std::net::TcpListener::bind(("0.0.0.0", block.0)).unwrap();
    ok(env.run(&b, &["pin"]));
    ok(env.run(&b, &["prune", "--all"]));
    assert_eq!(block_of(&env, &a_wt), Some(block), "0.0.0.0 listener");
    drop(published);

    // A dev server on `localhost` that resolved to ::1 (skipped without IPv6).
    if let Ok(v6) = std::net::TcpListener::bind(("::1", block.0 + 1)) {
        ok(env.run(&b, &["unpin"]));
        assert_eq!(block_of(&env, &a_wt), Some(block), "[::1] listener");
        drop(v6);
    }

    ok(env.run(&b, &["pin"]));
    assert!(
        block_of(&env, &a_wt).is_none(),
        "released once the ports are free"
    );
}

/// A `git` that never answers for one repository and is the real git for
/// everything else. Returns the directory to put first on PATH and the log
/// the stand-in appends a line to each time it is asked about `stuck`.
fn hanging_git(env: &TestEnv, stuck: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let real = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let dir = env.path("fake-bin");
    let log = env.path("fake-git.log");
    std::fs::create_dir_all(&dir).unwrap();
    let stuck = std::fs::canonicalize(stuck).unwrap();
    let script = format!(
        "#!/bin/sh\ncase \"$*\" in\n  *\"{}/.git worktree list\"*) echo asked >> \"{}\"; exec sleep 30 ;;\nesac\nexec \"{}\" \"$@\"\n",
        stuck.display(),
        log.display(),
        real.trim()
    );
    let path = dir.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, log)
}

fn run_with_path(env: &TestEnv, first: &Path, dir: &Path, args: &[&str]) -> Output {
    let path = format!(
        "{}:{}",
        first.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    env.command()
        .env("PATH", path)
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn a_git_that_never_answers_for_another_project_cannot_stall_a_command() {
    let env = TestEnv::new();
    env.write_config("range = [19250, 19299]\n");
    let (a, b, a_wt) = two_projects(&env);
    std::fs::remove_dir_all(&a_wt).unwrap(); // a candidate, so git is asked about `a`
    let (fake, log) = hanging_git(&env, &a);
    let asked = || {
        std::fs::read_to_string(&log)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };

    let started = std::time::Instant::now();
    let out = ok(run_with_path(&env, &fake, &b, &["pin"]));
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_secs(12),
        "the command waited {took:?} for a git that sleeps 30s"
    );
    let warnings = stderr(&out);
    assert_eq!(
        warnings.matches("could not check 1 project(s)").count(),
        1,
        "exactly one warning line: {warnings}"
    );
    assert_eq!(warnings.lines().count(), 1, "{warnings}");
    assert!(block_of(&env, &a_wt).is_some(), "the entry must be kept");
    assert_eq!(asked(), 1);

    // The failure is remembered: the next command neither asks git about
    // that project again nor repeats the warning.
    let started = std::time::Instant::now();
    let out = ok(run_with_path(&env, &fake, &b, &["unpin"]));
    assert!(started.elapsed() < std::time::Duration::from_secs(12));
    assert_eq!(stderr(&out), "");
    assert_eq!(asked(), 1, "git must not be spawned again within the hour");
    assert!(block_of(&env, &a_wt).is_some());

    // The memory is a file of its own; the next save removes it once expired.
    let memory = env.state.join("portool").join("stale-check-failed.json");
    assert!(memory.exists());
    age(&memory, 3601);
    ok(env.run(&b, &["pin"]));
    assert!(!memory.exists(), "{:?}", names(&env.state.join("portool")));
    ok(env.run(&b, &["unpin"])); // forgotten: checked again (real git), released
    assert!(block_of(&env, &a_wt).is_none());
}

#[test]
fn a_project_git_cannot_read_is_reported_once_kept_and_droppable_on_request() {
    let env = TestEnv::new();
    env.write_config("range = [19300, 19349]\n");
    let (a, b, a_wt) = two_projects(&env);
    std::fs::remove_dir_all(&a_wt).unwrap();
    std::fs::write(a.join(".git").join("HEAD"), "garbage\n").unwrap();

    let out = ok(env.run(&b, &["pin"]));
    assert_eq!(
        stderr(&out).matches("could not check 1 project(s)").count(),
        1,
        "{}",
        stderr(&out)
    );
    assert!(block_of(&env, &a_wt).is_some());
    assert_eq!(
        stderr(&ok(env.run(&b, &["unpin"]))),
        "",
        "remembered: silent"
    );

    // The manual command checks now, says what it kept and why ...
    let out = ok(env.run(&b, &["prune", "--all"]));
    let text = stderr(&out);
    assert!(text.contains("skipping project"), "{text}");
    assert!(text.contains("git could not list"), "{text}");
    assert!(text.contains("--forget-unverifiable"), "{text}");
    assert!(block_of(&env, &a_wt).is_some());

    // ... and the named override lists what it would drop, then drops it.
    let out = ok(env.run(
        &b,
        &["prune", "--all", "--forget-unverifiable", "--dry-run"],
    ));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("would prune"), "{text}");
    assert!(text.contains("forgotten on request"), "{text}");
    assert!(block_of(&env, &a_wt).is_some(), "dry run changes nothing");
    ok(env.run(&b, &["prune", "--all", "--forget-unverifiable"]));
    assert!(block_of(&env, &a_wt).is_none());
    assert!(
        block_of(&env, &b).is_some(),
        "an existing worktree is never forgotten"
    );
}

#[test]
fn commands_that_did_not_write_before_do_not_write_or_fail_now() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    env.write_config("range = [19350, 19399]\n");
    let (a, b, a_wt) = two_projects(&env);
    let c = env.path("c");
    init_repo(&c);
    let free = unallocated_ports(&env, (19350, 19399));
    let (reserved, other_port) = (free[0].clone(), free[1].clone());
    ok(env.run(&b, &["reserve", &reserved, "--label", "db"]));
    // Something for the sweep to release, if it were allowed to write.
    std::fs::remove_dir_all(&a_wt).unwrap();
    git(&a, &["worktree", "prune"]);

    let state = env.state.join("portool");
    let before = std::fs::read(env.registry_path()).unwrap();
    let listing = names(&state);
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o555)).unwrap();

    let release = env.run(&c, &["release"]);
    let pin = env.run(&c, &["pin"]);
    let unreserve = env.run(&b, &["unreserve", &other_port]);
    let reserve = env.run(&b, &["reserve", &reserved, "--label", "db"]);

    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(release.status.success(), "{}", stderr(&release));
    assert!(
        String::from_utf8_lossy(&release.stdout).contains("had no allocation"),
        "{release:?}"
    );
    assert!(
        stderr(&pin).contains("no allocated block yet"),
        "{}",
        stderr(&pin)
    );
    assert!(
        stderr(&unreserve).contains("no reservation matches"),
        "{}",
        stderr(&unreserve)
    );
    assert!(reserve.status.success(), "{}", stderr(&reserve));
    assert_eq!(std::fs::read(env.registry_path()).unwrap(), before);
    assert_eq!(names(&state), listing);
    assert!(
        block_of(&env, &a_wt).is_some(),
        "nothing was swept without a save"
    );
}

#[test]
fn the_sweep_rides_on_release_reserve_unreserve_and_reallocate() {
    for (i, name) in ["release", "reserve", "unreserve", "reallocate"]
        .into_iter()
        .enumerate()
    {
        let env = TestEnv::new();
        env.write_config("range = [19400, 19499]\n");
        let (a, b, a_wt) = two_projects(&env);
        let free = unallocated_ports(&env, (19400, 19499));
        let (held, fresh) = (free[0].clone(), free[1].clone());
        ok(env.run(&b, &["reserve", &held]));
        let command = match name {
            "reserve" => vec![name, fresh.as_str()],
            "unreserve" => vec![name, held.as_str()],
            _ => vec![name],
        };
        std::fs::remove_dir_all(&a_wt).unwrap();
        if i % 2 == 0 {
            git(&a, &["worktree", "prune"]);
        }
        ok(env.run(&b, &command));
        assert!(
            block_of(&env, &a_wt).is_none(),
            "{command:?}: {:?}",
            allocations(&env)
        );
    }
}

#[test]
fn crash_temp_files_in_the_hook_directory_are_swept() {
    let env = TestEnv::new();
    env.write_config("range = [19500, 19549]\n");
    let a = env.path("a");
    init_repo(&a);
    let hooks = a.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let stale = hooks.join(format!(".portool-tmp-{}-post-checkout", dead_pid()));
    let live = hooks.join(format!(".portool-tmp-{}-post-checkout", std::process::id()));
    let young = hooks.join(format!(".portool-tmp-{}-post-merge", dead_pid()));
    let foreign = hooks.join(".tmpAbCdEf");
    for p in [&stale, &live, &young, &foreign] {
        std::fs::write(p, "x").unwrap();
    }
    for p in [&stale, &live, &foreign] {
        age(p, 3 * 3600);
    }

    ok(env.run(&a, &["init", "--hook-only"]));

    assert!(!stale.exists(), "a dead owner's old temp must go");
    assert!(live.exists(), "a live owner's temp must stay");
    assert!(young.exists(), "a young temp must stay");
    assert!(
        foreign.exists(),
        "legacy names are not ours outside the state directory"
    );
}

fn corrupt_copies(env: &TestEnv) -> Vec<String> {
    names(&env.state.join("portool"))
        .into_iter()
        .filter(|n| n.contains(".corrupt-"))
        .collect()
}

/// The path `doctor --repair` printed for the copy it set aside.
fn printed_copy(out: &Output) -> PathBuf {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let at = text.find("aside to ").expect(&text) + "aside to ".len();
    let rest = &text[at..];
    let end = rest.find(" and ").expect(&text);
    PathBuf::from(&rest[..end])
}

#[test]
fn the_copy_doctor_prints_still_exists_even_after_the_clock_stepped_back() {
    let env = TestEnv::new();
    env.write_config("range = [19550, 19599]\n");
    let a = env.path("a");
    init_repo(&a);
    ok(env.run(&a, &["sync"]));
    // A copy from "before the clock stepped back": the time in its name is
    // far in the future, so by name it is newer than anything made now.
    let dir = env.state.join("portool");
    let earlier = dir.join("registry.json.corrupt-99999999999999999999999-1");
    std::fs::write(&earlier, "x").unwrap();
    // The ledger itself was last written long ago; the copy made from it
    // must not inherit that age.
    std::fs::write(env.registry_path(), b"{ corrupt").unwrap();
    age(&env.registry_path(), 8 * 24 * 3600);

    let out = ok(env.run(&a, &["doctor", "--repair"]));
    let copy = printed_copy(&out);
    assert!(
        copy.exists(),
        "{} was deleted by the command that printed it",
        copy.display()
    );
    assert!(earlier.exists());

    // Nor does the next command take it for a week-old file.
    ok(env.run(&a, &["pin"]));
    assert!(copy.exists(), "{:?}", corrupt_copies(&env));
}

#[test]
fn two_abandon_repairs_keep_both_set_aside_ledgers() {
    let env = TestEnv::new();
    env.write_config("range = [19650, 19699]\n");
    let a = env.path("a");
    init_repo(&a);
    ok(env.run(&a, &["sync"]));
    let mut printed = Vec::new();
    for _ in 0..2 {
        std::fs::write(env.registry_path(), b"{ corrupt").unwrap();
        // An 8-day-old ledger: renaming it aside must not make it expire.
        age(&env.registry_path(), 8 * 24 * 3600);
        let out = ok(env.run(&a, &["doctor", "--repair", "--abandon-other-projects"]));
        printed.push(printed_copy(&out));
        ok(env.run(&a, &["sync"]));
    }
    for copy in &printed {
        assert!(
            copy.exists(),
            "{} is the only copy of the other projects",
            copy.display()
        );
    }
    assert_eq!(corrupt_copies(&env).len(), 2);
}

// ---------------------------------------------------------------------------
// A worktree's own running stack on its own ports is never a conflict.
// ---------------------------------------------------------------------------

/// A synced repository with a real listener on the first port of its own
/// block, bound the way a published container port is (`0.0.0.0`).
fn repo_with_its_stack_up(
    env: &TestEnv,
    name: &str,
) -> (PathBuf, (u16, u16), std::net::TcpListener) {
    let repo = env.path(name);
    init_repo(&repo);
    ok(env.run(&repo, &["sync"]));
    let block = block_of(env, &repo).unwrap();
    let listener = std::net::TcpListener::bind(("0.0.0.0", block.0)).unwrap();
    (repo, block, listener)
}

/// Nothing about ports in use, and nothing being moved.
fn assert_quiet(out: &Output, what: &str) {
    let text = stderr(out);
    assert!(
        !text.contains("in use") && !text.contains("moving") && !text.contains("listening"),
        "{what}: {text}"
    );
}

#[test]
fn own_stack_sync_fast_and_slow_path_change_nothing() {
    let env = TestEnv::new();
    env.write_config("range = [18000, 18049]\n");
    let (repo, block, _stack) = repo_with_its_stack_up(&env, "own-sync");
    let env_file = std::fs::read_to_string(repo.join(".env.portool")).unwrap();

    let out = ok(env.run(&repo, &["sync"])); // fast path
    assert_quiet(&out, "fast path");
    assert_eq!(block_of(&env, &repo), Some(block));

    std::fs::remove_file(repo.join(".env.portool")).unwrap();
    let out = ok(env.run(&repo, &["sync"])); // slow path: env file rewritten
    assert_quiet(&out, "slow path");
    assert_eq!(block_of(&env, &repo), Some(block), "no port may move");
    let ports = |s: &str| -> Vec<String> {
        s.lines()
            .filter(|l| !l.starts_with('#') && l.contains("PORT"))
            .map(str::to_string)
            .collect()
    };
    assert_eq!(
        ports(&std::fs::read_to_string(repo.join(".env.portool")).unwrap()),
        ports(&env_file)
    );
}

#[test]
fn own_stack_manifest_growth_extends_in_place() {
    let env = TestEnv::new();
    env.write_config("range = [18050, 18099]\n");
    let repo = env.path("own-grow");
    init_repo(&repo);
    ok(env.run(&repo, &["sync"]));
    let first = block_of(&env, &repo).unwrap();
    let width = first.1 - first.0 + 1;
    let block = (18050, 18050 + width - 1);
    set_block(&env, &repo, block);
    let _stack = std::net::TcpListener::bind(("0.0.0.0", block.0)).unwrap();
    std::fs::write(
        repo.join(".portool.toml"),
        format!("[ports]\nweb = 0\nworker = {}\n", width * 2 - 1),
    )
    .unwrap();
    let out = ok(env.run(&repo, &["sync"]));
    assert_quiet(&out, "growth");
    let grown = block_of(&env, &repo).unwrap();
    assert_eq!(grown.0, block.0, "the running stack's ports must not move");
    assert_eq!(grown.1, block.0 + width * 2 - 1);
}

/// The one place where the wider probe changes what a worktree with its
/// stack up sees: growth that cannot happen in place. `sync` has always
/// refused to move a block something listens on; it now also sees a stack
/// published on `0.0.0.0`, which the old probe missed on macOS (and moved).
#[test]
fn own_stack_blocked_growth_is_refused_and_nothing_moves() {
    let env = TestEnv::new();
    env.write_config("range = [18250, 18299]\n");
    let repo = env.path("own-blocked");
    init_repo(&repo);
    ok(env.run(&repo, &["sync"]));
    let first = block_of(&env, &repo).unwrap();
    let width = first.1 - first.0 + 1;
    let block = (18250, 18250 + width - 1);
    set_block(&env, &repo, block);
    // The neighbour is reserved, so the block cannot grow in place.
    let neighbour = format!("{}-{}", block.1 + 1, block.1 + width);
    ok(env.run(&repo, &["reserve", &neighbour]));
    let _stack = std::net::TcpListener::bind(("0.0.0.0", block.0)).unwrap();

    std::fs::write(
        repo.join(".portool.toml"),
        format!("[ports]\nweb = 0\nworker = {}\n", width * 2 - 1),
    )
    .unwrap();
    let out = env.run(&repo, &["sync"]);
    assert!(!out.status.success(), "a move under a running stack");
    assert!(
        stderr(&out).contains("portool reallocate"),
        "{}",
        stderr(&out)
    );
    assert_eq!(block_of(&env, &repo), Some(block), "no port may move");
}

#[test]
fn a_failed_check_writes_nothing_on_a_path_that_does_not_save() {
    let env = TestEnv::new();
    env.write_config("range = [18300, 18349]\n");
    let (a, b, a_wt) = two_projects(&env);
    let c = env.path("c");
    init_repo(&c);
    std::fs::remove_dir_all(&a_wt).unwrap();
    std::fs::write(a.join(".git").join("HEAD"), "garbage\n").unwrap();
    let state = env.state.join("portool");
    let before = (names(&state), std::fs::read(env.registry_path()).unwrap());

    // `release` in a worktree without an allocation saves nothing.
    let out = ok(env.run(&c, &["release"]));
    assert_eq!(stderr(&out).matches("could not check").count(), 1);
    assert_eq!(
        (names(&state), std::fs::read(env.registry_path()).unwrap()),
        before
    );
    // A command that saves remembers the failure.
    ok(env.run(&b, &["pin"]));
    assert!(state.join("stale-check-failed.json").exists());
}

#[test]
fn own_stack_exec_and_doctor_report_what_they_always_did() {
    let env = TestEnv::new();
    env.write_config("range = [18100, 18149]\n");
    let (repo, block, stack) = repo_with_its_stack_up(&env, "own-exec");

    let out = ok(env.run(&repo, &["exec", "--", "true"]));
    assert_quiet(&out, "exec");
    assert_eq!(block_of(&env, &repo), Some(block));
    drop(stack);

    // The opt-in check and doctor's report look at the worktree's own
    // block exactly as before: from 127.0.0.1 only. A listener that probe
    // never saw (IPv6 loopback) still raises nothing and moves nothing.
    if let Ok(_v6) = std::net::TcpListener::bind(("::1", block.0)) {
        for flags in [
            vec!["exec", "--check-ports", "--", "true"],
            vec!["exec", "--strict", "--", "true"],
            vec!["exec", "--reallocate-on-conflict", "--", "true"],
        ] {
            let out = ok(env.run(&repo, &flags));
            assert_quiet(&out, &format!("{flags:?}"));
            assert_eq!(block_of(&env, &repo), Some(block), "{flags:?}");
        }
        let out = env.run(&repo, &["doctor"]);
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(!text.contains("has ports in use"), "{text}");
    }

    // And one it always saw is still worded as possibly the worktree's own.
    let _v4 = std::net::TcpListener::bind(("127.0.0.1", block.0)).unwrap();
    let out = env.run(&repo, &["doctor"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("may be this worktree's own processes"),
        "{text}"
    );
    let out = ok(env.run(&repo, &["exec", "--check-ports", "--", "true"]));
    assert!(
        stderr(&out).contains("may be this worktree's own"),
        "{}",
        stderr(&out)
    );
    assert_eq!(block_of(&env, &repo), Some(block));
}

#[test]
fn own_stack_is_untouched_by_prune_and_by_other_projects_commands() {
    let env = TestEnv::new();
    env.write_config("range = [18150, 18199]\n");
    let (repo, block, _stack) = repo_with_its_stack_up(&env, "own-prune");
    let other = env.path("other");
    init_repo(&other);

    ok(env.run(&other, &["sync"]));
    let port = unallocated_ports(&env, (18150, 18199))[0].clone();
    assert_ne!(block_of(&env, &other).map(|b| b.0), Some(block.0));
    for (dir, args) in [
        (&repo, vec!["prune"]),
        (&repo, vec!["prune", "--all"]),
        (&repo, vec!["prune", "--all", "--forget-unverifiable"]),
        (&other, vec!["pin"]),
        (&other, vec!["reserve", port.as_str()]),
        (&other, vec!["reallocate"]),
        (&other, vec!["release"]),
    ] {
        let out = ok(env.run(dir, &args));
        assert_quiet(&out, &format!("{args:?}"));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout)
                .matches("pruned")
                .count(),
            0
        );
        assert_eq!(block_of(&env, &repo), Some(block), "{args:?}");
    }
}

#[test]
fn own_stack_reallocate_moves_only_because_it_was_asked_to() {
    let env = TestEnv::new();
    env.write_config("range = [18200, 18249]\n");
    let (repo, block, _stack) = repo_with_its_stack_up(&env, "own-realloc");
    let out = ok(env.run(&repo, &["reallocate"]));
    assert_quiet(&out, "reallocate");
    assert_ne!(
        block_of(&env, &repo),
        Some(block),
        "reallocate always moves"
    );
}
