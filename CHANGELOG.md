# Changelog

All notable changes to portool are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/) (pre-1.0: a breaking change bumps
the minor version).

## [0.10.0] - 2026-07-18

The 5th-round external review's seven P0 items: hook writes become
symlink-safe, the ledger gains a stale-backup recovery contract, `deinit`/
`unhook` become transactional, the machine-readable API stops hiding pending
moves, identity checks reject half-written state, and shebang classification
is tokenized. **Ledger schema bumps to v4** (a v3 ledger auto-migrates).

### Breaking

- **Ledger schema v4** adds a top-level monotonic `sequence`, and every
  `.env.portool` header now carries a `sequence:` field. A v3 ledger
  migrates automatically in memory and is rewritten on the next locked
  save; a pre-0.10 `.env.portool` (no `sequence:`) keeps working and gains a
  `sequence:` field the next time a slow-path `sync` rewrites it (a branch
  change, a manifest change, or `reallocate`) — until then it is simply
  excluded from stale-backup detection, which stays sound via the ledger's
  own backup comparison and `doctor` reconciliation. Nothing you depend on
  in the CLI output changes shape except the additive `ls --json` fields
  below.
- **`portool check` now exits non-zero on a degraded backup** — one whose
  parsed `sequence` is behind the ledger, or that is unreadable/corrupt —
  instead of only printing a warning. A missing backup (fresh ledger) is
  still just a warning.
- **`deinit` and `unhook` exit non-zero and print a `partial_deinit` /
  `partial_unhook` JSON object** listing residue when they cannot fully
  complete (a symlink/unreadable/malformed/non-shell hook that still invokes
  portool), instead of reporting success. A clean run is unchanged.

### Security

- **Hook writes are symlink-safe (P0-1).** Every hook and `info/exclude`
  write now goes through an `openat(O_NOFOLLOW)`, fd-relative walk that
  refuses the moment any path component below the repository boundary is a
  symlink, `..`, or otherwise escapes — closing a reproduced escape where a
  symlinked `.husky` (or hooks dir, or hook file) redirected a write outside
  the repository. The check and the write share the same verified directory
  descriptor, so a check-then-swap race cannot slip through. A
  `core.hooksPath` whose nearest existing ancestor resolves outside the repo
  is now refused even when its leaf does not yet exist (the old
  lexical-only fallback left an intermediate symlink unresolved).
- **Shebang classification is tokenized (P0-6).** Hook interpreter detection
  parses the shebang (resolving `/usr/bin/env [-S] <cmd>`, matching the
  interpreter *basename* exactly against `sh`/`bash`/`dash`) instead of a
  substring test, and fails closed on an unknown interpreter, an empty file,
  a leading BOM, or an oversized shebang — so `zsh`, `flash`, or a Python
  hook is never mistaken for a POSIX shell.

### Added

- **A stale-backup recovery contract (P0-2).** The ledger's monotonic
  `sequence` is mirrored into each `.env.portool`. `doctor --repair`
  restores the whole ledger from its backup (all projects kept) and emits a
  machine-readable `portool_recovery` advisory noting that other projects
  may need their own repair. `sync`/`reallocate` quarantine new allocation
  when a *tracked* worktree's env records a higher sequence than the ledger
  (a stale-backup rollback), pointing at `doctor --repair`.
- **`ls --json` allocations now carry `pending_block`, `env_block`,
  `state`, and `sync_required` (P0-4).** `state` is one of `ok`,
  `pending_move`, `recovery_required`, `env_missing`, or `stale`, so a
  machine consumer is never shown a clean state while a move is pending or
  the env disagrees with the ledger.

### Fixed

- **Partial identity state is rejected (P0-5).** A `.env.portool` with
  exactly one of `PORTOOL_PROJECT_ID` / `PORTOOL_WORKTREE_ID` is now treated
  as corruption (`doctor` refuses to import it and asks for manual repair),
  never conflated with a legitimately ID-less pre-0.9 file.

## [0.9.0] - 2026-07-18

The 4th-round external review items: identity IDs widen to 128 bits, the
crate drops its library target, hook-managed-block handling becomes fully
fail-closed against destructive auto-repair, and several reporting/reclaim
gaps close. **No ledger schema change** (still v3).

### Breaking

- **`PORTOOL_PROJECT_ID` / `PORTOOL_WORKTREE_ID` widen from 16 to 32 hex
  characters** (64→128-bit truncated SHA-256). Existing worktrees pick up
  new, longer IDs on their next `sync`; anything derived from the old ID —
  Docker Compose project/container/network/volume names, per-worktree
  caches — changes accordingly. Migration note: `doctor` will not re-import
  a pre-0.9 `.env.portool` (its 16-hex IDs no longer match); run `portool
  sync` in each worktree after upgrading to rewrite the env file.
- **`unhook`/`deinit` exit non-zero when a hook file exists but cannot be
  read**, instead of silently succeeding as if there were nothing to
  remove.
- **The crate no longer ships a library target.** `portool` is binary-only;
  the CLI (documented commands, exit codes, file formats) is the only
  interface, so there is no Rust API to depend on or that follows semver.
- **`core.hooksPath` resolving outside the repository is refused in *any*
  config scope** (`local`, `worktree`, `global`, `system`, or a
  command-line `-c`), not just an absolute path set in shared
  `global`/`system` scope. A relative `../` escape or a symlink into
  another directory is now caught in every scope too.
- **`exec --strict` and `--reallocate-on-conflict` are mutually
  exclusive** — passing both is now a CLI usage error (exit 64) instead of
  one silently winning.
- **`unpin` now clears the pin's label**, so a later label-less `pin`
  doesn't resurrect a stale name.
- **`init` exits non-zero when the post-checkout hook could not actually be
  installed** — a symlinked hook, a non-shell interpreter, or
  unreadable/malformed existing content — even though the `info/exclude`
  update and initial `sync` still ran.
- **A malformed managed hook block is never auto-repaired.** Previously a
  truncated block (a missing, duplicated, or reversed `# >>> portool >>>` /
  `# <<< portool <<<` marker) was treated as extending to end-of-file and
  owned wholesale, which could delete a user's own code appended after it.
  `init`/`unhook`/`deinit` now leave a malformed block completely untouched
  and demand manual repair.

### Fixed

- **The managed hook block is now inserted right after the shebang**
  instead of appended at end-of-file, so a pre-existing top-level
  `exit`/`exec` can no longer make it unreachable. `doctor` warns
  (advisory) about an unreachable or malformed managed block.
- **`prune --all` honors pins even when the repository directory is gone**,
  so a pinned worktree in a deleted repo clone is no longer reclaimed out
  from under its pin.
- **Internal git spawns scrub inherited `GIT_*` environment variables**, so
  running portool from inside another git operation can't leak the wrong
  repo context into its own git calls; git command failures now surface
  the underlying stderr instead of a generic message.
- **`doctor` skips worktrees with a non-UTF-8 path** instead of risking a
  lossy, potentially colliding ledger key, and verifies a worktree's
  `.env.portool` `PORTOOL_PROJECT_ID` / `PORTOOL_WORKTREE_ID` match before
  importing it.
- **`deinit` removes `.env.portool` from the union of live worktrees and
  ledger-recorded paths**, so it still cleans up every worktree even when
  the ledger is missing, was abandoned, or is already empty.
- **`check` warns when `registry.json.bak` looks stale**, so a silently
  failed backup refresh doesn't go unnoticed until a repair is actually
  needed; the backup refresh itself remains best-effort.
- **A config whose `block_align` exceeds the pool's range capacity is
  rejected at load** instead of only surfacing later as `PoolExhausted` on
  every `sync`.
- **Corrupt-ledger aside files are uniquely named**
  (`registry.json.corrupt-<nanos>-<pid>`), so two repairs within the same
  wall-clock second no longer collide on one filename.
- **Human-readable output is sanitized across every command**, not just
  `ls` and `sync` — control characters and ANSI escapes in worktree paths,
  branch names, and labels can no longer inject themselves into terminal
  output. Overlap validation is also sort-based (O(n log n)) instead of a
  pairwise scan.

### Added

- **Pin labels show in human `ls` output** (a new trailing `LABEL`
  column).
- **`ls --json` emits the versioned error envelope for every failure
  path**, not just some of them.
- **MSRV is declared** (`rust-version = "1.85"`), with a matching CI job so
  a build against an older toolchain fails predictably instead of hitting
  an unexplained compile error.

## [0.8.0] - 2026-07-17

The 3rd-round external review items: `exec`'s bind check stops being a
default-on surprise, a manifest that outgrows its block tries to grow in
place before moving, an implicit block move is refused while it would split
a still-running worktree across ports, and the ledger/env/backup writes get
real durability guarantees. **No ledger schema change** (still v3).

### Breaking

- **`exec` no longer bind-checks the block by default.** A worktree's own
  already-running dev servers legitimately occupy it, so the default-on
  check was noise on every second `portool exec`. Pass `--check-ports` for
  the advisory warning (unchanged behavior otherwise); `--strict` and
  `--reallocate-on-conflict` each imply the check, since acting on a
  conflict requires detecting one.
- **`sync` refuses an implicit block move while the current block's ports
  are in use.** A manifest that outgrows its block and can't be extended
  in place used to move to a fresh block unconditionally, which could split
  a running worktree across its old and new ports. `sync` now errors
  instead — stop the processes and re-run `sync`, or run `portool
  reallocate` to move explicitly and accept the split.

### Added

- **In-place block growth is tried first** when a manifest outgrows its
  block: the block is extended in place (keeping its start; only the newly
  added tail is bind-checked) rather than handed to the general allocator,
  which would otherwise tend to move it to an unrelated slot. The general
  allocator (and the new implicit-move refusal above) only comes into play
  when the extension doesn't fit — blocked by a neighboring block/
  reservation or the end of the pool.
- **`exec --env-file-overrides`** — lets the passed `-e`/`--env-file`
  files beat the parent environment instead of losing to it, so an
  explicitly passed `.env.test` can't be silently shadowed by a stale
  parent-shell variable of the same name. Default precedence is unchanged
  (parent beats files); with the flag it becomes `parent < files <
  portool-managed variables`.

### Fixed

- **Ledger, env, and backup writes are fsynced and rename-atomic.** Every
  write goes through a temp file in the same directory, `fsync`s it, then
  renames it into place (with a best-effort directory fsync after), so a
  crash mid-write can never leave a partially written file. The backup
  (`registry.json.bak`) is refreshed the same way — temp file + rename —
  instead of an in-place copy, so a crash mid-backup can no longer leave it
  half-overwritten. (This is a standard `fsync`/`rename` guarantee, not
  macOS's stronger `F_FULLFSYNC`.)
- **`ls` and sync's stdout output sanitize control characters** in
  worktree paths, branch names, and labels — a newline or ANSI escape
  sequence can no longer inject itself into a table or summary line — and
  table columns align by Unicode display width instead of byte length, so
  CJK and other wide characters no longer skew the table.

## [0.7.0] - 2026-07-17

The trust-contract items from the post-0.6.0 external review: `doctor
--repair` becomes restore-first, `deinit` fully reverses `init`, the ignore
rule moves off the tracked `.gitignore`, `ls --json` gets a stable
envelope, and the manifest/config parsers close the remaining
silently-wrong-defaults gaps. **No ledger schema change** (still v3); the
breaking changes are all in command semantics and output shapes.

### Breaking

- **`doctor --repair` is restore-first**, not destructive. A corrupt
  ledger is now restored from `registry.json.bak` (every project kept);
  the old move-aside-and-start-empty behavior only happens with the new,
  explicit `--repair --abandon-other-projects` flag (also the only path
  used for an unsupported-version ledger written by a newer portool).
- **`deinit` now releases the project's ledger allocations and removes its
  `.env.portool` files** in addition to hooks and the ignore rule — pass
  `--keep-allocations` for the old, hooks-and-ignore-only behavior. Hook
  removal on its own is the new `portool unhook`.
- **`init` writes the ignore rule to `$GIT_COMMON_DIR/info/exclude`**, not
  the tracked `.gitignore` — shared by every linked worktree, never
  committed. `init` no longer edits `.gitignore`; a leftover line from an
  older portool only earns a hint. `--gitignore-only` keeps its name but
  now updates `info/exclude`.
- **`init` exits non-zero when no hook location is installable** (a
  `core.hooksPath` pointing at a directory that doesn't exist, or an
  absolute `core.hooksPath` in shared `global`/`system` git config scope)
  instead of silently succeeding with no hook installed.
- **`ls --json` emits a versioned format v1 envelope**
  (`format_version`/`ok`/`registry_schema_version`/`effective_config`/
  `allocations`/`reservations`, or `{"format_version":1,"ok":false,
  "error":...}` on failure) instead of the raw ledger. `format_version`
  will only ever gain fields on 1.x; a breaking shape change bumps it.
- **Manifest parsing is fail-closed**: an empty `[ports]` table, an
  unknown top-level field, or a `portool`/`portool_*` key is now a hard
  `.portool.toml` error instead of silently producing a full-pool block or
  a colliding env var.
- **Config `gc_days` is deprecated and ignored** (with a warning if set) —
  GC is condition-based (gone worktree directory + free ports), not
  age-based; the field is gone from `Config` itself.
- **Preferred block placement no longer special-cases `main`/`master`.**
  Every branch, including `main`, now hashes `project + branch` to a
  preferred slot; only a detached worktree falls back to hashing its path.

### Added

- **`registry.json.bak`** is refreshed on every ledger save — the backup
  `doctor --repair` restores from.
- **`reserve <PORT|START-END> [--label]` / `unreserve`** — permanent
  reservations for ports portool must never hand out (e.g. a stopped
  Postgres that a bind check alone would read as "free"). `unreserve`
  matches a single port against its containing reservation; a range must
  match exactly.
- **`pin [--label]` / `unpin`** — exempt the current worktree's allocation
  from every GC path until unpinned. Reservations and pinned worktrees now
  show up in `portool ls`'s table footer and JSON output.
- **`portool exec` no longer requires a `.portool.toml`** — with no
  manifest it injects a single `block_align`-wide block as `PORT`, same as
  `sync`.
- **Hook scripts embed the absolute `portool` binary path** at install
  time (`PORTOOL_BIN="..."`, falling back to a PATH lookup if that path
  stops existing), so GUI git clients with a minimal `PATH` can still find
  it. A foreign hook now gets a managed `# >>> portool >>> ... # <<<
  portool <<<` block instead of a single appended line, so re-running
  `init` can refresh it cleanly.
- **`doctor` diagnoses hook effectiveness** as advisories: missing, not
  executable, not invoking portool, or invoking a dead embedded path.
- **`examples/webapp`** ships inside the published crate (`cargo package
  --list` now includes it), so `cargo install portool` and `crates.io`
  users get the same runnable example as the repo.

### Changed / Fixed

- **`prune --all` no longer treats a `git worktree list` failure as "no
  worktrees".** A project whose worktree enumeration fails is skipped
  (with a stderr message) and its entries are kept, instead of being
  treated as fully stale and pruned away.
- **`ls --json` reports the real configured range** (`effective_config`)
  even when no ledger exists yet, instead of fabricating
  `Config::default()`.
- **Port derivation uses checked arithmetic**: `checked_add` instead of a
  saturating add, with in-block validation, so a manifest offset that
  would overflow `u16` is now a clean error instead of a silently wrapped
  or truncated port.
- **Implicit GC now runs before allocation** in `sync`, so a worktree
  re-created on the same branch reclaims the block it just vacated instead
  of being forced onto a different one.
- **`envfile::variables` / `envfile::render` return `Result`** instead of
  panicking or silently producing a partial environment on a bad manifest.

## [0.6.0] - 2026-07-17

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
