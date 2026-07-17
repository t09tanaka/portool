//! `portool init` (spec §9.1, frozen decisions 2, 6, 7, 8; hardening batch
//! A): installs the `post-checkout` and `post-merge` hooks into the
//! repository's *effective* hooks location (honoring `core.hooksPath` /
//! Husky, and refusing shared global/system scope -- see `crate::hooks`),
//! appends `.env.portool` to `.gitignore`, and runs `sync` once. The
//! installed hooks always exit 0, so a portool failure never fails the
//! caller's git command.

use crate::cmd::sync;
use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::hooks::HooksLocation;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// The full script written when no hook exists yet. The `command -v` guard
/// makes the hook a no-op (exit 0) when portool isn't installed; the
/// `|| echo … >&2` and trailing `exit 0` make it exit 0 *even when portool
/// is installed and `sync` fails*, so a portool problem can never turn
/// `git checkout` / `git worktree add` into a failure (batch A #1).
const HOOK_SCRIPT: &str = "#!/bin/sh\n\
# installed by portool\n\
if command -v portool >/dev/null 2>&1; then\n\
\x20\x20portool sync --quiet || echo 'portool: sync failed; Git was not blocked' >&2\n\
fi\n\
exit 0\n";

/// The single line appended to an existing (foreign) hook. `|| true` means
/// portool's invocation -- the last line of the hook -- can never become the
/// hook's failing exit status.
const HOOK_APPEND_LINE: &str =
    "if command -v portool >/dev/null 2>&1; then portool sync --quiet || true; fi\n";

/// Whole-file portool scripts from earlier versions that portool owns
/// outright and that propagate `sync`'s failure (no `exit 0` / `|| true`).
/// Matched after trimming a single trailing newline; a match is rewritten to
/// the current safe [`HOOK_SCRIPT`].
const OWNED_FULL_SCRIPTS: &[&str] = &[
    // portool <= 0.2
    "#!/bin/sh\n# installed by portool\ncommand -v portool >/dev/null 2>&1 && portool sync --quiet",
    // portool 0.3 / 0.4 (unsafe: no exit 0)
    "#!/bin/sh\n# installed by portool\nif command -v portool >/dev/null 2>&1; then\n  portool sync --quiet\nfi",
];

/// Single unsafe portool lines that may sit inside a *foreign* hook. Matched
/// on the trimmed line; a match is rewritten to the safe [`HOOK_APPEND_LINE`].
/// These are the exact shapes portool itself emitted -- a line a user merely
/// wrote by hand that happens to mention `portool sync` is never one of them,
/// so it is left untouched (batch A #2, Fable review).
const UNSAFE_PORTOOL_LINES: &[&str] = &[
    "command -v portool >/dev/null 2>&1 && portool sync --quiet",
    "if command -v portool >/dev/null 2>&1; then portool sync --quiet; fi",
];

const GITIGNORE_LINE: &str = ".env.portool";

/// Runs `portool init`. With neither flag, installs the hooks, updates
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

/// Installs portool's hooks where git will actually run them. When that's
/// nowhere safe -- a `core.hooksPath` dir that doesn't exist / isn't Husky's
/// (`Missing`), or an absolute global/system-scope shared dir
/// (`SharedScope`) -- it warns with the manual line instead of writing.
fn install_hook(ctx: &GitCtx) -> Result<()> {
    let loc = HooksLocation::resolve(ctx);
    match &loc {
        HooksLocation::GitDefault { .. } | HooksLocation::Custom { .. } => {
            install_managed_hooks(&loc)
        }
        HooksLocation::Husky { .. } => {
            install_managed_hooks(&loc)?;
            let hook_file = loc
                .hook_file("post-checkout")
                .expect("husky location yields a hook file");
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
        HooksLocation::SharedScope {
            configured,
            resolved,
            scope,
        } => {
            eprintln!(
                "warning: core.hooksPath '{configured}' is a {scope}-scope shared hooks dir \
                 ({}); refusing to auto-install portool's hook there -- it would run on every \
                 repository's checkout",
                resolved.display()
            );
            eprintln!(
                "hint: add this line to a per-repo post-checkout (and post-merge) hook instead:"
            );
            eprintln!("hint:   {}", HOOK_APPEND_LINE.trim_end());
            Ok(())
        }
    }
}

/// Runs `portool deinit` (batch D #5): reverses `init` by removing portool's
/// lines from the effective `post-checkout`/`post-merge` hooks and removing
/// `.env.portool` from `.gitignore`. Idempotent and symmetric with `init`.
pub fn deinit() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;
    let loc = HooksLocation::resolve(&ctx);
    for name in ["post-checkout", "post-merge"] {
        if let Some(path) = loc.hook_file(name) {
            deinit_hook(&path)?;
        }
    }
    deinit_gitignore(&ctx.worktree_root)?;
    println!("portool: removed portool's hooks and .gitignore entry");
    Ok(())
}

/// Removes portool's content from one hook: deletes the file if it is
/// portool's own standalone script, otherwise drops just portool's own lines
/// from a foreign hook. Never follows a symlink.
fn deinit_hook(hook_path: &Path) -> Result<()> {
    if fs::symlink_metadata(hook_path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Ok(());
    }
    let existing = match fs::read_to_string(hook_path) {
        Ok(content) => content,
        Err(_) => return Ok(()),
    };

    let trimmed = existing.trim_end_matches('\n');
    if trimmed == HOOK_SCRIPT.trim_end_matches('\n') || OWNED_FULL_SCRIPTS.contains(&trimmed) {
        fs::remove_file(hook_path)?;
        return Ok(());
    }

    let safe_line = HOOK_APPEND_LINE.trim();
    let kept: Vec<&str> = existing
        .lines()
        .filter(|line| {
            let t = line.trim();
            t != safe_line && !UNSAFE_PORTOOL_LINES.contains(&t)
        })
        .collect();
    if kept.len() != existing.lines().count() {
        let mut out = kept.join("\n");
        if existing.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        atomic_write(hook_path, out.as_bytes())?;
    }
    Ok(())
}

fn deinit_gitignore(worktree_root: &Path) -> Result<()> {
    let path = worktree_root.join(".gitignore");
    let existing = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(_) => return Ok(()),
    };
    if !existing.lines().any(|line| line == GITIGNORE_LINE) {
        return Ok(());
    }
    let kept: Vec<&str> = existing.lines().filter(|l| *l != GITIGNORE_LINE).collect();
    let mut out = kept.join("\n");
    if existing.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    fs::write(&path, out)?;
    Ok(())
}

/// Installs (or migrates) portool's `post-checkout` and `post-merge` hooks at
/// a location that is safe to auto-write (batch A #5: post-merge widens
/// passive freshness to `git pull` / `git merge`).
fn install_managed_hooks(loc: &HooksLocation) -> Result<()> {
    for name in ["post-checkout", "post-merge"] {
        if let Some(path) = loc.hook_file(name) {
            install_into(&path)?;
        }
    }
    Ok(())
}

/// Installs into `hook_path`, idempotently and non-destructively:
///
/// - A **symlink** is never followed or modified.
/// - A brand-new hook gets the full safe [`HOOK_SCRIPT`] (mode 0755).
/// - An **already-safe** portool hook is left as-is (only the execute bit is
///   ensured).
/// - An **unsafe** portool form (legacy standalone script, or an unsafe
///   appended line) is **migrated** to the safe form in place.
/// - A foreign **shell** hook (shell shebang, or none) gets the safe line
///   appended; a foreign **non-shell** hook (Python/Node/…) is left untouched
///   with a manual-line hint.
/// - A hook that exists but can't be read as UTF-8 is left entirely alone.
///
/// Rewrites go through a temp-file + rename, and preserve the hook's original
/// permission bits (only ever *adding* the owner-execute bit).
fn install_into(hook_path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(hook_path) {
        if meta.file_type().is_symlink() {
            eprintln!(
                "warning: {} is a symlink; not modifying it",
                hook_path.display()
            );
            return Ok(());
        }
    }

    let hooks_dir = hook_path.parent().ok_or_else(|| {
        Error::General(format!("{} has no parent directory", hook_path.display()))
    })?;
    fs::create_dir_all(hooks_dir)?;

    match fs::read_to_string(hook_path) {
        Ok(existing) => {
            // Already safe: exactly our current script, or it already carries
            // the safe appended line.
            if existing.trim_end_matches('\n') == HOOK_SCRIPT.trim_end_matches('\n')
                || existing.contains(HOOK_APPEND_LINE.trim_end())
            {
                ensure_executable(hook_path)?;
                return Ok(());
            }

            let original_mode = fs::metadata(hook_path)?.permissions().mode();

            // Unsafe portool form present: migrate it in place.
            if let Some(rewritten) = migrate_unsafe_lines(&existing) {
                atomic_write(hook_path, rewritten.as_bytes())?;
                set_mode(hook_path, original_mode | 0o100)?;
                return Ok(());
            }

            // Foreign hook with no portool line: append only if it's shell.
            if shebang_is_posix_shell(&existing) {
                let mut content = existing;
                if !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str(HOOK_APPEND_LINE);
                atomic_write(hook_path, content.as_bytes())?;
                set_mode(hook_path, original_mode | 0o100)?;
            } else {
                eprintln!(
                    "warning: {} has a non-shell interpreter; not appending portool's shell line",
                    hook_path.display()
                );
                eprintln!(
                    "hint: add this line to the hook your interpreter runs, if you want portool:"
                );
                eprintln!("hint:   {}", HOOK_APPEND_LINE.trim_end());
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            atomic_write(hook_path, HOOK_SCRIPT.as_bytes())?;
            set_mode(hook_path, 0o755)?;
        }
        // Exists but not readable as UTF-8: leave it entirely untouched --
        // we can't reason about its content, and must not risk clobbering it
        // or changing its permissions.
        Err(_) => {}
    }

    Ok(())
}

/// Rewrites unsafe portool forms in a hook's content, or returns `None` if
/// there is nothing portool-owned and unsafe to change.
fn migrate_unsafe_lines(existing: &str) -> Option<String> {
    if OWNED_FULL_SCRIPTS.contains(&existing.trim_end_matches('\n')) {
        return Some(HOOK_SCRIPT.to_string());
    }

    let safe_line = HOOK_APPEND_LINE.trim_end_matches('\n');
    let mut changed = false;
    let mut out: Vec<String> = Vec::new();
    for line in existing.lines() {
        if UNSAFE_PORTOOL_LINES.contains(&line.trim()) {
            out.push(safe_line.to_string());
            changed = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !changed {
        return None;
    }

    let mut result = out.join("\n");
    if existing.ends_with('\n') {
        result.push('\n');
    }
    Some(result)
}

/// True when `content` carries a portool invocation in an unsafe form (one
/// that can propagate `sync`'s failure to git). `sync` uses this to hint that
/// `init` should be re-run to upgrade an old hook.
pub(crate) fn contains_unsafe_portool_form(content: &str) -> bool {
    OWNED_FULL_SCRIPTS.contains(&content.trim_end_matches('\n'))
        || content
            .lines()
            .any(|line| UNSAFE_PORTOOL_LINES.contains(&line.trim()))
}

/// Whether it is safe to append a POSIX `sh` line to an existing hook: true
/// when it has no shebang (git runs such hooks via `sh`) or a shell shebang,
/// false for any other interpreter.
fn shebang_is_posix_shell(existing: &str) -> bool {
    match existing.lines().next() {
        None => true,
        Some(first) if first.starts_with("#!") => {
            ["/sh", "/bash", "/dash", "env sh", "env bash", "env dash"]
                .iter()
                .any(|marker| first.contains(marker))
        }
        Some(_) => true,
    }
}

/// Adds the owner-execute bit if missing, preserving every other permission
/// bit (never downgrades e.g. a `0700` hook to `0755`).
fn ensure_executable(path: &Path) -> Result<()> {
    let mode = fs::metadata(path)?.permissions().mode();
    if mode & 0o100 == 0 {
        set_mode(path, mode | 0o100)?;
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(mode);
    fs::set_permissions(path, perms)?;
    Ok(())
}

/// Writes `contents` to `path` atomically (temp file + rename in the same
/// directory).
fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .ok_or_else(|| Error::General(format!("{} has no parent directory", path.display())))?;
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(contents)?;
    tmp.persist(path).map_err(|e| Error::from(e.error))?;
    Ok(())
}

/// Frozen decision 7: appends `.env.portool` to the current worktree root's
/// `.gitignore`, idempotently (no-op if that exact line is already present).
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
    use crate::hooks::HOOK_MARKER;
    use tempfile::TempDir;

    #[test]
    fn install_into_writes_the_safe_script_and_sets_exec_bit() {
        let tmp = TempDir::new().unwrap();
        let hook_path = tmp.path().join("repo/.git/hooks/post-checkout");

        install_into(&hook_path).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content, HOOK_SCRIPT);
        let mode = fs::metadata(&hook_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn install_into_new_hook_exits_zero_and_reports_failure() {
        let tmp = TempDir::new().unwrap();
        let hook_path = tmp.path().join("repo/.git/hooks/post-checkout");

        install_into(&hook_path).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(
            content.trim_end().ends_with("exit 0"),
            "must end with exit 0"
        );
        assert!(
            content.contains("|| echo"),
            "must report sync failure without propagating it"
        );
        assert!(!contains_unsafe_portool_form(&content));
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
    fn install_into_appends_to_an_existing_sh_hook() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        fs::write(&hook_path, "#!/bin/sh\necho hi\n").unwrap();

        install_into(&hook_path).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(content.starts_with("#!/bin/sh\necho hi\n"));
        assert!(content.contains(HOOK_MARKER));
        assert!(
            content.contains("|| true"),
            "appended line must be self-neutralizing"
        );
    }

    #[test]
    fn install_into_skips_a_python_hook() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        let original = "#!/usr/bin/env python3\nprint('existing hook')\n";
        fs::write(&hook_path, original).unwrap();

        install_into(&hook_path).unwrap();

        // Left byte-identical: portool never injects sh into a python hook.
        assert_eq!(fs::read_to_string(&hook_path).unwrap(), original);
    }

    #[test]
    fn install_into_rejects_a_symlink() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let target = hooks_dir.join("real-hook");
        fs::write(&target, "#!/bin/sh\necho hi\n").unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        std::os::unix::fs::symlink(&target, &hook_path).unwrap();

        install_into(&hook_path).unwrap();

        // The symlink and its target are untouched.
        assert!(fs::symlink_metadata(&hook_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "#!/bin/sh\necho hi\n");
    }

    #[test]
    fn install_into_preserves_restrictive_perms() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        fs::write(&hook_path, "#!/bin/sh\necho hi\n").unwrap();
        set_mode(&hook_path, 0o700).unwrap();

        install_into(&hook_path).unwrap();

        // Appended-to, but 0700 is preserved (not widened to 0755).
        assert!(fs::read_to_string(&hook_path)
            .unwrap()
            .contains(HOOK_MARKER));
        assert_eq!(
            fs::metadata(&hook_path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn install_into_migrates_legacy_standalone_hook() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        let legacy = "#!/bin/sh\n# installed by portool\ncommand -v portool >/dev/null 2>&1 && portool sync --quiet\n";
        fs::write(&hook_path, legacy).unwrap();

        install_into(&hook_path).unwrap();

        // The unsafe legacy script is now the current safe script.
        assert_eq!(fs::read_to_string(&hook_path).unwrap(), HOOK_SCRIPT);
    }

    #[test]
    fn install_into_migrates_unsafe_line_in_foreign_hook() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        let unsafe_hook = "#!/bin/sh\necho hi\nif command -v portool >/dev/null 2>&1; then portool sync --quiet; fi\n";
        fs::write(&hook_path, unsafe_hook).unwrap();

        install_into(&hook_path).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(
            content.starts_with("#!/bin/sh\necho hi\n"),
            "foreign lines preserved"
        );
        assert!(
            content.contains("|| true"),
            "portool line migrated to safe form"
        );
        assert!(!contains_unsafe_portool_form(&content));
    }

    #[test]
    fn install_into_leaves_user_written_portool_mention_untouched() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        // A user's own comment mentioning portool sync -- NOT an emitted form.
        let user_hook = "#!/bin/sh\n# remember to run portool sync manually\necho hi\n";
        fs::write(&hook_path, user_hook).unwrap();

        install_into(&hook_path).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(content.contains("# remember to run portool sync manually"));
        // The comment is left; portool appends its own guarded line below it.
        assert!(content.contains("|| true"));
    }

    #[test]
    fn install_managed_hooks_installs_post_checkout_and_post_merge() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        let loc = HooksLocation::GitDefault {
            hooks_dir: hooks_dir.clone(),
        };

        install_managed_hooks(&loc).unwrap();

        assert_eq!(
            fs::read_to_string(hooks_dir.join("post-checkout")).unwrap(),
            HOOK_SCRIPT
        );
        assert_eq!(
            fs::read_to_string(hooks_dir.join("post-merge")).unwrap(),
            HOOK_SCRIPT
        );
    }

    #[test]
    fn contains_unsafe_portool_form_recognizes_old_but_not_new() {
        let legacy = "#!/bin/sh\n# installed by portool\ncommand -v portool >/dev/null 2>&1 && portool sync --quiet\n";
        assert!(contains_unsafe_portool_form(legacy));
        assert!(contains_unsafe_portool_form(
            "#!/bin/sh\necho hi\nif command -v portool >/dev/null 2>&1; then portool sync --quiet; fi\n"
        ));
        assert!(!contains_unsafe_portool_form(HOOK_SCRIPT));
        assert!(!contains_unsafe_portool_form(
            "#!/bin/sh\n# remember to run portool sync manually\n"
        ));
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
