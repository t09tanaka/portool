//! `portool init` (spec §9.1, frozen decisions 2, 6, 7, 8): installs the
//! post-checkout hook into the repository's *effective* hooks location
//! (honoring `core.hooksPath` / Husky — see `crate::hooks`), appends
//! `.env.portool` to `.gitignore`, and runs `sync` once.

use crate::cmd::sync;
use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::hooks::{HooksLocation, HOOK_MARKER};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// The full script written when no post-checkout hook exists yet. The
/// `command -v` guard sits inside `if`/`fi` so the hook exits 0 when
/// portool isn't installed — even under hook managers that run hooks with
/// `sh -e` and propagate exit codes (e.g. Husky).
const HOOK_SCRIPT: &str = "#!/bin/sh\n\
# installed by portool\n\
if command -v portool >/dev/null 2>&1; then\n\
\x20\x20portool sync --quiet\n\
fi\n";
/// The single line appended to an existing hook (same guard, one-liner).
const HOOK_APPEND_LINE: &str =
    "if command -v portool >/dev/null 2>&1; then portool sync --quiet; fi\n";
const GITIGNORE_LINE: &str = ".env.portool";

/// Runs `portool init`. With neither flag, installs the hook, updates
/// `.gitignore`, and runs `sync`; `--hook-only`/`--gitignore-only` (clap
/// enforces they're mutually exclusive) each perform just their one step.
pub fn run(hook_only: bool, gitignore_only: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;

    if hook_only {
        return install_hook(&ctx);
    }
    if gitignore_only {
        return update_gitignore(&ctx.worktree_root);
    }

    install_hook(&ctx)?;
    update_gitignore(&ctx.worktree_root)?;
    sync::run(false)
}

/// Frozen decisions 2 & 8, extended for `core.hooksPath`: installs the
/// post-checkout hook where git will actually run it. When that's nowhere
/// safe (`core.hooksPath` points at a directory that doesn't exist and
/// isn't Husky's), warns with concrete instructions instead of silently
/// writing to the unused `<common_dir>/hooks`.
fn install_hook(ctx: &GitCtx) -> Result<()> {
    match HooksLocation::resolve(ctx) {
        HooksLocation::GitDefault { hooks_dir } | HooksLocation::Custom { hooks_dir } => {
            install_into(&hooks_dir.join("post-checkout"))
        }
        HooksLocation::Husky { hook_file } => {
            install_into(&hook_file)?;
            let display = hook_file
                .strip_prefix(&ctx.worktree_root)
                .unwrap_or(&hook_file);
            eprintln!(
                "portool: Husky detected; installed {} (a tracked file -- commit it to share the hook)",
                display.display()
            );
            eprintln!(
                "note: Husky hooks can't fire on a brand-new worktree's first checkout \
                 (its .husky/_ is only generated later, e.g. by 'npm install'); run \
                 'portool sync' once in new worktrees, or add 'portool sync --quiet' to \
                 your package.json \"prepare\" script"
            );
            Ok(())
        }
        HooksLocation::Missing {
            configured,
            resolved,
        } => {
            eprintln!(
                "warning: core.hooksPath is set to '{configured}' but {} is not an existing \
                 directory; git would ignore a hook installed at <git-common-dir>/hooks, so \
                 nothing was installed",
                resolved.display()
            );
            eprintln!(
                "hint: once that directory exists, re-run 'portool init --hook-only', or \
                 add this line to the post-checkout hook your hook manager runs:"
            );
            eprintln!("hint:   {}", HOOK_APPEND_LINE.trim_end());
            Ok(())
        }
    }
}

/// Installs into `hook_path`, idempotently. A brand-new hook gets the full
/// script (spec §10.1); an existing hook that doesn't already invoke
/// `portool sync` gets the invocation appended to its end. Either way the
/// executable bit is guaranteed afterward.
fn install_into(hook_path: &Path) -> Result<()> {
    let hooks_dir = hook_path.parent().ok_or_else(|| {
        Error::General(format!("{} has no parent directory", hook_path.display()))
    })?;
    fs::create_dir_all(hooks_dir)?;

    match fs::read_to_string(hook_path) {
        Ok(existing) => {
            if !existing.contains(HOOK_MARKER) {
                let mut content = existing;
                if !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str(HOOK_APPEND_LINE);
                fs::write(hook_path, content)?;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::write(hook_path, HOOK_SCRIPT)?;
        }
        // A hook that exists but can't be read as UTF-8 text (or otherwise)
        // is left untouched rather than risk clobbering it; only the
        // executable bit below is still enforced.
        Err(_) => {}
    }

    let mut perms = fs::metadata(hook_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(hook_path, perms)?;
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
    fn install_into_writes_the_spec_script_and_sets_exec_bit() {
        let tmp = TempDir::new().unwrap();
        let hook_path = tmp.path().join("repo/.git/hooks/post-checkout");

        install_into(&hook_path).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content, HOOK_SCRIPT);
        let mode = fs::metadata(&hook_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn install_into_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let hook_path = tmp.path().join("repo/.git/hooks/post-checkout");

        install_into(&hook_path).unwrap();
        install_into(&hook_path).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content, HOOK_SCRIPT);
    }

    #[test]
    fn install_into_appends_to_an_existing_foreign_hook() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        fs::write(&hook_path, "#!/bin/sh\necho hi\n").unwrap();

        install_into(&hook_path).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(content.starts_with("#!/bin/sh\necho hi\n"));
        assert!(content.contains(HOOK_MARKER));
    }

    #[test]
    fn install_into_does_not_duplicate_an_old_style_marker_line() {
        // Hooks installed by portool <= 0.2 used a bare `command -v ... &&`
        // line; the marker must still recognize them as already installed.
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        let legacy = "#!/bin/sh\n# installed by portool\ncommand -v portool >/dev/null 2>&1 && portool sync --quiet\n";
        fs::write(&hook_path, legacy).unwrap();

        install_into(&hook_path).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content, legacy);
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
