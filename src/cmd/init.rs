//! `portool init` (spec §9.1, frozen decisions 2, 6, 7, 8): installs the
//! post-checkout hook, appends `.env.portool` to `.gitignore`, and runs
//! `sync` once.

use crate::cmd::sync;
use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const HOOK_SCRIPT: &str = "#!/bin/sh\n\
# installed by portool\n\
command -v portool >/dev/null 2>&1 && portool sync --quiet\n";
const HOOK_APPEND_LINE: &str = "command -v portool >/dev/null 2>&1 && portool sync --quiet\n";
const HOOK_MARKER: &str = "portool sync";
const GITIGNORE_LINE: &str = ".env.portool";

/// Runs `portool init`. With neither flag, installs the hook, updates
/// `.gitignore`, and runs `sync`; `--hook-only`/`--gitignore-only` (clap
/// enforces they're mutually exclusive) each perform just their one step.
pub fn run(hook_only: bool, gitignore_only: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;

    if hook_only {
        return install_hook(&ctx.common_dir);
    }
    if gitignore_only {
        return update_gitignore(&ctx.worktree_root);
    }

    install_hook(&ctx.common_dir)?;
    update_gitignore(&ctx.worktree_root)?;
    sync::run(false)
}

/// Frozen decisions 2 & 8: installs `<common_dir>/hooks/post-checkout`,
/// idempotently. A brand-new hook gets the full 3-line script (spec
/// §10.1); an existing hook that doesn't already invoke `portool sync` gets
/// the invocation appended to its end. Either way the executable bit is
/// guaranteed afterward.
fn install_hook(common_dir: &Path) -> Result<()> {
    let hooks_dir = common_dir.join("hooks");
    fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join("post-checkout");

    match fs::read_to_string(&hook_path) {
        Ok(existing) => {
            if !existing.contains(HOOK_MARKER) {
                let mut content = existing;
                if !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str(HOOK_APPEND_LINE);
                fs::write(&hook_path, content)?;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::write(&hook_path, HOOK_SCRIPT)?;
        }
        // A hook that exists but can't be read as UTF-8 text (or otherwise)
        // is left untouched rather than risk clobbering it; only the
        // executable bit below is still enforced.
        Err(_) => {}
    }

    let mut perms = fs::metadata(&hook_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook_path, perms)?;
    Ok(())
}

/// Frozen decision 7: appends `.env.portool` to the current worktree
/// root's `.gitignore`, idempotently (no-op if that exact line is already
/// present).
fn update_gitignore(worktree_root: &Path) -> Result<()> {
    let path = worktree_root.join(".gitignore");
    let existing = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(Error::from(err)),
    };

    if existing.lines().any(|line| line == GITIGNORE_LINE) {
        return Ok(());
    }

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(GITIGNORE_LINE);
    content.push('\n');
    fs::write(&path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn install_hook_writes_the_spec_script_and_sets_exec_bit() {
        let tmp = TempDir::new().unwrap();
        let common_dir = tmp.path().join("repo/.git");
        fs::create_dir_all(&common_dir).unwrap();

        install_hook(&common_dir).unwrap();

        let hook_path = common_dir.join("hooks/post-checkout");
        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content, HOOK_SCRIPT);
        let mode = fs::metadata(&hook_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn install_hook_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let common_dir = tmp.path().join("repo/.git");
        fs::create_dir_all(&common_dir).unwrap();

        install_hook(&common_dir).unwrap();
        install_hook(&common_dir).unwrap();

        let content = fs::read_to_string(common_dir.join("hooks/post-checkout")).unwrap();
        assert_eq!(content, HOOK_SCRIPT);
    }

    #[test]
    fn install_hook_appends_to_an_existing_foreign_hook() {
        let tmp = TempDir::new().unwrap();
        let common_dir = tmp.path().join("repo/.git");
        let hooks_dir = common_dir.join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        fs::write(hooks_dir.join("post-checkout"), "#!/bin/sh\necho hi\n").unwrap();

        install_hook(&common_dir).unwrap();

        let content = fs::read_to_string(hooks_dir.join("post-checkout")).unwrap();
        assert!(content.starts_with("#!/bin/sh\necho hi\n"));
        assert!(content.contains(HOOK_MARKER));
    }

    #[test]
    fn update_gitignore_creates_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        update_gitignore(root).unwrap();
        update_gitignore(root).unwrap();

        let content = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(content, ".env.portool\n");
    }

    #[test]
    fn update_gitignore_preserves_existing_content() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join(".gitignore"), "node_modules\n").unwrap();

        update_gitignore(root).unwrap();

        let content = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(content, "node_modules\n.env.portool\n");
    }
}
