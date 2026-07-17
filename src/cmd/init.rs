//! `portool init` (spec §9.1, frozen decisions 2, 6, 7, 8; hardening batch A;
//! Task 6): installs the `post-checkout` and `post-merge` hooks into the
//! repository's *effective* hooks location (honoring `core.hooksPath` /
//! Husky, and refusing shared global/system scope -- see `crate::hooks`),
//! appends `.env.portool` to `$GIT_COMMON_DIR/info/exclude` (never the
//! tracked `.gitignore`), and runs `sync` once. The installed hooks always
//! exit 0, so a portool failure never fails the caller's git command.
//!
//! `portool unhook` and `portool deinit` reverse this: `unhook` removes just
//! the hooks, `deinit` also releases the project's ledger allocations, its
//! env files, and the `info/exclude` entry.

use crate::cmd::sync;
use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::hooks::HooksLocation;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The single line appended to an existing (foreign) hook by portool <= 0.5.
/// `|| true` means portool's invocation -- the last line of the hook -- can
/// never become the hook's failing exit status. Superseded by the managed
/// block ([`hook_append_block`]), but still recognized so a legacy line is
/// migrated (Step 3) and still hinted in messages where a per-repo one-liner
/// is simplest to print.
const HOOK_APPEND_LINE: &str =
    "if command -v portool >/dev/null 2>&1; then portool sync --quiet || true; fi\n";

/// Whole-file portool scripts from earlier versions that propagate `sync`'s
/// failure (no `exit 0` / `|| true`). Matched after trimming a single
/// trailing newline. Only used by [`contains_unsafe_portool_form`] now --
/// `install_into`/`deinit_hook` recognize *any* owned standalone script via
/// [`is_owned_standalone`] (the second-line marker), which subsumes this.
const LEGACY_UNSAFE_STANDALONE_SCRIPTS: &[&str] = &[
    // portool <= 0.2
    "#!/bin/sh\n# installed by portool\ncommand -v portool >/dev/null 2>&1 && portool sync --quiet",
    // portool 0.3 / 0.4 (unsafe: no exit 0)
    "#!/bin/sh\n# installed by portool\nif command -v portool >/dev/null 2>&1; then\n  portool sync --quiet\nfi",
];

/// Single unsafe portool lines that may sit inside a *foreign* hook. Matched
/// on the trimmed line; a match is migrated to the managed block
/// ([`hook_append_block`]). These are the exact shapes portool itself
/// emitted -- a line a user merely wrote by hand that happens to mention
/// `portool sync` is never one of them, so it is left untouched (batch A #2,
/// Fable review).
const UNSAFE_PORTOOL_LINES: &[&str] = &[
    "command -v portool >/dev/null 2>&1 && portool sync --quiet",
    "if command -v portool >/dev/null 2>&1; then portool sync --quiet; fi",
];

const EXCLUDE_COMMENT: &str = "# managed by portool";
const IGNORE_LINE: &str = ".env.portool";

/// What happened to one hook file, returned by [`install_into`] and
/// [`deinit_hook`] so callers can report accurately instead of assuming
/// success (external review: `init`/`unhook`/`deinit` used to exit 0 even
/// when a symlink, non-shell hook, or malformed managed block meant nothing
/// was actually written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookOutcome {
    /// The hook was written (fresh install, refresh, migration, or insert).
    Installed,
    /// The hook already had portool's current content; nothing was written.
    AlreadyCurrent,
    /// Portool's content was removed from the hook (`deinit_hook` only).
    Removed,
    /// The hook had no portool content to remove (`deinit_hook` only).
    NotFound,
    /// The hook path is a symlink; never followed or modified.
    SkippedSymlink,
    /// Nothing was written because the hook needs a human to look at it
    /// (non-shell interpreter, unreadable content, or a malformed managed
    /// block). `reason` is safe to show the user as-is.
    ManualRequired { reason: String },
}

/// The three shapes a hook's `# >>> portool >>> ... # <<< portool <<<`
/// markers can be in, from a full line-exact scan of the file:
///
/// - Exactly one begin, one end, begin before end -> [`Self::Valid`].
/// - No markers at all -> [`Self::Absent`].
/// - Everything else -- an end with no begin, more than one of either,
///   or a begin that comes after its end -- is [`Self::Malformed`]. This is
///   deliberately fail-closed: a truncated block used to be treated as
///   extending to EOF and owned wholesale by `install_into`/`deinit_hook`,
///   which could delete a user's own code appended after a portool block
///   whose end marker was accidentally deleted.
pub(crate) enum ManagedBlockState {
    Absent,
    /// Line indices into `content.lines()`, inclusive.
    Valid {
        begin: usize,
        end: usize,
    },
    Malformed,
}

/// Scans `content` line-exactly (matching how [`replace_managed_block`]
/// slices) for the begin/end markers and classifies the result. See
/// [`ManagedBlockState`] for the exact rule.
///
/// `pub(crate)`: reused by `doctor`'s hook-health report (Task 3) to detect
/// both a malformed layout and an unreachable-but-valid block (one sitting
/// after a top-level `exit`/`exec` left by portool <= 0.8's EOF-append
/// insertion).
pub(crate) fn managed_block_state(content: &str) -> ManagedBlockState {
    let mut begins = Vec::new();
    let mut ends = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let t = line.trim();
        if t == crate::hooks::HOOK_BLOCK_BEGIN {
            begins.push(i);
        } else if t == crate::hooks::HOOK_BLOCK_END {
            ends.push(i);
        }
    }
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => ManagedBlockState::Absent,
        (&[begin], &[end]) if begin < end => ManagedBlockState::Valid { begin, end },
        _ => ManagedBlockState::Malformed,
    }
}

fn exclude_path(common_dir: &Path) -> PathBuf {
    common_dir.join("info").join("exclude")
}

/// The absolute path of the running `portool` binary, canonicalized, for
/// embedding in the hook script (so it works even when the process that
/// runs the hook -- e.g. a GUI git client -- has a PATH that doesn't include
/// wherever `portool` was installed; external review P1-4). Falls back to
/// `None` (meaning: look up `portool` on PATH at hook-run time) when the
/// path can't be embedded: `current_exe()` failed, or the path contains a
/// character that isn't [`sh_safe_in_double_quotes`].
fn portool_bin_path() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let s = exe.to_str()?.to_string();
    if !sh_safe_in_double_quotes(&s) {
        return None;
    }
    Some(s)
}

/// True when `s` can be interpolated inside double quotes in a POSIX `sh`
/// script without escaping them or triggering an expansion. Rejects `"`,
/// `\`, `$`, and a backtick (all of which stay active *inside* double
/// quotes -- backticks perform command substitution there), plus `'` and
/// newline out of caution.
fn sh_safe_in_double_quotes(s: &str) -> bool {
    !s.chars()
        .any(|c| matches!(c, '"' | '\'' | '\\' | '$' | '`' | '\n'))
}

/// The `PORTOOL_BIN=...` preamble shared by both hook forms: the absolute
/// path recorded at init time, falling back to a plain PATH lookup when
/// `bin` is `None` or the recorded binary no longer exists at that path
/// (e.g. it moved -- `[ -x ... ]` fails and PATH is tried instead).
fn bin_preamble(bin: Option<&str>) -> String {
    match bin {
        Some(path) => format!(
            "PORTOOL_BIN=\"{path}\"\nif ! [ -x \"$PORTOOL_BIN\" ]; then PORTOOL_BIN=portool; fi\n"
        ),
        None => "PORTOOL_BIN=portool\n".to_string(),
    }
}

/// The full script written for portool's standalone hook. The `command -v`
/// guard makes the hook a no-op (exit 0) when portool isn't resolvable; the
/// `|| echo … >&2` and trailing `exit 0` make it exit 0 *even when portool
/// is installed and `sync` fails*, so a portool problem can never turn
/// `git checkout` / `git worktree add` into a failure (batch A #1).
fn hook_script(bin: Option<&str>) -> String {
    format!(
        "#!/bin/sh\n{}\n{}if command -v \"$PORTOOL_BIN\" >/dev/null 2>&1; then\n\
         \x20\x20\"$PORTOOL_BIN\" sync --quiet || echo 'portool: sync failed; Git was not blocked' >&2\n\
         fi\nexit 0\n",
        crate::hooks::HOOK_OWNED_COMMENT,
        bin_preamble(bin),
    )
}

/// The managed block appended to (and refreshed within) a foreign hook.
/// `|| true` means portool's invocation can never become the hook's failing
/// exit status.
fn hook_append_block(bin: Option<&str>) -> String {
    format!(
        "{}\n{}if command -v \"$PORTOOL_BIN\" >/dev/null 2>&1; then \"$PORTOOL_BIN\" sync --quiet || true; fi\n{}\n",
        crate::hooks::HOOK_BLOCK_BEGIN,
        bin_preamble(bin),
        crate::hooks::HOOK_BLOCK_END,
    )
}

/// Runs `portool init`. With neither flag, installs the hooks, updates
/// `$GIT_COMMON_DIR/info/exclude`, and runs `sync`; `--hook-only`/
/// `--gitignore-only` (clap enforces they're mutually exclusive) each perform
/// just their one step. `--gitignore-only` keeps its pre-1.0 name even though
/// it now touches `info/exclude`, not the tracked `.gitignore`.
pub fn run(hook_only: bool, gitignore_only: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;

    if hook_only {
        let outcome = install_hook(&ctx)?;
        return finish_hook_install(outcome);
    }
    if gitignore_only {
        return update_exclude(&ctx.common_dir);
    }

    let outcome = install_hook(&ctx)?;
    update_exclude(&ctx.common_dir)?;
    sync::run(false)?;
    finish_hook_install(outcome)
}

/// Turns the post-checkout hook's install outcome into `init`'s exit status.
/// `Installed`/`AlreadyCurrent` mean git will actually run the hook, so
/// `init` succeeds; anything else (a symlink, a non-shell interpreter, or
/// unreadable/malformed content) means nothing was wired up, and `init` must
/// say so with a non-zero exit -- even though (for full `init`) the exclude
/// update and `sync` already ran (external review: `init` used to exit 0
/// while silently installing nothing).
fn finish_hook_install(outcome: HookOutcome) -> Result<()> {
    if matches!(
        outcome,
        HookOutcome::Installed | HookOutcome::AlreadyCurrent
    ) {
        return Ok(());
    }
    let reason = match outcome {
        HookOutcome::SkippedSymlink => {
            "the post-checkout hook is a symlink and was not modified".to_string()
        }
        HookOutcome::ManualRequired { reason } => reason,
        other => format!("unexpected hook outcome {other:?}"),
    };
    Err(Error::General(format!(
        "the post-checkout hook was not installed: {reason}; see the warnings above"
    )))
}

/// Installs portool's hooks where git will actually run them. When that's
/// nowhere safe -- a `core.hooksPath` dir that doesn't exist / isn't Husky's
/// (`Missing`), or one that resolves outside the repository regardless of
/// scope (`SharedScope`) -- it warns with the manual line instead of
/// writing.
fn install_hook(ctx: &GitCtx) -> Result<HookOutcome> {
    let loc = HooksLocation::resolve(ctx);
    match &loc {
        HooksLocation::GitDefault { .. } | HooksLocation::Custom { .. } => {
            install_managed_hooks(&loc)
        }
        HooksLocation::Husky { .. } => {
            let outcome = install_managed_hooks(&loc)?;
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
            Ok(outcome)
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
            Err(Error::General(
                "no hook was installed; see the hints above".to_string(),
            ))
        }
        HooksLocation::SharedScope {
            configured,
            resolved,
            scope,
        } => {
            eprintln!(
                "warning: core.hooksPath '{configured}' resolves outside this repository \
                 ({}, {scope} scope); refusing to auto-install portool's hook there -- it \
                 could run on every repository's checkout",
                resolved.display()
            );
            eprintln!(
                "hint: add this line to a per-repo post-checkout (and post-merge) hook instead:"
            );
            eprintln!("hint:   {}", HOOK_APPEND_LINE.trim_end());
            Err(Error::General(
                "no hook was installed; see the hints above".to_string(),
            ))
        }
    }
}

/// Runs `portool unhook`: removes portool's content from the effective
/// post-checkout/post-merge hooks and nothing else.
pub fn unhook() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;
    let results = remove_hooks(&ctx)?;
    report_hook_removal(&results);
    Ok(())
}

/// Removes portool's content from the effective `post-checkout`/`post-merge`
/// hooks, returning each hook's outcome so `unhook`/`deinit` can report an
/// accurate summary instead of an unconditional "removed" (external review).
/// An unreadable (non-`NotFound`) hook propagates as `Err`, which makes
/// `unhook`/`deinit` exit non-zero via `?` instead of silently doing nothing.
fn remove_hooks(ctx: &GitCtx) -> Result<Vec<(&'static str, HookOutcome)>> {
    let loc = HooksLocation::resolve(ctx);
    let mut results = Vec::new();
    for name in ["post-checkout", "post-merge"] {
        if let Some(path) = loc.hook_file(name) {
            results.push((name, deinit_hook(&path)?));
        }
    }
    Ok(results)
}

/// Prints a summary of `results` (from [`remove_hooks`]) that matches what
/// actually happened: "removed" only when at least one hook's content was
/// actually removed, "no portool hooks found" when every hook had nothing of
/// portool's to remove, plus a warning per hook that needs manual attention.
fn report_hook_removal(results: &[(&'static str, HookOutcome)]) {
    if results
        .iter()
        .any(|(_, outcome)| matches!(outcome, HookOutcome::Removed))
    {
        println!("portool: removed portool's hooks");
    } else {
        println!("portool: no portool hooks found");
    }
    for (name, outcome) in results {
        match outcome {
            HookOutcome::SkippedSymlink => {
                eprintln!("warning: {name} hook is a symlink; not modified");
            }
            HookOutcome::ManualRequired { reason } => {
                eprintln!("warning: {name} hook needs manual attention: {reason}");
            }
            HookOutcome::Removed | HookOutcome::NotFound => {}
            // `deinit_hook` never produces these -- they're install-only.
            HookOutcome::AlreadyCurrent | HookOutcome::Installed => {}
        }
    }
}

/// Runs `portool deinit`: releases every allocation of the current project,
/// removes generated env files, hooks, and the `info/exclude` entry. The
/// tracked `.gitignore` is never edited (ownership of a bare line there is
/// unknowable) -- a leftover line only earns a hint.
pub fn deinit(keep_allocations: bool) -> Result<()> {
    use crate::{lock, paths, store};
    use std::time::Duration;

    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;
    let common_dir_key = ctx.common_dir.to_string_lossy().into_owned();

    if !keep_allocations {
        let _lock = lock::acquire(&paths::lock_path()?, Duration::from_secs(10))?;
        let registry_path = paths::registry_path()?;
        if let Some(mut registry) = store::load_strict(&registry_path)? {
            if let Some(project) = registry.find_project(&common_dir_key) {
                // Env files first, ledger second (same ordering rationale as
                // `release`): a failed removal keeps the block reserved.
                for path in project.worktrees.keys() {
                    let env_path = Path::new(path).join(".env.portool");
                    match fs::remove_file(&env_path) {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => {
                            return Err(Error::General(format!(
                                "failed to remove {}: {err}; no allocations were released",
                                env_path.display()
                            )))
                        }
                    }
                }
                registry.projects.remove(&common_dir_key);
                store::save(&registry_path, &registry)?;
                println!("portool: released all of this project's allocations");
            }
        }
    }

    let hook_results = remove_hooks(&ctx)?;
    report_hook_removal(&hook_results);
    deinit_exclude(&ctx.common_dir)?;

    let gitignore = ctx.worktree_root.join(".gitignore");
    if fs::read_to_string(&gitignore)
        .map(|c| c.lines().any(|l| l == IGNORE_LINE))
        .unwrap_or(false)
    {
        println!(
            "portool: note: {} still lists '.env.portool' (added by an older portool \
             or by hand); remove it yourself if unwanted -- portool no longer edits \
             tracked files",
            gitignore.display()
        );
    }
    println!("portool: deinitialized this project");
    Ok(())
}

/// Removes portool's content from one hook. Shared by `unhook` and `deinit`
/// via `remove_hooks`:
///
/// - An **owned standalone** script (any shape -- current or legacy,
///   identified by [`is_owned_standalone`]) is deleted outright -> `Removed`.
/// - A **valid managed block** in a foreign hook has just its block lines
///   removed -> `Removed`. A **malformed** one (mismatched/duplicate/
///   reversed markers) is left entirely untouched -> `ManualRequired`
///   (external review: a truncated block used to be treated as extending to
///   EOF and deleted wholesale, which could delete a user's own code).
/// - **Legacy appended lines** (safe or unsafe) are dropped individually ->
///   `Removed` when any were found, `NotFound` otherwise.
///
/// Never follows a symlink (`SkippedSymlink`). A read error other than
/// "file doesn't exist" (`NotFound`) propagates as `Err` -- previously it
/// was swallowed into a silent `Ok(())`, so `unhook`/`deinit` looked
/// successful while never touching the hook.
fn deinit_hook(hook_path: &Path) -> Result<HookOutcome> {
    if fs::symlink_metadata(hook_path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Ok(HookOutcome::SkippedSymlink);
    }
    let existing = match fs::read_to_string(hook_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HookOutcome::NotFound),
        Err(err) => return Err(Error::from(err)),
    };

    if is_owned_standalone(&existing) {
        fs::remove_file(hook_path)?;
        return Ok(HookOutcome::Removed);
    }

    match managed_block_state(&existing) {
        ManagedBlockState::Malformed => {
            eprintln!(
                "warning: {} has a malformed portool managed block (mismatched, duplicate, \
                 or reversed '# >>> portool >>>' / '# <<< portool <<<' markers); leaving it \
                 untouched -- repair or remove the markers by hand",
                hook_path.display()
            );
            return Ok(HookOutcome::ManualRequired {
                reason: format!(
                    "{} has a malformed managed block and needs manual repair",
                    hook_path.display()
                ),
            });
        }
        ManagedBlockState::Valid { begin, end } => {
            if let Some(stripped) = replace_managed_block(&existing, begin, end, "") {
                atomic_write(hook_path, stripped.as_bytes())?;
            }
            return Ok(HookOutcome::Removed);
        }
        ManagedBlockState::Absent => {}
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
        Ok(HookOutcome::Removed)
    } else {
        Ok(HookOutcome::NotFound)
    }
}

/// Removes exactly the pair `update_exclude` wrote (comment + following
/// `.env.portool` line). A bare user-added line is left alone.
fn deinit_exclude(common_dir: &Path) -> Result<()> {
    let path = exclude_path(common_dir);
    let existing = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(Error::from(err)),
    };
    let mut out: Vec<&str> = Vec::new();
    let mut lines = existing.lines().peekable();
    let mut changed = false;
    while let Some(line) = lines.next() {
        if line.trim() == EXCLUDE_COMMENT && lines.peek().map(|l| l.trim()) == Some(IGNORE_LINE) {
            lines.next();
            changed = true;
            continue;
        }
        out.push(line);
    }
    if changed {
        let mut content = out.join("\n");
        if existing.ends_with('\n') && !content.is_empty() {
            content.push('\n');
        }
        atomic_write(&path, content.as_bytes())?;
    }
    Ok(())
}

/// Installs (or migrates) portool's `post-checkout` and `post-merge` hooks at
/// a location that is safe to auto-write (batch A #5: post-merge widens
/// passive freshness to `git pull` / `git merge`). Computes the absolute
/// binary path, and both hook forms derived from it, once up front so both
/// hooks embed the same content. Returns **post-checkout's** outcome --
/// that's the hook `run`/`init --hook-only` gate their exit status on, since
/// it's the one every `git checkout`/`worktree add` actually fires.
fn install_managed_hooks(loc: &HooksLocation) -> Result<HookOutcome> {
    let bin = portool_bin_path();
    let script = hook_script(bin.as_deref());
    let block = hook_append_block(bin.as_deref());
    let mut post_checkout_outcome = None;
    for name in ["post-checkout", "post-merge"] {
        if let Some(path) = loc.hook_file(name) {
            let outcome = install_into(&path, &script, &block)?;
            if name == "post-checkout" {
                post_checkout_outcome = Some(outcome);
            }
        }
    }
    Ok(post_checkout_outcome
        .expect("a resolvable HooksLocation always yields a post-checkout hook_file"))
}

/// Installs into `hook_path`, idempotently and non-destructively:
///
/// - A **symlink** is never followed or modified -> `SkippedSymlink`.
/// - A brand-new hook gets the full standalone `script` (mode 0755) ->
///   `Installed`.
/// - An **owned standalone** hook (any shape, current or legacy -- see
///   [`is_owned_standalone`]) is rewritten wholesale when its content is
///   stale (this is how re-running `init` refreshes a moved binary path) ->
///   `Installed`; left as-is (only the execute bit is ensured) when already
///   current -> `AlreadyCurrent`.
/// - A foreign hook with a **valid managed block** has just the block
///   refreshed in place (never relocated) when stale -> `Installed`, or left
///   as-is -> `AlreadyCurrent`. A **malformed** block (mismatched, duplicate,
///   or reversed markers) is left entirely untouched -> `ManualRequired`
///   (external review: a truncated block used to be treated as extending to
///   EOF, so re-running `init` could silently rewrite/delete a user's own
///   code after it).
/// - A foreign hook with a **legacy appended line** (safe or unsafe) has it
///   migrated to a managed `block` -> `Installed`.
/// - A foreign **shell** hook with no portool content gets `block` inserted
///   right after the shebang (or at the top if there is none) -> `Installed`
///   -- never appended at EOF, where a pre-existing top-level `exit`/`exec`
///   could make it unreachable (external review).
/// - A foreign **non-shell** hook (Python/Node/…) is left untouched with a
///   manual-line hint -> `ManualRequired`.
/// - A hook that exists but can't be read (non-UTF-8, permissions, ...) is
///   left entirely alone -> `ManualRequired` (previously silently `Ok(())`,
///   so `init` looked successful while installing nothing; external review).
///
/// Rewrites go through a temp-file + rename, and preserve the hook's original
/// permission bits (only ever *adding* the owner-execute bit).
fn install_into(hook_path: &Path, script: &str, block: &str) -> Result<HookOutcome> {
    if let Ok(meta) = fs::symlink_metadata(hook_path) {
        if meta.file_type().is_symlink() {
            eprintln!(
                "warning: {} is a symlink; not modifying it",
                hook_path.display()
            );
            return Ok(HookOutcome::SkippedSymlink);
        }
    }

    let hooks_dir = hook_path.parent().ok_or_else(|| {
        Error::General(format!("{} has no parent directory", hook_path.display()))
    })?;
    fs::create_dir_all(hooks_dir)?;

    match fs::read_to_string(hook_path) {
        Ok(existing) => {
            // 1. Owned standalone script: second line is the ownership
            //    marker. Rewrite when stale (legacy shape or moved binary).
            if is_owned_standalone(&existing) {
                return if existing != script {
                    let original_mode = fs::metadata(hook_path)?.permissions().mode();
                    atomic_write(hook_path, script.as_bytes())?;
                    set_mode(hook_path, original_mode | 0o100)?;
                    Ok(HookOutcome::Installed)
                } else {
                    ensure_executable(hook_path)?;
                    Ok(HookOutcome::AlreadyCurrent)
                };
            }

            let original_mode = fs::metadata(hook_path)?.permissions().mode();

            // 2. Managed block: fail closed on anything malformed, refresh a
            //    valid one in place when stale.
            match managed_block_state(&existing) {
                ManagedBlockState::Malformed => {
                    eprintln!(
                        "warning: {} has a malformed portool managed block (mismatched, \
                         duplicate, or reversed '# >>> portool >>>' / '# <<< portool <<<' \
                         markers); leaving it untouched -- repair or remove the markers by \
                         hand, then re-run 'portool init --hook-only'",
                        hook_path.display()
                    );
                    return Ok(HookOutcome::ManualRequired {
                        reason: format!(
                            "{} has a malformed managed block and needs manual repair",
                            hook_path.display()
                        ),
                    });
                }
                ManagedBlockState::Valid { begin, end } => {
                    return if let Some(rewritten) =
                        replace_managed_block(&existing, begin, end, block)
                    {
                        atomic_write(hook_path, rewritten.as_bytes())?;
                        set_mode(hook_path, original_mode | 0o100)?;
                        Ok(HookOutcome::Installed)
                    } else {
                        ensure_executable(hook_path)?;
                        Ok(HookOutcome::AlreadyCurrent)
                    };
                }
                ManagedBlockState::Absent => {}
            }

            // 3. Legacy appended lines (safe or unsafe): migrate to the block.
            if let Some(rewritten) = migrate_legacy_lines(&existing, block) {
                atomic_write(hook_path, rewritten.as_bytes())?;
                set_mode(hook_path, original_mode | 0o100)?;
                return Ok(HookOutcome::Installed);
            }

            // 4. Foreign hook, no portool content: insert right after the
            //    shebang (never append at EOF -- see `insert_block_after_shebang`).
            if shebang_is_posix_shell(&existing) {
                let content = insert_block_after_shebang(&existing, block);
                atomic_write(hook_path, content.as_bytes())?;
                set_mode(hook_path, original_mode | 0o100)?;
                Ok(HookOutcome::Installed)
            } else {
                eprintln!(
                    "warning: {} has a non-shell interpreter; not appending portool's shell block",
                    hook_path.display()
                );
                eprintln!(
                    "hint: add this line to the hook your interpreter runs, if you want portool:"
                );
                eprintln!("hint:   {}", HOOK_APPEND_LINE.trim_end());
                Ok(HookOutcome::ManualRequired {
                    reason: format!("{} has a non-shell interpreter", hook_path.display()),
                })
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            atomic_write(hook_path, script.as_bytes())?;
            set_mode(hook_path, 0o755)?;
            Ok(HookOutcome::Installed)
        }
        // Exists but not readable (non-UTF-8, permissions, ...): leave it
        // entirely untouched -- we can't reason about its content, and must
        // not risk clobbering it or changing its permissions. Still warn, so
        // this doesn't look like a silent success (doctor flags it later
        // too).
        Err(_) => {
            eprintln!(
                "warning: {} exists but is not readable as UTF-8; leaving it untouched \
                 (portool's hook was NOT installed)",
                hook_path.display()
            );
            Ok(HookOutcome::ManualRequired {
                reason: format!("{} is not readable as UTF-8", hook_path.display()),
            })
        }
    }
}

/// Inserts `block` right after the shebang line (or at the very top when
/// there is none), so a pre-existing top-level `exit`/`exec` further down
/// cannot make the block unreachable.
fn insert_block_after_shebang(existing: &str, block: &str) -> String {
    let (head, rest) = if existing.starts_with("#!") {
        match existing.find('\n') {
            Some(i) => existing.split_at(i + 1),
            None => (existing, ""),
        }
    } else {
        ("", existing)
    };
    let mut out = String::with_capacity(existing.len() + block.len() + 1);
    out.push_str(head);
    if !head.is_empty() && !head.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(block);
    out.push_str(rest);
    out
}

/// True when `content` is portool's own standalone hook script: its second
/// line (trimmed) is the ownership marker. This covers every shape portool
/// has ever emitted for a standalone script (old and new), since that
/// comment has been the second line since the very first release.
fn is_owned_standalone(content: &str) -> bool {
    content.lines().nth(1).map(str::trim) == Some(crate::hooks::HOOK_OWNED_COMMENT)
}

/// Replaces the `# >>> portool >>> ... # <<< portool <<<` region (inclusive,
/// `begin`/`end` from a [`ManagedBlockState::Valid`]) with `block`. Returns
/// `None` when the existing region already equals `block`. Only ever called
/// with a `Valid` state's indices -- a `Malformed` layout is never rewritten
/// (see [`managed_block_state`]).
fn replace_managed_block(existing: &str, begin: usize, end: usize, block: &str) -> Option<String> {
    let lines: Vec<&str> = existing.lines().collect();

    let current_region = format!("{}\n", lines[begin..=end].join("\n"));
    if current_region == block {
        return None;
    }

    let mut out = String::new();
    for line in &lines[..begin] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(block);
    for line in &lines[end + 1..] {
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

/// Replaces the first legacy appended portool line (safe [`HOOK_APPEND_LINE`]
/// or any [`UNSAFE_PORTOOL_LINES`] form) with `block`, dropping any further
/// legacy occurrences. `None` when there is no legacy line.
fn migrate_legacy_lines(existing: &str, block: &str) -> Option<String> {
    let safe_line = HOOK_APPEND_LINE.trim();
    let mut replaced = false;
    let mut out: Vec<String> = Vec::new();
    for line in existing.lines() {
        let t = line.trim();
        if t == safe_line || UNSAFE_PORTOOL_LINES.contains(&t) {
            if !replaced {
                out.push(block.trim_end_matches('\n').to_string());
                replaced = true;
            }
            // Further legacy occurrences are dropped, not duplicated.
        } else {
            out.push(line.to_string());
        }
    }
    if !replaced {
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
    LEGACY_UNSAFE_STANDALONE_SCRIPTS.contains(&content.trim_end_matches('\n'))
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

/// Adds the managed `.env.portool` pair to `$GIT_COMMON_DIR/info/exclude`
/// (external review P0-3): repo-specific but never committed, and shared by
/// every linked worktree, unlike the old tracked-`.gitignore` edit (frozen
/// decision 7, superseded by Task 6). Idempotent: a no-op when any line is
/// already exactly `.env.portool`.
fn update_exclude(common_dir: &Path) -> Result<()> {
    let path = exclude_path(common_dir);
    let existing = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(Error::from(err)),
    };
    if existing.lines().any(|line| line.trim() == IGNORE_LINE) {
        return Ok(());
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(EXCLUDE_COMMENT);
    content.push('\n');
    content.push_str(IGNORE_LINE);
    content.push('\n');
    atomic_write(&path, content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn hook_script_embeds_an_absolute_path_with_fallback() {
        let script = hook_script(Some("/opt/portool/bin/portool"));
        assert!(script.starts_with("#!/bin/sh\n# installed by portool\n"));
        assert!(script.contains("PORTOOL_BIN=\"/opt/portool/bin/portool\""));
        assert!(
            script.contains("PORTOOL_BIN=portool"),
            "must fall back to PATH"
        );
        assert!(script.trim_end().ends_with("exit 0"));
    }

    #[test]
    fn install_into_refreshes_an_owned_script_with_a_stale_path() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        fs::write(&hook_path, hook_script(Some("/stale/old/portool"))).unwrap();

        install_into(
            &hook_path,
            &hook_script(Some("/new/portool")),
            &hook_append_block(Some("/new/portool")),
        )
        .unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(content.contains("/new/portool"));
        assert!(!content.contains("/stale/old/portool"));
    }

    #[test]
    fn install_into_inserts_a_managed_block_after_the_shebang_of_a_foreign_hook() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        fs::write(&hook_path, "#!/bin/sh\necho hi\n").unwrap();
        let block = hook_append_block(Some("/p"));

        let outcome = install_into(&hook_path, &hook_script(Some("/p")), &block).unwrap();

        assert_eq!(outcome, HookOutcome::Installed);
        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(
            content.starts_with("#!/bin/sh\n# >>> portool >>>\n"),
            "block must sit right after the shebang, not at EOF, got: {content}"
        );
        assert!(content.ends_with("echo hi\n"));
        assert!(content.contains(crate::hooks::HOOK_BLOCK_BEGIN));
        assert!(content.contains(crate::hooks::HOOK_BLOCK_END));
        assert!(content.contains("|| true"));
    }

    #[test]
    fn install_into_migrates_a_legacy_appended_line_to_the_block() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        fs::write(
            &hook_path,
            "#!/bin/sh\necho hi\nif command -v portool >/dev/null 2>&1; then portool sync --quiet || true; fi\n",
        )
        .unwrap();

        install_into(
            &hook_path,
            &hook_script(Some("/p")),
            &hook_append_block(Some("/p")),
        )
        .unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(content.contains(crate::hooks::HOOK_BLOCK_BEGIN));
        assert!(
            !content.contains(
                "if command -v portool >/dev/null 2>&1; then portool sync --quiet || true; fi"
            ),
            "legacy line must be replaced, not duplicated"
        );
    }

    #[test]
    fn sh_safe_in_double_quotes_rejects_every_active_character() {
        for unsafe_path in [
            "/opt/port`whoami`ool/portool", // backtick: command substitution INSIDE double quotes
            "/opt/$HOME/portool",
            "/opt/po\"rtool",
            "/opt/po'rtool",
            "/opt/po\\rtool",
            "/opt/po\nrtool",
        ] {
            assert!(
                !sh_safe_in_double_quotes(unsafe_path),
                "must reject: {unsafe_path:?}"
            );
        }
        assert!(sh_safe_in_double_quotes("/opt portool/bin/portool (v2)"));
    }

    #[test]
    fn install_into_treats_a_mid_line_marker_mention_as_a_foreign_hook() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        // The marker text appears only mid-line (not as its own trimmed
        // line) -- NOT a managed block; install must still wire the hook.
        fs::write(
            &hook_path,
            "#!/bin/sh\necho \"not a marker: # >>> portool >>>\"\n",
        )
        .unwrap();
        let block = hook_append_block(Some("/p"));

        install_into(&hook_path, &hook_script(Some("/p")), &block).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(
            content.starts_with("#!/bin/sh\n# >>> portool >>>"),
            "a real managed block must be inserted after the shebang, got: {content}"
        );
        assert!(
            content.ends_with("echo \"not a marker: # >>> portool >>>\"\n"),
            "the foreign line must be preserved, got: {content}"
        );
    }

    #[test]
    fn migrate_legacy_lines_replaces_first_and_drops_further_occurrences() {
        let existing = "#!/bin/sh\necho hi\n\
            if command -v portool >/dev/null 2>&1; then portool sync --quiet || true; fi\n\
            echo again\n\
            if command -v portool >/dev/null 2>&1; then portool sync --quiet || true; fi\n";
        let block = hook_append_block(Some("/p"));

        let rewritten = migrate_legacy_lines(existing, &block).expect("legacy line present");

        assert_eq!(
            rewritten.matches(crate::hooks::HOOK_BLOCK_BEGIN).count(),
            1,
            "only one block must be inserted"
        );
        assert_eq!(
            rewritten.matches("if command -v portool").count(),
            0,
            "every legacy line must be gone"
        );
        assert!(rewritten.contains("echo hi") && rewritten.contains("echo again"));
    }

    #[test]
    fn truncated_managed_block_never_modifies_foreign_hook() {
        let content =
            "#!/bin/sh\n# >>> portool >>>\nportool sync --quiet || true\nimportant-user-code\n";
        assert!(matches!(
            managed_block_state(content),
            ManagedBlockState::Malformed
        ));
        // install_into / deinit_hook must not touch the file at all.
        let tmp = TempDir::new().unwrap();
        let hook = tmp.path().join("post-checkout");
        fs::write(&hook, content).unwrap();
        let out = install_into(&hook, "SCRIPT", "BLOCK\n").unwrap();
        assert!(matches!(out, HookOutcome::ManualRequired { .. }));
        assert_eq!(fs::read_to_string(&hook).unwrap(), content);
        let out = deinit_hook(&hook).unwrap();
        assert!(matches!(out, HookOutcome::ManualRequired { .. }));
        assert_eq!(fs::read_to_string(&hook).unwrap(), content);
    }

    #[test]
    fn duplicate_markers_are_malformed() {
        for content in [
            "# >>> portool >>>\nx\n# <<< portool <<<\n# >>> portool >>>\ny\n# <<< portool <<<\n",
            "# <<< portool <<<\n",
            "# <<< portool <<<\n# >>> portool >>>\n",
        ] {
            assert!(
                matches!(managed_block_state(content), ManagedBlockState::Malformed),
                "{content:?}"
            );
        }
    }

    #[test]
    fn block_is_inserted_after_shebang_not_appended() {
        let existing = "#!/bin/sh\ndo_something\nexit 0\n";
        let tmp = TempDir::new().unwrap();
        let hook = tmp.path().join("post-checkout");
        fs::write(&hook, existing).unwrap();
        let block = "# >>> portool >>>\nB\n# <<< portool <<<\n";
        assert_eq!(
            install_into(&hook, "S", block).unwrap(),
            HookOutcome::Installed
        );
        let written = fs::read_to_string(&hook).unwrap();
        assert_eq!(
            written,
            "#!/bin/sh\n# >>> portool >>>\nB\n# <<< portool <<<\ndo_something\nexit 0\n"
        );
    }

    #[test]
    fn block_is_inserted_at_top_when_no_shebang() {
        let tmp = TempDir::new().unwrap();
        let hook = tmp.path().join("post-checkout");
        fs::write(&hook, "do_something\n").unwrap();
        let block = "# >>> portool >>>\nB\n# <<< portool <<<\n";
        install_into(&hook, "S", block).unwrap();
        assert_eq!(
            fs::read_to_string(&hook).unwrap(),
            "# >>> portool >>>\nB\n# <<< portool <<<\ndo_something\n"
        );
    }

    #[test]
    fn install_into_reports_symlink_and_nonshell_as_not_installed() {
        let tmp = TempDir::new().unwrap();
        let hook = tmp.path().join("post-checkout");
        std::os::unix::fs::symlink("/nonexistent", &hook).unwrap();
        assert_eq!(
            install_into(&hook, "S", "B\n").unwrap(),
            HookOutcome::SkippedSymlink
        );
        let py = tmp.path().join("post-merge");
        fs::write(&py, "#!/usr/bin/env python3\nprint('hi')\n").unwrap();
        assert!(matches!(
            install_into(&py, "S", "B\n").unwrap(),
            HookOutcome::ManualRequired { .. }
        ));
    }

    #[test]
    fn deinit_hook_propagates_read_errors() {
        let tmp = TempDir::new().unwrap();
        let hook = tmp.path().join("post-checkout");
        fs::write(&hook, [0xFF, 0xFE]).unwrap(); // non-UTF-8 -> read_to_string error
        assert!(deinit_hook(&hook).is_err());
    }

    #[test]
    fn replace_managed_block_returns_none_when_already_current() {
        let block = hook_append_block(Some("/p"));
        let existing = format!("#!/bin/sh\necho hi\n{block}");
        let (begin, end) = match managed_block_state(&existing) {
            ManagedBlockState::Valid { begin, end } => (begin, end),
            _ => panic!("expected a valid block"),
        };

        assert_eq!(replace_managed_block(&existing, begin, end, &block), None);
    }

    #[test]
    fn deinit_hook_removes_each_new_form() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();

        // Owned standalone.
        let owned = hooks_dir.join("owned");
        fs::write(&owned, hook_script(Some("/p"))).unwrap();
        deinit_hook(&owned).unwrap();
        assert!(!owned.exists());

        // Managed block in a foreign hook.
        let managed = hooks_dir.join("managed");
        let block = hook_append_block(Some("/p"));
        fs::write(&managed, format!("#!/bin/sh\necho hi\n{block}")).unwrap();
        deinit_hook(&managed).unwrap();
        assert_eq!(
            fs::read_to_string(&managed).unwrap(),
            "#!/bin/sh\necho hi\n"
        );

        // Legacy appended line.
        let legacy = hooks_dir.join("legacy");
        fs::write(
            &legacy,
            "#!/bin/sh\necho hi\nif command -v portool >/dev/null 2>&1; then portool sync --quiet || true; fi\n",
        )
        .unwrap();
        deinit_hook(&legacy).unwrap();
        assert_eq!(fs::read_to_string(&legacy).unwrap(), "#!/bin/sh\necho hi\n");
    }

    #[test]
    fn install_into_writes_the_safe_script_and_sets_exec_bit() {
        let tmp = TempDir::new().unwrap();
        let hook_path = tmp.path().join("repo/.git/hooks/post-checkout");
        let script = hook_script(Some("/p"));
        let block = hook_append_block(Some("/p"));

        install_into(&hook_path, &script, &block).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content, script);
        let mode = fs::metadata(&hook_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn install_into_new_hook_exits_zero_and_reports_failure() {
        let tmp = TempDir::new().unwrap();
        let hook_path = tmp.path().join("repo/.git/hooks/post-checkout");
        let script = hook_script(None);
        let block = hook_append_block(None);

        install_into(&hook_path, &script, &block).unwrap();

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
        let script = hook_script(Some("/p"));
        let block = hook_append_block(Some("/p"));

        install_into(&hook_path, &script, &block).unwrap();
        install_into(&hook_path, &script, &block).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content, script);
    }

    #[test]
    fn install_into_inserts_into_an_existing_sh_hook() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        fs::write(&hook_path, "#!/bin/sh\necho hi\n").unwrap();
        let script = hook_script(Some("/p"));
        let block = hook_append_block(Some("/p"));

        install_into(&hook_path, &script, &block).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(content.starts_with("#!/bin/sh\n# >>> portool >>>\n"));
        assert!(content.ends_with("echo hi\n"));
        assert!(crate::hooks::contains_portool_invocation(&content));
        assert!(
            content.contains("|| true"),
            "inserted block must be self-neutralizing"
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
        let script = hook_script(Some("/p"));
        let block = hook_append_block(Some("/p"));

        install_into(&hook_path, &script, &block).unwrap();

        // Left byte-identical: portool never injects sh into a python hook.
        assert_eq!(fs::read_to_string(&hook_path).unwrap(), original);
    }

    #[test]
    fn install_into_leaves_a_non_utf8_hook_untouched() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        // Invalid UTF-8 byte sequence: not readable as a string.
        let original: &[u8] = b"#!/bin/sh\n\xff\xfe not valid utf-8\n";
        fs::write(&hook_path, original).unwrap();
        let script = hook_script(Some("/p"));
        let block = hook_append_block(Some("/p"));

        install_into(&hook_path, &script, &block).unwrap();

        // Left byte-identical: portool can't reason about non-UTF-8 content.
        assert_eq!(fs::read(&hook_path).unwrap(), original);
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
        let script = hook_script(Some("/p"));
        let block = hook_append_block(Some("/p"));

        install_into(&hook_path, &script, &block).unwrap();

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
        let script = hook_script(Some("/p"));
        let block = hook_append_block(Some("/p"));

        install_into(&hook_path, &script, &block).unwrap();

        // Appended-to, but 0700 is preserved (not widened to 0755).
        assert!(crate::hooks::contains_portool_invocation(
            &fs::read_to_string(&hook_path).unwrap()
        ));
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
        let script = hook_script(Some("/p"));
        let block = hook_append_block(Some("/p"));

        install_into(&hook_path, &script, &block).unwrap();

        // The unsafe legacy script is now the current safe script.
        assert_eq!(fs::read_to_string(&hook_path).unwrap(), script);
    }

    #[test]
    fn install_into_migrates_unsafe_line_in_foreign_hook() {
        let tmp = TempDir::new().unwrap();
        let hooks_dir = tmp.path().join("repo/.git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        let unsafe_hook = "#!/bin/sh\necho hi\nif command -v portool >/dev/null 2>&1; then portool sync --quiet; fi\n";
        fs::write(&hook_path, unsafe_hook).unwrap();
        let script = hook_script(Some("/p"));
        let block = hook_append_block(Some("/p"));

        install_into(&hook_path, &script, &block).unwrap();

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
        let script = hook_script(Some("/p"));
        let block = hook_append_block(Some("/p"));

        install_into(&hook_path, &script, &block).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(content.contains("# remember to run portool sync manually"));
        // The comment is left; portool appends its own guarded block below it.
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

        let expected = hook_script(portool_bin_path().as_deref());
        assert_eq!(
            fs::read_to_string(hooks_dir.join("post-checkout")).unwrap(),
            expected
        );
        assert_eq!(
            fs::read_to_string(hooks_dir.join("post-merge")).unwrap(),
            expected
        );
    }

    #[test]
    fn contains_unsafe_portool_form_recognizes_old_but_not_new() {
        let legacy = "#!/bin/sh\n# installed by portool\ncommand -v portool >/dev/null 2>&1 && portool sync --quiet\n";
        assert!(contains_unsafe_portool_form(legacy));
        assert!(contains_unsafe_portool_form(
            "#!/bin/sh\necho hi\nif command -v portool >/dev/null 2>&1; then portool sync --quiet; fi\n"
        ));
        assert!(!contains_unsafe_portool_form(&hook_script(Some("/p"))));
        assert!(!contains_unsafe_portool_form(&hook_script(None)));
        assert!(!contains_unsafe_portool_form(
            "#!/bin/sh\n# remember to run portool sync manually\n"
        ));
    }

    #[test]
    fn update_exclude_creates_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let common_dir = tmp.path();

        update_exclude(common_dir).unwrap();
        update_exclude(common_dir).unwrap();

        let content = fs::read_to_string(exclude_path(common_dir)).unwrap();
        assert_eq!(content, "# managed by portool\n.env.portool\n");
    }

    #[test]
    fn update_exclude_preserves_existing_content() {
        let tmp = TempDir::new().unwrap();
        let common_dir = tmp.path();
        let path = exclude_path(common_dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "node_modules\n").unwrap();

        update_exclude(common_dir).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(
            content,
            "node_modules\n# managed by portool\n.env.portool\n"
        );
    }

    #[test]
    fn update_exclude_skips_a_bare_user_added_line() {
        let tmp = TempDir::new().unwrap();
        let common_dir = tmp.path();
        let path = exclude_path(common_dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "node_modules\n.env.portool\n").unwrap();

        update_exclude(common_dir).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(
            content, "node_modules\n.env.portool\n",
            "already-present line means no managed pair is added"
        );
    }

    #[test]
    fn deinit_exclude_removes_only_the_managed_pair() {
        let tmp = TempDir::new().unwrap();
        let common_dir = tmp.path();
        let path = exclude_path(common_dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "node_modules\n# managed by portool\n.env.portool\nother.log\n",
        )
        .unwrap();

        deinit_exclude(common_dir).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "node_modules\nother.log\n");
    }

    #[test]
    fn deinit_exclude_leaves_a_bare_user_added_line() {
        let tmp = TempDir::new().unwrap();
        let common_dir = tmp.path();
        let path = exclude_path(common_dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "node_modules\n.env.portool\n").unwrap();

        deinit_exclude(common_dir).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(
            content, "node_modules\n.env.portool\n",
            "a bare line the user added themselves must be left alone"
        );
    }
}
