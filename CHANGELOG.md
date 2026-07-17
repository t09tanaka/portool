# Changelog

All notable changes to portool are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/) (pre-1.0: a breaking change bumps
the minor version).

## [Unreleased]

The design-change items from the post-0.5.0 external review: transactional
ledger/env state transitions. **Contains a breaking ledger schema change.**

### Breaking

- **Ledger schema v2 → v3.** Each worktree entry gains a `generation`
  counter (bumped on every block change, mirrored into the `.env.portool`
  header) and a `pending_block` slot. v1/v2 ledgers are migrated
  automatically on the next locked write; every block is preserved
  verbatim, so no ports move on upgrade. Older portool binaries refuse a
  v3 ledger fail-closed ("unsupported version").
- **`.env.portool` header format.** The `# block:` line now carries
  `generation: N`. Files written by 0.5.x are upgraded on the next sync.

### Changed / Fixed

- **Block moves are a two-phase update.** Moving a worktree to a different
  block (manifest growth, `reallocate`) now reserves the target block
  alongside the old one, writes `.env.portool`, then finalizes. A crash at
  any point leaves the env's block reserved — the ledger and the env file
  can no longer disagree in a way that hands one worktree's ports to
  another. The next `sync` resolves an interrupted move automatically
  (forward if the env was already rewritten, backward otherwise), and a
  pending target counts as occupied for allocation and GC.
- **The lock-free sync fast path revalidates its snapshot.** After the env
  comparison it re-reads the ledger and requires the same block and
  generation, so a concurrent `reallocate`/`release` between the two reads
  can no longer produce a stale success (the generation counter makes even
  an A→B→A move visible).
- **The Rust API is now explicitly internal.** All library modules are
  `#[doc(hidden)]`; the stable interface is the CLI (commands, exit codes,
  file formats). `cmd::exec::run` no longer panics on an empty command.
- **Non-UTF-8 repository/worktree paths are rejected** (fail-closed)
  instead of being lossily converted into ledger keys that could collide.
- An absolute `core.hooksPath` whose git scope cannot be determined
  (git < 2.26) is refused conservatively (documented; behavior since
  0.5.1).

## [0.5.1] - 2026-07-17

Fixes from the post-0.5.0 external review: the state-transition gaps between
the ledger, `.env.portool`, and Git hooks.

### Changed / Fixed

- **A bad ledger is now truly fail-closed.** A corrupt, semantically
  invalid, or unreadable `registry.json` makes `sync` / `reallocate` /
  `release` / `prune` / `doctor` fail (exit 1) and leaves the file exactly
  where it is — it is no longer silently moved aside and replaced with an
  empty ledger (which reset all allocations and let stopped worktrees'
  blocks be handed out again). A ledger written by a *newer* portool
  (unsupported schema version) is reported distinctly, with "upgrade
  portool" as the fix, and is likewise never touched.
- **`portool doctor --repair`** is the new, single, explicit recovery path:
  it moves the bad ledger aside to `registry.json.corrupt-<ts>` and rebuilds
  the current project's entries from live worktrees' `.env.portool`.
- **The config is validated before the sync fast path.** A `config.toml`
  broken *after* a successful sync now fails the very next `sync`, instead
  of being skipped on the lock-free fast path and only surfacing when some
  unrelated change forced the slow path.
- **A global/system-scope `core.hooksPath` shaped like Husky's `.husky/_` is
  refused.** The shared-scope check now runs before Husky/custom
  classification, so `init` can no longer write a hook into a shared
  directory that would run on every repository's checkout. The check is also
  fail-closed: an absolute `core.hooksPath` whose scope cannot be determined
  (git < 2.26) is treated as shared.
- **`reallocate` always moves.** The current block is kept in the occupied
  set, so `portool reallocate` can never re-select the block it was asked to
  leave (it errors with exit 3 if no other block fits), matching its
  documented contract.
- **`release` removes `.env.portool` before freeing the block.** A failed
  env-file removal now keeps the ledger entry (block still reserved) and
  exits 1, instead of freeing the block while the stale env file kept
  handing out its ports to a second worktree.
- **`doctor` validates before writing.** A nonsense block in a hand-edited
  `.env.portool` header (port 0, reversed range) is reported and skipped,
  and the rebuilt ledger is re-validated before it is saved — `doctor` can
  no longer write a ledger the next command would reject as corrupt.
- **A manifest too wide for a port is rejected.** A `.portool.toml` whose
  required block size exceeds 65535 is a hard error instead of being clamped
  (under which two declared offsets silently shared one port).
- **Real flock errors are no longer reported as timeouts.** Only genuine
  lock contention is retried; any other locking failure (unsupported
  filesystem, I/O error) is returned immediately as itself.

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
