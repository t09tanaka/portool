//! Thin `git` command-line wrapper: project/worktree identity discovery
//! and `git worktree list` parsing (spec §6.1). The only external
//! processes this crate spawns are `git rev-parse`, `git symbolic-ref
//! --short -q HEAD`, `git worktree list --porcelain`, and `git config
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
        let common_dir_raw = run_git(cwd, &["rev-parse", "--git-common-dir"]).ok_or_else(|| {
            Error::General(format!("{} is not inside a git repository", cwd.display()))
        })?;
        let common_dir = canonicalize(&cwd.join(common_dir_raw.trim()))?;

        let worktree_root_raw =
            run_git(cwd, &["rev-parse", "--show-toplevel"]).ok_or_else(|| {
                Error::General(format!("{} is not inside a git repository", cwd.display()))
            })?;
        let worktree_root = canonicalize(&cwd.join(worktree_root_raw.trim()))?;

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
    let output = run_git(dir, &["worktree", "list", "--porcelain"])
        .ok_or_else(|| Error::General("failed to run 'git worktree list'".to_string()))?;

    let mut paths = Vec::new();
    for line in output.lines() {
        if let Some(raw) = line.strip_prefix("worktree ") {
            let path = PathBuf::from(raw);
            paths.push(std::fs::canonicalize(&path).unwrap_or(path));
        }
    }
    Ok(paths)
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

/// Runs `git -C <cwd> <args>`, returning stdout as a `String` on success
/// (exit code 0), or `None` if the process could not be spawned, exited
/// non-zero, or produced non-UTF-8 output.
fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn canonicalize(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path)
        .map_err(|e| Error::General(format!("failed to resolve {}: {e}", path.display())))
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
