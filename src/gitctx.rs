//! Thin `git` command-line wrapper: project/worktree identity discovery
//! and `git worktree list` parsing (spec §6.1). The only external
//! processes this crate spawns are `git rev-parse`, `git symbolic-ref
//! --short -q HEAD`, `git worktree list --porcelain` (for another project: with a
//! time limit, see [`worktree_listing_bounded`]), and `git config
//! --type=path --get`.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The git identity of a worktree at some `cwd`: which project it belongs
/// to, where its root is, and its current branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCtx {
    /// `realpath(git rev-parse --git-common-dir)` -- identifies the
    /// project. Resolves to the same path whether run from the main
    /// worktree or any linked worktree.
    pub common_dir: PathBuf,
    /// `realpath(git rev-parse --show-toplevel)` for the worktree at
    /// `cwd`.
    pub worktree_root: PathBuf,
    /// The short branch name, or `None` for detached HEAD (frozen
    /// decision 1).
    pub branch: Option<String>,
    /// Display name inferred from `common_dir` (frozen decision 16).
    pub project_name: String,
}

impl GitCtx {
    /// Discovers the git identity of the repository containing `cwd`.
    ///
    /// Returns [`Error::General`] if `cwd` is not inside a git repository.
    pub fn discover(cwd: &Path) -> Result<GitCtx> {
        let common_dir_raw = run_git_checked(cwd, &["rev-parse", "--git-common-dir"])
            .map_err(|e| not_inside_a_git_repository(cwd, &e))?;
        let common_dir = canonicalize(&cwd.join(common_dir_raw.trim()))?;

        let worktree_root_raw = run_git_checked(cwd, &["rev-parse", "--show-toplevel"])
            .map_err(|e| not_inside_a_git_repository(cwd, &e))?;
        let worktree_root = canonicalize(&cwd.join(worktree_root_raw.trim()))?;

        // Ledger keys are JSON strings; a non-UTF-8 path would be keyed by
        // its lossy conversion, under which two distinct paths can collide
        // and silently share a block. Fail closed instead (external review
        // v0.6 #5) -- possible on Linux only; APFS/HFS+ enforce UTF-8.
        require_utf8(&common_dir)?;
        require_utf8(&worktree_root)?;

        let branch = run_git(cwd, &["symbolic-ref", "--short", "-q", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let project_name = infer_project_name(&common_dir);

        Ok(GitCtx {
            common_dir,
            worktree_root,
            branch,
            project_name,
        })
    }

    /// Builds the identity of a worktree that may already have been deleted
    /// from disk, for releasing its allocation. `repo_dir` is any live
    /// worktree of the same project (typically the main checkout) and
    /// supplies the project identity; `worktree` is the removed (or still
    /// present) worktree path. The path is keyed exactly as it was when it
    /// existed: canonicalized if it is still there, otherwise its parent
    /// directory canonicalized plus its final component.
    pub fn for_worktree_path(repo_dir: &Path, worktree: &Path) -> Result<GitCtx> {
        let mut ctx = GitCtx::discover(repo_dir)?;
        // Keep the final component's identity. A teardown hook can replace
        // the worktree with a symlink; it must not redirect ledger cleanup.
        let name = worktree.file_name().ok_or_else(|| {
            Error::General(format!(
                "{} has no final path component",
                worktree.display()
            ))
        })?;
        let parent = worktree
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let worktree_root = canonicalize(parent)?.join(name);
        require_utf8(&worktree_root)?;
        ctx.worktree_root = worktree_root;
        ctx.branch = None;
        Ok(ctx)
    }

    /// Lists every worktree (main and linked) belonging to this project, by
    /// parsing `worktree ` lines out of `git worktree list --porcelain`.
    /// Each path is canonicalized when possible; a worktree whose directory
    /// has since vanished is reported with its raw (un-canonicalized) path
    /// instead of being dropped.
    pub fn worktree_list(&self) -> Result<Vec<PathBuf>> {
        worktree_list_at(&self.worktree_root)
    }
}

/// Lists every worktree belonging to the project reachable from `dir`, by
/// running `git worktree list --porcelain` with `dir` as the working
/// directory (spec §6.1). `dir` may be a worktree root, or the project's
/// git-common-dir itself (git supports being pointed directly at a gitdir)
/// -- this is what lets [`crate::cmd::prune`] enumerate a project's
/// worktrees without a live worktree root to run `git` from.
pub fn worktree_list_at(dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(worktree_listing_at(dir)?
        .into_iter()
        .map(|w| w.path)
        .collect())
}

/// One worktree as `git worktree list --porcelain` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub path: PathBuf,
    /// Git printed a `prunable` line: the worktree's directory no longer
    /// holds its `.git` file (removed with `rm -rf`, or on a volume that is
    /// not mounted -- git cannot tell these apart, and neither does this
    /// flag). Printed by git 2.31 and later; an older git never sets it.
    pub prunable: bool,
    /// Git printed a `locked` line (`git worktree lock`, also git >= 2.31).
    pub locked: bool,
    /// Git printed a `bare` line: this entry is the repository itself, not
    /// a checkout. (Git before 2.22 also printed it for any common
    /// directory not named `.git`, so it is never trusted on its own.)
    pub bare: bool,
}

/// [`worktree_list_at`] with each worktree's `prunable` / `locked` state.
pub fn worktree_listing_at(dir: &Path) -> Result<Vec<Listed>> {
    // Prefer `-z` (git >= 2.36): its NUL-separated records safely carry
    // worktree paths that contain newlines, and preserve non-UTF-8 bytes.
    // Fall back to the newline parser on older git rather than failing the
    // whole slow path (batch D #12).
    if let Some(bytes) = run_git_bytes(dir, &["worktree", "list", "--porcelain", "-z"]) {
        return Ok(parse_listing_z(&bytes));
    }
    let output = run_git_checked(dir, &["worktree", "list", "--porcelain"])?;
    Ok(parse_listing_lines(&output))
}

/// [`worktree_listing_at`] for a repository this command is not running in,
/// which may sit on a dead network mount or be otherwise broken: git is
/// killed (the one child spawned here, by its handle) when it has not
/// finished within `timeout`, and a timeout is never retried.
pub fn worktree_listing_bounded(
    dir: &Path,
    timeout: std::time::Duration,
) -> std::result::Result<Vec<Listed>, Bounded> {
    let deadline = std::time::Instant::now() + timeout;
    match run_git_bounded(dir, &["worktree", "list", "--porcelain", "-z"], deadline) {
        Ok(bytes) => return Ok(parse_listing_z(&bytes)),
        Err(Bounded::Failed(_)) => {} // possibly git < 2.36: retry without -z
        Err(other) => return Err(other),
    }
    let bytes = run_git_bounded(dir, &["worktree", "list", "--porcelain"], deadline)?;
    Ok(parse_listing_lines(&String::from_utf8_lossy(&bytes)))
}

/// What the repository's own configuration says about its main checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainConfig {
    /// `core.bare` is set and true: the repository has no main checkout.
    pub bare: bool,
    /// `core.worktree` is set: the main checkout is wherever it points,
    /// which `git worktree list` does not show.
    pub worktree_set: bool,
}

/// Reads [`MainConfig`] for the repository whose common directory is `dir`,
/// with the same time limit and kill as [`worktree_listing_bounded`].
pub fn main_config_bounded(
    dir: &Path,
    timeout: std::time::Duration,
) -> std::result::Result<MainConfig, Bounded> {
    let deadline = std::time::Instant::now() + timeout;
    let bytes = run_git_bounded(dir, &["config", "-z", "--list"], deadline)?;
    Ok(parse_main_config(&bytes))
}

/// Parses `git config -z --list`: records `key\nvalue\0` (or `key\0` for a
/// key without a value, which git reads as true); the last one wins.
fn parse_main_config(bytes: &[u8]) -> MainConfig {
    let mut out = MainConfig {
        bare: false,
        worktree_set: false,
    };
    for record in bytes.split(|&b| b == 0) {
        let mut parts = record.splitn(2, |&b| b == b'\n');
        let key = parts.next().unwrap_or_default();
        let value = parts.next();
        if key.eq_ignore_ascii_case(b"core.worktree") {
            out.worktree_set = true;
        } else if key.eq_ignore_ascii_case(b"core.bare") {
            out.bare = match value {
                None => true,
                Some(v) => [&b"true"[..], b"yes", b"on", b"1"]
                    .iter()
                    .any(|t| v.eq_ignore_ascii_case(t)),
            };
        }
    }
    out
}

/// Why a bounded git call gave no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bounded {
    /// Git was killed at the time limit.
    TimedOut,
    /// Git ran and failed, or could not be started.
    Failed(String),
    /// This process could not set the call up (no temp file for git's
    /// output ...): says nothing about the repository.
    Local(String),
}

/// Runs git with its output in anonymous temp files (no pipe, so neither a
/// full pipe nor a grandchild holding one open can block this process) and
/// polls for its exit until `deadline`.
fn run_git_bounded(
    cwd: &Path,
    args: &[&str],
    deadline: std::time::Instant,
) -> std::result::Result<Vec<u8>, Bounded> {
    run_bounded_command(git_command(cwd, args), deadline).map_err(|e| match e {
        Bounded::Failed(reason) => Bounded::Failed(format!("git {}: {reason}", args.join(" "))),
        other => other,
    })
}

/// [`run_git_bounded`] for an already built command.
fn run_bounded_command(
    mut command: Command,
    deadline: std::time::Instant,
) -> std::result::Result<Vec<u8>, Bounded> {
    use std::io::{Read, Seek, SeekFrom};
    use std::process::Stdio;
    let failed = |what: &str, e: &dyn std::fmt::Display| Bounded::Failed(format!("{what}: {e}"));
    let local = |e: std::io::Error| {
        Bounded::Local(format!(
            "no temporary file for git's output in {}: {e}",
            std::env::temp_dir().display()
        ))
    };
    let mut out = tempfile::tempfile().map_err(local)?;
    let mut err = tempfile::tempfile().map_err(local)?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(out.try_clone().map_err(local)?)
        .stderr(err.try_clone().map_err(local)?)
        .spawn()
        .map_err(|e| failed("could not be started", &e))?;
    let mut pause = std::time::Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                // `child` is the process spawned above and not yet reaped,
                // so its pid cannot have been reused.
                let _ = child.kill();
                let _ = child.wait();
                return Err(Bounded::TimedOut);
            }
            Ok(None) => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(std::time::Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(failed("could not be waited for", &e));
            }
        }
    };
    if !status.success() {
        let mut text = String::new();
        let _ = err.seek(SeekFrom::Start(0));
        let _ = err.take(400).read_to_string(&mut text);
        return Err(Bounded::Failed(format!(
            "failed ({status}): {}",
            text.trim()
        )));
    }
    let mut bytes = Vec::new();
    out.seek(SeekFrom::Start(0))
        .and_then(|_| out.read_to_end(&mut bytes))
        .map_err(|e| failed("output could not be read", &e))?;
    Ok(bytes)
}

/// The path a ledger key for `path` would have: canonical when the
/// directory exists; otherwise the canonical form of its nearest existing
/// ancestor plus the missing tail (so a removed worktree under a symlinked
/// temp directory still compares equal to its ledger key).
fn ledger_form(path: PathBuf) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(&path) {
        return real;
    }
    let mut tail = Vec::new();
    let mut cursor = path.as_path();
    while let (Some(parent), Some(name)) = (cursor.parent(), cursor.file_name()) {
        tail.push(name.to_os_string());
        if let Ok(mut real) = std::fs::canonicalize(parent) {
            real.extend(tail.iter().rev());
            return real;
        }
        cursor = parent;
    }
    path
}

/// Folds one porcelain attribute line into the worktree being read.
fn listing_field(items: &mut Vec<Listed>, field: &[u8]) {
    use std::os::unix::ffi::OsStrExt;
    let is = |name: &[u8]| {
        field == name || (field.starts_with(name) && field.get(name.len()) == Some(&b' '))
    };
    if let Some(raw) = field.strip_prefix(b"worktree ") {
        items.push(Listed {
            path: ledger_form(PathBuf::from(std::ffi::OsStr::from_bytes(raw))),
            prunable: false,
            locked: false,
            bare: false,
        });
    } else if let Some(last) = items.last_mut() {
        if is(b"prunable") {
            last.prunable = true;
        } else if is(b"locked") {
            last.locked = true;
        } else if is(b"bare") {
            last.bare = true;
        }
    }
}

/// Parses the NUL-separated records of `git worktree list --porcelain -z`.
fn parse_listing_z(bytes: &[u8]) -> Vec<Listed> {
    let mut items = Vec::new();
    for field in bytes.split(|&b| b == 0) {
        listing_field(&mut items, field);
    }
    items
}

/// Fallback line parser for git older than 2.36 (no `-z`). A worktree path
/// containing a newline can't be represented faithfully here -- the reason
/// `-z` is preferred.
fn parse_listing_lines(output: &str) -> Vec<Listed> {
    let mut items = Vec::new();
    for line in output.lines() {
        listing_field(&mut items, line.as_bytes());
    }
    items
}

#[cfg(test)]
fn parse_worktree_list_z(bytes: &[u8]) -> Vec<PathBuf> {
    parse_listing_z(bytes).into_iter().map(|w| w.path).collect()
}

/// Reads a path-valued git config key via `git config --type=path --get`
/// (so `~` is expanded by git itself), returning `None` when the key is
/// unset or git fails. Relative values are returned as-is; resolving them
/// against the right base is the caller's job.
pub fn config_path_value(dir: &Path, key: &str) -> Option<String> {
    run_git(dir, &["config", "--type=path", "--get", key])
        .map(|s| s.trim_end().to_string())
        .filter(|s| !s.is_empty())
}

/// The git config scope a key's effective value comes from -- the leading
/// token of `git config --show-scope --get <key>` (`local`, `global`,
/// `system`, or `worktree`). Returns `None` when the key is unset or git
/// fails (e.g. `--show-scope` predates git 2.26).
pub fn config_scope(dir: &Path, key: &str) -> Option<String> {
    run_git(dir, &["config", "--show-scope", "--get", key])
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
}

/// Environment variables through which a *parent* git process (most
/// notably a git hook, per githooks(5)) pins its child processes to its own
/// repository. If inherited, they override `git -C <cwd>`'s target -- a
/// `portool` invoked from inside a hook would then operate on the parent
/// repo instead of the one at `cwd`, which is silently wrong at best and,
/// for something like `prune --all` iterating many repos, actively
/// destructive at worst. Cleared on every git spawn via [`git_command`].
const GIT_REPO_ENV_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
];

/// Builds a `git -C <cwd> <args>` command with every repo-pinning
/// environment variable in [`GIT_REPO_ENV_VARS`] removed, so `-C` is always
/// the sole source of truth for which repository it operates on. Every git
/// spawn in this module goes through this constructor.
fn git_command(cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(cwd).args(args);
    for var in GIT_REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd
}

/// Runs `git -C <cwd> <args>`, returning stdout as a `String` on success
/// (exit code 0), or `None` if the process could not be spawned, exited
/// non-zero, or produced non-UTF-8 output. For callers where absence is a
/// normal outcome (config lookups, `symbolic-ref`); callers that need to
/// distinguish *why* it failed should use [`run_git_checked`] instead.
fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = git_command(cwd, args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Like [`run_git`] but returns raw stdout bytes (for `-z` output that may
/// contain non-UTF-8 paths), or `None` if the process failed to spawn or
/// exited non-zero.
fn run_git_bytes(cwd: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = git_command(cwd, args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// Like [`run_git`], but returns a [`Result`] that distinguishes *why* it
/// failed instead of collapsing spawn failures, a non-zero exit, and
/// non-UTF-8 output into a single `None` -- so callers for whom failure is
/// unexpected can report git's own stderr instead of a misleading generic
/// message.
fn run_git_checked(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = git_command(cwd, args)
        .output()
        .map_err(|e| Error::General(format!("failed to run git {}: {e}", args.join(" "))))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::General(format!(
            "git {} failed ({}): {}",
            args.join(" "),
            output.status,
            stderr.trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| Error::General(format!("git {} produced non-UTF-8 output", args.join(" "))))
}

/// Wraps a [`run_git_checked`] failure from `discover`'s rev-parse calls
/// with the legacy "not inside a git repository" wording that tests and
/// users depend on, while still surfacing git's own error text (e.g. "not a
/// git repository (or any of the parent directories)") instead of swallowing
/// it -- distinguishing a genuine "no repo here" from an unrelated spawn
/// failure or a git bug.
fn not_inside_a_git_repository(cwd: &Path, cause: &Error) -> Error {
    Error::General(format!(
        "{} is not inside a git repository ({cause})",
        cwd.display()
    ))
}

fn canonicalize(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path)
        .map_err(|e| Error::General(format!("failed to resolve {}: {e}", path.display())))
}

/// Rejects a path whose bytes are not valid UTF-8: it could not be keyed in
/// the ledger without a lossy conversion under which distinct paths collide.
fn require_utf8(path: &Path) -> Result<()> {
    if path.to_str().is_none() {
        return Err(Error::General(format!(
            "{} is not valid UTF-8; portool requires UTF-8 repository and worktree paths",
            path.display()
        )));
    }
    Ok(())
}

/// Infers a project's display name from its common dir (frozen decision
/// 16): if the common dir's final component is `.git`, use its parent
/// directory's name; otherwise use the common dir's own final component.
fn infer_project_name(common_dir: &Path) -> String {
    let file_name = common_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    if file_name == ".git" {
        common_dir
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(file_name)
    } else {
        file_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Output;
    use tempfile::TempDir;

    /// Runs a git command isolated from the host machine's real global /
    /// system config (so a signing key requirement, custom hooksPath, etc.
    /// on the developer's machine can never affect these tests).
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
        assert!(git(dir, &["init", "-q", "-b", "main"]).status.success());
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
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

    #[test]
    fn discover_resolves_main_worktree() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myapp");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);

        let ctx = GitCtx::discover(&repo).unwrap();

        assert_eq!(ctx.worktree_root, std::fs::canonicalize(&repo).unwrap());
        assert_eq!(
            ctx.common_dir,
            std::fs::canonicalize(repo.join(".git")).unwrap()
        );
        assert_eq!(ctx.branch.as_deref(), Some("main"));
        assert_eq!(ctx.project_name, "myapp");
    }

    #[test]
    fn discover_outside_repo_is_general_error() {
        let tmp = TempDir::new().unwrap();

        let err = GitCtx::discover(tmp.path()).unwrap_err();

        assert_eq!(err.exit_code(), 1);
    }

    /// P1 (external review v0.9): the old `run_git` collapsed every failure
    /// mode (spawn failure, non-zero exit, non-UTF-8 output) into a bare
    /// `None`, so `discover` could only ever report the generic "not inside
    /// a git repository" -- even if the real cause was something else
    /// entirely. It must now keep that wording (existing tests and users
    /// depend on it) while also surfacing git's own stderr.
    #[test]
    fn discover_error_message_includes_git_stderr() {
        let tmp = TempDir::new().unwrap();

        let err = GitCtx::discover(tmp.path()).unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("is not inside a git repository"),
            "must keep the legacy wording, got: {msg}"
        );
        assert!(
            msg.contains("not a git repository"),
            "must surface git's own stderr, got: {msg}"
        );
    }

    #[test]
    fn linked_worktree_resolves_to_same_common_dir() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);

        let wt = tmp.path().join("repo-wt");
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

        let main_ctx = GitCtx::discover(&repo).unwrap();
        let wt_ctx = GitCtx::discover(&wt).unwrap();

        assert_eq!(main_ctx.common_dir, wt_ctx.common_dir);
        assert_ne!(main_ctx.worktree_root, wt_ctx.worktree_root);
        assert_eq!(wt_ctx.branch.as_deref(), Some("feature/api"));
        assert_eq!(wt_ctx.project_name, "repo");
    }

    #[test]
    fn detached_head_has_no_branch() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);

        let head_output = git(&repo, &["rev-parse", "HEAD"]);
        let rev = String::from_utf8(head_output.stdout).unwrap();
        assert!(git(&repo, &["checkout", "-q", rev.trim()]).status.success());

        let ctx = GitCtx::discover(&repo).unwrap();

        assert_eq!(ctx.branch, None);
    }

    #[test]
    fn worktree_list_at_accepts_the_common_dir_itself() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);

        let ctx = GitCtx::discover(&repo).unwrap();
        let list = worktree_list_at(&ctx.common_dir).unwrap();

        assert_eq!(list, vec![std::fs::canonicalize(&repo).unwrap()]);
    }

    #[test]
    fn worktree_list_includes_main_and_linked_worktrees() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);

        let wt = tmp.path().join("repo-wt");
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

        let ctx = GitCtx::discover(&repo).unwrap();
        let list = ctx.worktree_list().unwrap();

        assert_eq!(list.len(), 2);
        assert!(list.contains(&std::fs::canonicalize(&repo).unwrap()));
        assert!(list.contains(&std::fs::canonicalize(&wt).unwrap()));
    }

    #[test]
    fn worktree_list_from_linked_worktree_sees_all_worktrees() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);

        let wt = tmp.path().join("repo-wt");
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

        let ctx = GitCtx::discover(&wt).unwrap();
        let list = ctx.worktree_list().unwrap();

        assert_eq!(list.len(), 2);
    }

    #[test]
    fn parse_worktree_list_z_preserves_newline_in_path() {
        // `-z` records are NUL-separated; a worktree path containing a
        // newline must survive intact (the whole point of `-z`, batch D #12).
        let bytes =
            b"worktree /a/normal\0HEAD abc\0branch refs/heads/main\0\0worktree /b/new\nline\0HEAD def\0\0";
        let paths = parse_worktree_list_z(bytes);
        assert!(paths.iter().any(|p| p == Path::new("/a/normal")));
        assert!(
            paths.iter().any(|p| p == Path::new("/b/new\nline")),
            "a newline-bearing path must be preserved, got: {paths:?}"
        );
    }

    #[test]
    fn listing_reads_prunable_and_locked_with_or_without_a_reason() {
        let bytes = b"worktree /a/main\0HEAD abc\0branch refs/heads/main\0\0\
            worktree /a/gone\0HEAD def\0detached\0prunable gitdir file points to non-existent location\0\0\
            worktree /a/locked\0HEAD def\0locked\0\0\
            worktree /a/locked-why\0HEAD def\0locked on a usb disk\0\0\
            worktree /a/prunable-bare\0prunable\0\0\
            worktree /a/lockedish\0branch refs/heads/locked\0\0";
        let got: Vec<(String, bool, bool)> = parse_listing_z(bytes)
            .into_iter()
            .map(|w| (w.path.to_string_lossy().into_owned(), w.prunable, w.locked))
            .collect();
        let want = [
            ("/a/main", false, false),
            ("/a/gone", true, false),
            ("/a/locked", false, true),
            ("/a/locked-why", false, true),
            ("/a/prunable-bare", true, false),
            ("/a/lockedish", false, false),
        ];
        assert_eq!(got.len(), want.len());
        for ((path, prunable, locked), (p, pr, lo)) in got.iter().zip(want) {
            assert_eq!((path.as_str(), *prunable, *locked), (p, pr, lo));
        }
        // The line form (git < 2.36) reads the same attributes.
        let lines = "worktree /a/main\nHEAD abc\n\nworktree /a/gone\nprunable reason\n\n";
        let got = parse_listing_lines(lines);
        assert_eq!(got.len(), 2);
        assert!(!got[0].prunable && got[1].prunable);
    }

    #[test]
    fn ledger_form_resolves_the_existing_part_of_a_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(tmp.path()).unwrap();
        std::os::unix::fs::symlink(&real, real.join("link")).unwrap();
        assert_eq!(
            ledger_form(real.join("link").join("gone").join("wt")),
            real.join("gone").join("wt")
        );
        assert_eq!(ledger_form(real.join("link")), real);
    }

    #[test]
    fn main_config_reads_bare_and_core_worktree() {
        let cfg = |s: &[u8]| parse_main_config(s);
        let plain = cfg(b"core.bare\nfalse\0core.filemode\ntrue\0");
        assert!(!plain.bare && !plain.worktree_set);
        assert!(cfg(b"core.bare\ntrue\0").bare);
        assert!(cfg(b"core.bare\0").bare, "a key without a value is true");
        assert!(
            !cfg(b"core.bare\ntrue\0core.bare\nfalse\0").bare,
            "last wins"
        );
        assert!(!cfg(b"core.bare\nmaybe\0").bare, "unknown is not bare");
        assert!(cfg(b"core.worktree\n../../sub\0").worktree_set);
        assert!(!cfg(b"core.worktreex\n1\0user.name\ncore.worktree\0").worktree_set);
        assert!(!cfg(b"").bare);
    }

    #[test]
    fn listing_marks_the_bare_entry() {
        let got = parse_listing_z(b"worktree /a/r.git\0bare\0\0worktree /a/wt\0HEAD x\0\0");
        assert!(got[0].bare && !got[1].bare);
    }

    #[test]
    fn a_missing_temp_directory_is_not_reported_as_a_git_failure() {
        // The child of this test only: TMPDIR is set on the command, and
        // `tempfile` reads the parent's, so the parent's is what must be
        // unusable -- done in a child process to leave this one alone.
        let exe = std::env::current_exe().unwrap();
        if std::env::var_os("PORTOOL_TEST_NO_TMP").is_some() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let out = run_bounded_command(Command::new("true"), deadline);
            assert!(matches!(out, Err(Bounded::Local(_))), "{out:?}");
            return;
        }
        let status = Command::new(exe)
            .args([
                "--exact",
                "gitctx::tests::a_missing_temp_directory_is_not_reported_as_a_git_failure",
                "--nocapture",
            ])
            .env("PORTOOL_TEST_NO_TMP", "1")
            .env("TMPDIR", "/nonexistent-portool-test-tmp")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn bounded_listing_kills_a_git_that_does_not_answer() {
        // A stand-in for a git that hangs: a `sleep` this test spawns.
        let started = std::time::Instant::now();
        let deadline = started + std::time::Duration::from_millis(300);
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let out = run_bounded_command(cmd, deadline);
        assert!(matches!(out, Err(Bounded::TimedOut)));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn require_utf8_rejects_non_utf8_bytes_only() {
        use std::os::unix::ffi::OsStrExt;
        let bad = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff\xfe"));
        assert!(require_utf8(bad).is_err());
        assert!(require_utf8(Path::new("/tmp/ok")).is_ok());
    }

    #[test]
    fn project_name_falls_back_to_common_dir_component_when_not_dot_git() {
        // A bare-repo-style common dir whose final component isn't
        // `.git` should use that final component directly.
        assert_eq!(
            infer_project_name(Path::new("/srv/repos/blog.git")),
            "blog.git"
        );
        assert_eq!(
            infer_project_name(Path::new("/home/t/dev/myapp/.git")),
            "myapp"
        );
    }
}
