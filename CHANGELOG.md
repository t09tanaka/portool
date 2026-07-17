# Changelog

All notable changes to portool are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/) (pre-1.0: a breaking change bumps
the minor version).

## [0.5.0] - 2026-07-17

A hardening release that makes portool's guarantees match its claims,
addressing a full external review (16 findings). **Contains breaking
changes.**

### Breaking

- **Ledger schema v1 → v2.** The per-project `subranges` field is removed. A
  v1 `registry.json` is migrated automatically and in place on the next
  locked write; every worktree block is preserved verbatim, so no ports move
  on upgrade.
- **Allocation model.** Worktree blocks are now allocated directly from the
  pool instead of from a per-project 500-wide subrange. This removes the
  ~14-repository cap the old model imposed on the default pool regardless of
  actual usage. `main`/`master` prefer the pool start; other branches prefer a
  stable hash slot.
- **Config `subrange_size` removed.** A config that still sets it is accepted
  with a deprecation warning and otherwise ignored.
- **Exit codes.** Code `2` (`SubrangeExhausted`) is retired; `3`
  (pool exhausted) covers "no room for a block". CLI usage errors now exit
  `64` (`EX_USAGE`) instead of colliding with a semantic code.

### Added

- `portool reallocate` — force the current worktree onto a fresh block.
- `portool doctor` — rebuild ledger entries from live worktrees'
  `.env.portool` (overlap-guarded), and report blocks whose ports are in use.
- `portool check` — validate the config and ledger; non-zero on any problem.
- `portool release` — free the current worktree's block and remove its
  `.env.portool`.
- `portool deinit` — reverse `init` (remove hook lines + `.gitignore` entry).
- `portool exec` gains `--strict` and `--reallocate-on-conflict`, and now
  bind-checks the block at the execution boundary.
- A `post-merge` hook is installed alongside `post-checkout`, so a
  `.portool.toml` arriving via `git pull` is picked up.

### Changed / Fixed

- **Hooks can no longer fail your Git.** Installed hooks always `exit 0`; a
  `sync` failure is reported to stderr but never blocks `git checkout` /
  `git worktree add`. Unsafe hooks from earlier versions are migrated to the
  safe form. Hook installation is interpreter-aware (only appends to shell
  hooks), never follows symlinks, and preserves existing permissions.
- **Refuses shared-scope hooks.** An absolute `core.hooksPath` in
  `global`/`system` git scope is no longer auto-installed into.
- **Fail-closed config and ledger.** A malformed config, an unknown field, or
  a semantically invalid/corrupt ledger is a hard error instead of a silent
  fallback. `ls --json` exits non-zero (with an error object) on a corrupt
  ledger rather than presenting an empty-but-valid-looking one.
- `.env.portool` sanitizes control characters in its comment header, so a
  newline in a worktree path can't inject a line into a `source`d file.
- `git worktree list` is parsed with `--porcelain -z` (git ≥ 2.36, with a
  fallback), handling newline-bearing and non-UTF-8 paths.
- `branch` / `last_seen_at` are kept current across checkouts (day
  granularity); `XDG_*` values must be absolute; port math is overflow-safe
  and a `range` including port 0 is rejected.
- README reworded to describe portool's real guarantees (a cooperating ledger
  within one OS-user/XDG scope; availability verified at `exec` time).

### Requirements

- git ≥ 2.36 for `worktree list --porcelain -z` (falls back on older git);
  git ≥ 2.26 for `config --show-scope`. macOS / Linux only.

## [0.4.0] - 2026-07-16

- `portool exec`: run a command with the worktree's allocated ports composed
  into its environment (env-file loading, `${NAME}` expansion, `exec(2)`
  hand-off).

## [0.1.0] - 2026-07-15

- Initial release: passive per-worktree port allocation via a global ledger
  and a `post-checkout` hook, with `.env.portool` output.
