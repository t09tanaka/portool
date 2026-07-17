# Batch A — Hook Safety Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Make portool's git hooks incapable of failing the caller's Git,
migrate unsafe legacy hooks, append only to shell hooks non-destructively,
refuse shared-scope `core.hooksPath`, and cover `post-merge` too.

**Architecture:** All changes live in `src/cmd/init.rs` (install/migrate) and
`src/hooks.rs` (location + scope classification), with a one-line hint added
in `src/cmd/sync.rs`. TDD; unit tests in the touched modules; behavior-level
integration coverage in `tests/cli.rs` where a real git repo is needed.

**Tech Stack:** Rust 2021, `std::os::unix::fs::PermissionsExt`, `tempfile`
for atomic writes, `git config --show-scope`.

## Global Constraints

- macOS + Linux only. Effective git floor rises to 2.36 in Batch D (`-z`);
  Batch A uses `git config --show-scope` (git ≥ 2.26).
- Never break the caller's Git: a portool hook must exit 0 regardless of
  `sync` outcome.
- portool-owned hook lines are identified by the exact shapes portool has
  emitted (not a loose `contains`), per the Fable review.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo
  test` must pass. Reference spec:
  `docs/superpowers/specs/2026-07-17-portool-hardening-design.md` §Batch A.

---

### Task 1: Safe hook script & append line (A1)

**Files:** Modify `src/cmd/init.rs` (constants + `install_into` new-file path);
Test: `src/cmd/init.rs` `#[cfg(test)]`.

**Interfaces:**
- Produces: `HOOK_SCRIPT` (standalone, ends `exit 0`), `HOOK_APPEND_LINE`
  (guarded, ends `|| true`), consumed by Tasks 2–5.

New constants:

```rust
/// Standalone hook: guaranteed `exit 0` so a sync failure never fails Git.
const HOOK_SCRIPT: &str = "#!/bin/sh\n\
# installed by portool\n\
if command -v portool >/dev/null 2>&1; then\n\
\x20\x20portool sync --quiet || echo 'portool: sync failed; Git was not blocked' >&2\n\
fi\n\
exit 0\n";
/// Line appended into a foreign hook: `|| true` so portool can never become
/// the foreign hook's failing exit status (it is the last line).
const HOOK_APPEND_LINE: &str =
    "if command -v portool >/dev/null 2>&1; then portool sync --quiet || true; fi\n";
```

- [ ] **Step 1:** Write failing test `install_into_new_hook_exits_zero`:
  a fresh install produces a file that `== HOOK_SCRIPT`, ends with `exit 0\n`,
  and whose body contains `|| echo`.
- [ ] **Step 2:** Run `cargo test -p portool install_into_new_hook_exits_zero`
  → FAIL (old script has no `exit 0`).
- [ ] **Step 3:** Update the two constants as above.
- [ ] **Step 4:** Run the test → PASS. Update the existing
  `install_into_writes_the_spec_script_and_sets_exec_bit` expectation to the
  new script.
- [ ] **Step 5:** Commit `feat(hook): guarantee hook exits 0 on sync failure`.

### Task 2: Interpreter-aware, non-destructive append (A3)

**Files:** Modify `src/cmd/init.rs` (`install_into` existing-file path + new
helpers); Test: `src/cmd/init.rs`.

**Interfaces:**
- Produces: `fn shebang_is_posix_shell(existing: &str) -> bool`,
  `fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()>`,
  `fn ensure_executable(path: &Path) -> Result<()>` — consumed by Tasks 3–5.

Helper logic:

```rust
/// A hook is safe to append POSIX `sh` to when it has no shebang (git runs
/// such hooks via sh) or a shell shebang. Python/Node/Ruby/etc. are not.
fn shebang_is_posix_shell(existing: &str) -> bool {
    match existing.lines().next() {
        None => true,
        Some(first) if first.starts_with("#!") => {
            ["/sh", "/bash", "/dash", "env sh", "env bash", "env dash"]
                .iter()
                .any(|m| first.contains(m))
        }
        Some(_) => true,
    }
}

/// Add owner-execute if missing, preserving all other permission bits
/// (never downgrade a 0700 hook to 0755).
fn ensure_executable(path: &Path) -> Result<()> {
    let meta = fs::metadata(path)?;
    let mode = meta.permissions().mode();
    if mode & 0o100 == 0 {
        let mut perms = meta.permissions();
        perms.set_mode(mode | 0o100);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}
```

`install_into` behavior:
- **Symlink:** `fs::symlink_metadata(path)` reports a symlink → warn
  (`warning: <path> is a symlink; not modifying it`) and return `Ok(())`
  without touching it.
- **New file:** write `HOOK_SCRIPT` via `atomic_write`, then `set_mode(0o755)`.
- **Existing shell/none-shebang hook without a portool line:** append
  `HOOK_APPEND_LINE` (ensuring a trailing newline first) via `atomic_write`;
  `ensure_executable`.
- **Existing non-shell hook (Python/Node/…):** warn + print the manual
  one-liner (mirror the `Missing` case), do not modify; still
  `ensure_executable` only if a portool line is already present — otherwise
  leave perms untouched.
- **Non-UTF-8 / unreadable:** leave untouched (current behavior), do not
  force perms.

- [ ] **Step 1:** Failing test `install_into_skips_python_hook`: an existing
  `#!/usr/bin/env python3\nprint('x')\n` hook is left byte-identical after
  `install_into`, and no portool marker is added.
- [ ] **Step 2:** `cargo test … install_into_skips_python_hook` → FAIL
  (current code appends sh unconditionally).
- [ ] **Step 3:** Implement `shebang_is_posix_shell` + gate the append.
- [ ] **Step 4:** Test → PASS.
- [ ] **Step 5:** Add tests `install_into_rejects_symlink`,
  `install_into_preserves_0700_perms` (a `0700` sh hook stays `0700` +x, not
  `0755`), `install_into_appends_to_sh_hook`. Implement `atomic_write` +
  `ensure_executable` to make them pass.
- [ ] **Step 6:** Commit `feat(hook): interpreter-aware, non-destructive append`.

### Task 3: Migrate unsafe legacy/older hooks (A2)

**Files:** Modify `src/cmd/init.rs` (`install_into` migration branch +
helper); Test: `src/cmd/init.rs`.

**Interfaces:**
- Consumes: `HOOK_SCRIPT`, `HOOK_APPEND_LINE`, `atomic_write` (Tasks 1–2).
- Produces: `fn migrate_unsafe_lines(existing: &str) -> Option<String>` —
  returns the rewritten content when an unsafe portool form was found, else
  `None`. Consumed by sync-hint check (Task 5 reads the same shapes).

Exact unsafe shapes portool has emitted (rewrite targets):

```rust
/// Whole-file portool scripts portool owns outright (→ replace with the new
/// HOOK_SCRIPT). Compared after trimming a single trailing newline.
const OWNED_FULL_SCRIPTS: &[&str] = &[
    // legacy <= 0.2
    "#!/bin/sh\n# installed by portool\ncommand -v portool >/dev/null 2>&1 && portool sync --quiet",
    // 0.3 / 0.4 (unsafe: no exit 0)
    "#!/bin/sh\n# installed by portool\nif command -v portool >/dev/null 2>&1; then\n  portool sync --quiet\nfi",
];
/// Single unsafe portool lines inside a foreign hook (→ replace with
/// HOOK_APPEND_LINE). Matched on the trimmed line.
const UNSAFE_PORTOOL_LINES: &[&str] = &[
    "command -v portool >/dev/null 2>&1 && portool sync --quiet",
    "if command -v portool >/dev/null 2>&1; then portool sync --quiet; fi",
];
```

`migrate_unsafe_lines`:
- If `existing.trim_end_matches('\n')` equals an `OWNED_FULL_SCRIPTS` entry →
  return `Some(HOOK_SCRIPT.to_string())` (full rewrite).
- Else rewrite line-by-line: any line whose `trim()` equals an
  `UNSAFE_PORTOOL_LINES` entry becomes `HOOK_APPEND_LINE` (sans trailing
  `\n`, re-joined); if any line changed, return `Some(joined)`, else `None`.
- Never touch lines that don't exactly match these shapes.

`install_into` calls `migrate_unsafe_lines` first; if `Some`, `atomic_write`
the result + `ensure_executable` and stop (already-safe / migrated). The
existing `install_into_does_not_duplicate_an_old_style_marker_line` test is
**inverted**: the legacy hook is now rewritten to the safe form.

- [ ] **Step 1:** Failing test `install_into_migrates_legacy_standalone`:
  a legacy `OWNED_FULL_SCRIPTS[0]` file becomes exactly `HOOK_SCRIPT`.
- [ ] **Step 2:** Run → FAIL (current code preserves it).
- [ ] **Step 3:** Implement `migrate_unsafe_lines` + wire into `install_into`.
- [ ] **Step 4:** Run → PASS. Invert the old preservation test; add
  `install_into_migrates_unsafe_line_in_foreign_hook` (foreign hook whose
  last line is `UNSAFE_PORTOOL_LINES[1]` → that line becomes the `|| true`
  form, foreign lines untouched) and
  `install_into_leaves_user_written_portool_mention_untouched` (a comment
  `# run portool sync manually` is NOT rewritten).
- [ ] **Step 5:** Commit `feat(hook): migrate unsafe legacy hooks to safe form`.

### Task 4: Refuse shared-scope `core.hooksPath` (A4)

**Files:** Modify `src/hooks.rs` (`HooksLocation` + `resolve`), `src/gitctx.rs`
(new `config_scope`), `src/cmd/init.rs` (`install_hook` new arm); Test:
`src/hooks.rs`, `src/gitctx.rs`.

**Interfaces:**
- Produces: `pub fn config_scope(dir: &Path, key: &str) -> Option<String>`
  in `gitctx` (returns `"local"|"global"|"system"|"worktree"`);
  `HooksLocation::SharedScope { configured, resolved, scope }` whose
  `hook_file(_)` returns `None`.

```rust
// gitctx.rs — leading token of `git config --show-scope --get <key>`.
pub fn config_scope(dir: &Path, key: &str) -> Option<String> {
    run_git(dir, &["config", "--show-scope", "--get", key])
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
}
```

`resolve` gains: after `classify` yields `Custom { hooks_dir }` **and** the
raw configured value was absolute, query `config_scope`; if it is `global`
or `system`, return `SharedScope`. Repo-relative paths (inherently per-repo)
are never refused. `install_hook` gets a `SharedScope` arm that warns
(`warning: core.hooksPath '<v>' is a <scope>-scope shared hooks dir; refusing
to auto-install…`) and prints the manual `HOOK_APPEND_LINE`.

- [ ] **Step 1:** Failing unit test in `hooks.rs`: a `SharedScope` value's
  `hook_file("post-checkout")` returns `None`. (Pure; no git needed.)
- [ ] **Step 2:** Run → FAIL (variant doesn't exist).
- [ ] **Step 3:** Add the variant + `hook_file`; add `config_scope`.
- [ ] **Step 4:** Run → PASS. Add an integration test in `tests/cli.rs`:
  a repo with `git config --global core.hooksPath <abs dir>` → `portool init
  --hook-only` writes nothing into that dir and prints the manual hint
  (use `GIT_CONFIG_GLOBAL` pointing at a temp file to simulate global scope
  without touching the host).
- [ ] **Step 5:** Commit `feat(hook): refuse shared-scope core.hooksPath`.

### Task 5: Rename `post_checkout_file` → `hook_file(name)`, install `post-merge` (A5) + sync hint

**Files:** Modify `src/hooks.rs` (`hook_file`), `src/cmd/init.rs`
(`install_hook` installs both hooks), `src/cmd/sync.rs`
(`warn_if_hook_missing` + unsafe-form hint); Test: all three.

**Interfaces:**
- Produces: `HooksLocation::hook_file(&self, name: &str) -> Option<PathBuf>`
  (generalizes `post_checkout_file`; `GitDefault`/`Custom` → `dir.join(name)`,
  `Husky` → `.husky/<name>`, `Missing`/`SharedScope` → `None`).

`install_hook` installs into `hook_file("post-checkout")` **and**
`hook_file("post-merge")` through the same `install_into` (idempotent,
interpreter-aware, migrating). `warn_if_hook_missing` keeps checking
`post-checkout`; additionally, if that hook is present but contains an
`UNSAFE_PORTOOL_LINES`/`OWNED_FULL_SCRIPTS` shape, print the one-line hint:
`portool: your post-checkout hook uses an old form that can fail 'git
checkout'; run 'portool init' to update it`.

- [ ] **Step 1:** Failing test `install_hook_installs_post_merge`: after a
  full `init` in a temp repo, both `<hooks>/post-checkout` and
  `<hooks>/post-merge` exist and equal `HOOK_SCRIPT`.
- [ ] **Step 2:** Run → FAIL (only post-checkout today).
- [ ] **Step 3:** Add `hook_file`, replace `post_checkout_file` callers
  (`init.rs`, `sync.rs`), install both hooks.
- [ ] **Step 4:** Run → PASS. Add `sync_hints_on_unsafe_hook` (a repo with a
  legacy hook → `sync` stderr contains the update hint) and
  `deinit`-forward-compat note (deinit lands in Batch D; here just ensure
  both files carry the marker).
- [ ] **Step 5:** Commit `feat(hook): also manage post-merge; hint on unsafe hook`.

### Task 6: Full verification & PR

- [ ] **Step 1:** `cargo fmt --check` (delegate to a sonnet subagent).
- [ ] **Step 2:** `cargo clippy --all-targets -- -D warnings` (subagent).
- [ ] **Step 3:** `cargo test` (subagent) — all pass, including the inverted
  legacy test.
- [ ] **Step 4:** `/run-github-actions-locally` (logic change → local CI
  before push).
- [ ] **Step 5:** Update README `post-checkout` section to show the new safe
  script (`exit 0`, stderr on failure) and mention `post-merge` +
  shared-scope refusal.
- [ ] **Step 6:** Push branch, `gh pr create` to `main` with a summary of
  A1–A5 and a link to the spec. (README/spec-only follow-ups excepted, this
  PR carries logic — local CI gates the push.)

## Self-Review

- **Spec coverage:** A1→T1, A2→T3+T5, A3→T2, A4→T4, A5→T5. All Batch A
  findings (#1/#2/#5/#4) mapped.
- **Placeholder scan:** none.
- **Type consistency:** `HOOK_SCRIPT`/`HOOK_APPEND_LINE` (T1) reused verbatim
  in T2/T3/T5; `hook_file` (T5) supersedes `post_checkout_file` everywhere in
  one task; `migrate_unsafe_lines`/`OWNED_FULL_SCRIPTS`/`UNSAFE_PORTOOL_LINES`
  shared by T3 and T5's hint.
