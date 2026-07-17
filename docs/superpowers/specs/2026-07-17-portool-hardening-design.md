# portool Hardening — Design Spec

- **Date:** 2026-07-17
- **Status:** Approved for implementation
- **Scope:** Address the full external review (16 findings, P0–P2) and make
  portool's *guarantees* match its *claims*. Breaking changes and a ledger
  schema bump to **v2** are explicitly in scope.

## Context & goal

An external review found portool's implementation locally clean but its
system-level guarantees, failure boundaries, and operational contract
immature relative to what the README sells ("non-conflicting",
"machine-wide", "fully passive"). Every code-level claim in the review was
verified against the current `0.4.0` tree and confirmed (a few — notably the
`core.hooksPath` handling in `src/hooks.rs` — are already partially
mitigated; the design below builds on what exists rather than replacing it).

The goal is not merely to patch each finding but to make portool
**trustworthy**: a tool whose behavior at the edges (Git hook failures,
corrupt ledgers, port contention, config typos, hostile paths) is
predictable and safe, and whose documentation states only what it actually
guarantees.

### Guiding principles

1. **Never break the caller's Git.** portool is an overlay; a portool
   failure must never turn `git checkout` / `git worktree add` into a
   failure.
2. **Fail closed on state, fail open on the hook.** Config and ledger
   problems must surface loudly (non-zero, no silent fallback to defaults).
   The *hook* is the sole exception: it swallows portool's failure so Git
   keeps working.
3. **Verify ports where it matters, stay fast where it doesn't.** The
   per-checkout `sync` fast path stays lock-free and bind-check-free.
   Contention is caught at the **execution boundary** (`exec`) and by
   explicit `doctor` / `reallocate`, not by slowing every checkout.
4. **Say only what is true.** README claims are weakened to portool's real
   scope: a cooperating ledger for one OS-user/XDG scope, kept fresh by a
   hook plus explicit sync.

## Batch overview

| Batch | Theme | Primary files | Ledger schema | Priority |
|---|---|---|---|---|
| A | Hook safety | `cmd/init.rs`, `hooks.rs` | none | P0 |
| B | Fail-closed & honesty | `config.rs`, `registry.rs`, `store.rs`, `cmd/ls.rs`, `main.rs`, `paths.rs`, `README.md` | validation only | P0/P1 |
| C | Allocation model overhaul | `alloc.rs`, `registry.rs`, `cmd/sync.rs`, `cmd/exec.rs`, `store.rs` | **v2 (breaking)** | P0 |
<!-- Batch C addresses findings #3, #1 (bind-recheck/reallocate), #11. -->

| D | Robustness & tooling | `gitctx.rs`, `envfile.rs`, `ports.rs`, `cmd/*` (new commands), tests | none | P1/P2 |

Each batch is one design record here; each becomes its own implementation
plan and PR (default branch, one logical unit per PR). Batch C must land
before or with B's schema-validation code that assumes v2.

---

## Batch A — Hook safety

**Findings:** #1 (failure propagates to Git), #2 (legacy hook not migrated),
#5 (foreign-hook mangling), #4 (shared-scope `hooksPath`).

### A1 — Hook never fails Git (#1)

New standalone-hook script guarantees `exit 0` regardless of sync outcome;
sync failure is reported to stderr, not propagated:

```sh
#!/bin/sh
# installed by portool
if command -v portool >/dev/null 2>&1; then
  portool sync --quiet || echo "portool: sync failed; Git was not blocked" >&2
fi
exit 0
```

The line appended into an existing/foreign hook must likewise be
self-neutralizing (it is the last line, so the hook's exit status becomes
this line's):

```sh
if command -v portool >/dev/null 2>&1; then portool sync --quiet || true; fi
```

`|| true` guarantees portool's invocation can never become the foreign
hook's failing exit code. (Trade-off: a foreign hook whose own final command
previously determined its exit status now always exits 0 once our line is
appended last. Acceptable — never blocking Git wins, and it is documented.)

### A2 — Migrate unsafe legacy hooks (#2)

portool ≤0.2 wrote `command -v portool … && portool sync --quiet`, which can
propagate failure. Today `install_into` *preserves* such lines verbatim
(there is a test asserting this — `init.rs:201-215` gets inverted). New
behavior: when a portool-managed hook contains an unsafe form, **rewrite the
portool lines** to the safe A1 form, in place, atomically.

- **Marker is a substring match, not precise ownership (Fable review #6).**
  The `portool sync` marker (`hooks.rs:11`, matched at `init.rs:106`) is a
  plain `contains` check, so a line the *user* hand-wrote that mentions
  `portool sync` would also be a rewrite candidate. Constrain the rewrite:
  only rewrite a line that matches the exact shape portool itself emits (the
  `command -v portool … portool sync --quiet` guard forms this and prior
  versions wrote); leave anything else untouched, and never rewrite lines
  under a foreign hook's non-portool logic.
- **`init` re-run is not the only trigger (Fable review #6).** Migration
  only fires when `init` runs. `warn_if_hook_missing` (`sync.rs:350-359`)
  keys off marker presence only, so a user who never re-runs `init` keeps
  the unsafe hook silently. Extend the sync-time hook check to detect the
  *unsafe legacy shape* and print a one-line hint: `portool: your
  post-checkout hook uses an old form that can fail 'git checkout'; run
  'portool init' to update it`.

### A3 — Interpreter-aware, non-destructive append (#5)

Before appending shell to an existing hook:

- **Shebang check.** Parse the first line. Append only when it is a POSIX
  shell (`/bin/sh`, `bash`, `dash`, `/usr/bin/env sh|bash|dash`), or when
  there is no shebang (Git executes such hooks via `sh`). For any other
  interpreter (Python, Node, Ruby, …) **do not append** — warn and print the
  manual one-liner, exactly like the `Missing` hooks-location case.
- **Reject symlinks.** If the hook path is a symlink, do not follow/rewrite;
  warn and skip.
- **Preserve permissions.** Only add the execute bit if missing; never
  downgrade a `0700` hook to `0755`. (Current code force-sets `0o755`.)
- **Atomic write.** Rewrites go through temp-file + rename, like the ledger
  and env-file writes.
- **Non-UTF-8 / unreadable hooks** remain left untouched (current behavior),
  but the executable bit is still not force-changed.

### A4 — Refuse shared-scope `hooksPath` auto-install (#4)

`HooksLocation` already classifies GitDefault / Husky / Custom / Missing.
Extend the `Custom` (absolute-dir) case: query `git config --show-scope
--get core.hooksPath`. If the scope is `global` or `system` (a machine-wide
shared hooks dir), **refuse to auto-write** — warn and print the manual
one-liner. Auto-install proceeds only for `local` / `worktree` scope and for
repo-relative paths (which are inherently per-repo).

### A5 — Also install `post-merge` (enhancement, Fable review item 7)

`post-checkout` misses `.portool.toml` changes arriving via `git pull` /
`git merge`. Since the safe A1 script is trivial, install the same guarded
`portool sync --quiet` into `post-merge` alongside `post-checkout`, through
the identical interpreter-aware/scope-aware install path (A3/A4). This
widens passive freshness to the common pull-based workflow at near-zero
cost. `deinit` (D5) removes both. Purely additive; does not change the
finding-#7 contract (explicit `sync`/`doctor` remains the guarantee for
edits Git hooks can't observe at all, e.g. a manual editor save).

---

## Batch B — Fail-closed & honesty

**Findings:** #8 (config fail-open), #9 (no ledger validation), #10 (`ls
--json` hides corruption), #15 (exit-code collision), #6 (over-claiming
docs, non-absolute XDG).

### B1 — Config fails closed (#8)

`Config::load` currently warns and returns defaults on read/parse failure.
Change to: **any read error (other than NotFound) or parse error is fatal
(exit 1)**. A missing file still means defaults (that is intentional, not a
failure). Add `#[serde(deny_unknown_fields)]` to `RawConfig` so a typo like
`ragne = …` is a hard error, not a silently-ignored field. `port 0` is
rejected in range validation (see D4).

**Deprecated-field interaction with C1 (Fable review, blocker #2).** C1
removes `subrange_size`. With `deny_unknown_fields`, an existing
`config.toml` still carrying `subrange_size = 500` would become a fatal
error the moment C lands — silently breaking every user who ever tuned it.
Resolution: keep a `subrange_size: Option<toml::Value>` (or `#[serde(rename
= "subrange_size")]` sink) field on `RawConfig` that is **accepted and
ignored with a one-line deprecation warning to stderr**, so `deny_unknown_fields`
still rejects true typos but a legacy config keeps working. Everything else
unknown is rejected. Document the deprecation in the README config section.

### B2 — Ledger schema validation & versioning (#9)

Add a validation pass in `store::load` after successful JSON parse:

- **Version.** `version` must equal the current schema version (**2** after
  batch C). An unknown/newer version is treated as an incompatible ledger:
  refuse to write, surface loudly. A recognized older version (`1`) is run
  through migration (batch C). A **read-only** caller (`ls`, `sync` fast
  path, `prune --dry-run`) that encounters a v1 ledger migrates it **in
  memory only** for display/comparison and never persists (Fable review
  #5) — persistence happens solely on the locked slow path.
- **Invariants.** `range.0 ≤ range.1`; every block `start ≤ end`; no port is
  `0` and every port fits `[1, 65535]`; **no two blocks overlap** across the
  whole ledger. A violation makes the ledger *corrupt* (same handling as
  unparseable: move-aside under lock, non-zero for read-only callers).
  - **NOT a hard invariant: block ⊆ `ledger.range` (Fable review, blocker
    #1).** `ledger.range` is frozen at ledger-creation time (frozen decision
    14, `sync.rs:146-150`) while allocation always uses the *current*
    `config.range`. So simply widening `config.range` after creation
    produces legitimate blocks outside `ledger.range`; treating that as
    corrupt would move the ledger aside and forget live allocations — a new
    instance of finding #11. Therefore `ledger.range` is **informational
    only** (as `registry.rs:12-13` already states) and is NOT a validation
    bound. Blocks are validated against the absolute `[1, 65535]` bound and
    against each other, never against the stored `range`. A block outside
    the *current* `config.range` is surfaced by `doctor` as an advisory, not
    treated as corruption.
- `#[serde(deny_unknown_fields)]` on the ledger structs so a downgrade can't
  silently drop fields it doesn't understand.

A tiny `migrate(value, from_version) -> Result<Registry>` seam is introduced
here and used by C.

### B3 — `ls` is honest about corruption (#10)

`cmd/ls.rs` currently discards `corrupt` / `read_error` and prints an empty
but valid-looking ledger with exit 0. Change: on `corrupt` or `read_error`,
**exit non-zero** (general error, 1) and, for `--json`, emit an explicit
error object (e.g. `{"error":"registry unreadable","detail":"…"}`) to stdout
so a machine consumer can distinguish "no allocations" from "could not
read". Human table mode prints the error to stderr.

### B4 — Separate clap usage errors from semantic codes (#15)

`main.rs` uses `Cli::parse()`, so clap usage errors exit `2` — colliding
with `SubrangeExhausted`. Switch to `Cli::try_parse()`; on a clap error,
print clap's message and exit with a dedicated usage code **`64`**
(`EX_USAGE`, sysexits), distinct from all portool semantic codes
(1/2/3/4/126/127). `--help` / `--version` still exit `0`.

### B5 — Honest docs & absolute XDG (#6)

- **README wording.** Replace "non-conflicting" / "across your machine" /
  "Fully passive" with accurate language: portool maintains a *cooperating
  ledger* that prevents overlap *among portool-managed blocks within one
  OS-user + XDG-state scope*; freshness comes from the hook plus explicit
  `sync`; real port availability is verified at `exec` time and via
  `doctor`. Document that separate users / `sudo` / differing
  `XDG_STATE_HOME` have separate ledgers.
- **XDG absolute-path rule.** `paths::xdg_dir` must ignore a relative
  `XDG_STATE_HOME` / `XDG_CONFIG_HOME` (per the XDG Base Directory spec) and
  must not degrade to `"."` when `HOME` is unset — fall back to the spec
  default only when it can be resolved to an absolute path, otherwise error
  clearly.
- Update `docs/spec-*.md` exit-code / claim references that this change
  invalidates (README is the source of truth for users).

---

## Batch C — Allocation model overhaul (schema v2, breaking)

**Findings:** #3 (500-port exclusive subranges → 14-repo exhaustion), #1
(the "non-conflicting" guarantee — existing blocks are never bind-rechecked
and there is no `reallocate`), #11 (corruption recovery forgets
allocations). (Finding #5 is *foreign-hook mangling*, handled in Batch A;
the earlier spec draft mislabeled the bind-recheck work as #5 — Fable review
completeness note.)

### C1 — Abolish per-project subranges (#3)

Today each project reserves a whole `subrange_size` (default 500) slice of
the pool on first sync, so the default pool exhausts after 14 repos
regardless of actual usage. New model:

- **Blocks are allocated directly from the whole pool.** Remove
  `ProjectEntry.subranges` and all subrange-acquisition logic. The occupied
  set for allocation is *every* worktree block across *all* projects plus all
  reservations, scanned over the entire `config.range`.
- **Hash becomes a preferred position, not a reservation.** For a worktree,
  compute a preferred start `= align_down(range.0 + FNV1a(project_id ++
  branch_or_path) % pool_width, block_align)`, then linear-scan forward
  (wrapping within the pool) for the first free **and** bindable
  `block_size`-wide block. `main`/`master` bias toward the project's base
  position so a project's worktrees still cluster, without any exclusive
  hold. This preserves the "main lives at the start, features nearby"
  ergonomics while letting far more than 14 projects coexist.
  - **Clamp to `range.0` (Fable review, minor).** When `range.0` is not a
    multiple of `block_align`, `align_down(...)` can land below `range.0`;
    the candidate start must be clamped up to `range.0` (the forward scan
    then proceeds from a valid in-pool position).
- `Config.subrange_size` is **removed** (breaking config change; documented).
  `find_free_subrange` / the subrange scan in `sync.rs` are deleted;
  `allocate_block` is simplified to operate on the single pool interval.
- **Exit code 2 (`SubrangeExhausted`) is retired**; only `PoolExhausted`
  (3) remains for "the pool is full". (2 is now free; B4 already moved clap
  off it.)

### C2 — Ledger schema v2 & migration

New shape (v2):

```json
{
  "version": 2,
  "range": [3000, 9999],
  "projects": {
    "<realpath common-dir>": {
      "name": "myapp",
      "worktrees": {
        "<realpath worktree root>": {
          "block": [3000, 3004],
          "branch": "main",
          "manifest_hash": "a1b2c3d4e5f6",
          "pinned": false,
          "label": null,
          "allocated_at": "…",
          "last_seen_at": "…"
        }
      }
    }
  },
  "reservations": []
}
```

- `ProjectEntry.subranges` is gone.
- **Migration v1 → v2:** read the v1 ledger (which has `subranges`), drop the
  `subranges` field, keep every worktree `block` verbatim — blocks are
  absolute, so **no ports move on upgrade**. Run v2 invariant validation
  (B2) on the result; overlaps that the old subrange model happened to
  prevent will pass, but a genuinely corrupt v1 is caught. Migration runs
  under the write lock (slow path / `doctor`), never from a read-only path.

### C3 — Bind-recheck at the execution boundary (#1)

The everyday `sync` fast path stays bind-check-free (speed / passivity). The
real check moves to where ports are actually used:

- **`portool exec`** already runs `sync` before launching. After sync,
  **bind-recheck the allocated block**. On any port in the block being
  in use: print a stderr advisory naming the block and the port(s), then
  proceed (the user's other config may already reference these numbers —
  silently changing them at exec time is worse).
- **Own-process false positives are expected (Fable review, blocker #3).**
  The bind check (`ports.rs:10-17`) cannot tell a *foreign* process from
  *this worktree's own* running server. The everyday pattern — terminal 1
  runs `portool exec -- npm run dev` (binding `WEB_PORT`), terminal 2 runs
  `portool exec -- npm test` in the same worktree — will re-flag the block
  every time. So the advisory wording must be **neutral**, e.g. `portool:
  ports 3000-3004 are in use — this may be this worktree's own running
  processes`, and must NOT imply a misallocation. `--reallocate-on-conflict`
  is therefore **off by default and documented as a footgun**: reallocating
  away from a port your own live server still holds leaves that server on
  the old block while new processes get the new one. `--strict` (exit 1,
  command not started) is likewise opt-in for callers that genuinely want a
  hard gate.
- **Bind check is IPv4/`127.0.0.1`-only** (`ports.rs:11`): a server bound
  only to `::1` is not detected. This is a pre-existing limitation; note it
  in the `doctor`/README wording rather than expanding scope here.
- **`portool reallocate`** (new): force the current worktree onto a fresh
  free+bindable block, excluding its current block, and rewrite
  `.env.portool`. This is the explicit escape hatch the review asked for
  (subject to the same own-process caveat above).

### C4 — Corruption recovery that can rebuild (#11)

A corrupt ledger is still moved aside, but recovery no longer silently
forgets live allocations:

- Read-only callers (`ls`, `sync` fast path, `prune --dry-run`) surface the
  corruption (B3) and, for `ls`, exit non-zero — they never rebuild.
- **`portool doctor`** (new, batch D) provides the rebuild path: from `git
  worktree list` it enumerates the current project's worktrees, reads each
  existing `.env.portool`'s recorded block, and re-imports those blocks into
  the ledger under the lock — reconstructing state a blind empty-restart
  would have dropped. `doctor` also reports blocks whose ports are currently
  occupied and any schema-invariant issues.
- **Rebuild is per-project (Fable review item 8).** The move-aside is global
  (one `registry.json`), but `doctor` reconstructs only the project it runs
  in — other projects' entries stay dropped until `doctor` is run in each.
  The moved-aside `registry.json.corrupt-<ts>` file is therefore the
  authoritative manual-recovery artifact; `doctor` must print its path and
  never delete it, so a user can reconcile projects it didn't rebuild.

---

## Batch D — Robustness & tooling

**Findings:** #12 (`-z` porcelain), #13 (env-file injection), #14
(`branch`/`last_seen_at` staleness), #16 (overflow / port 0), plus the P1
subcommands and P2 tests.

### D1 — Safe `git worktree list` parsing (#12)

Switch `worktree_list_at` to `git worktree list --porcelain -z` and split on
NUL, so worktree paths containing newlines are handled correctly. Where a
path is legitimately non-UTF-8, preserve it via `OsString`/bytes rather than
lossily forcing `String`. `run_git` failures should carry git's stderr into
the error message instead of collapsing everything to `None`, at least on
the paths where a diagnostic matters (discovery, worktree list).

- **`-z` requires git ≥ 2.36 (Fable review item 4).** `git worktree list
  --porcelain -z` landed in Git 2.36. Declare the minimum git version in the
  README (alongside the A4 `--show-scope` ≥ 2.26 requirement — effective
  floor becomes 2.36), and on `-z` failure fall back to the newline-split
  parse rather than erroring the whole slow path.
- **Non-UTF-8 support is partial (Fable review, minor).** Ledger keys are
  `String` (`sync.rs:367-369` canonicalizes via `to_string_lossy`), so full
  non-UTF-8 fidelity ends at the parsing layer. Scope this in the spec: D1
  fixes *parsing/enumeration* robustness; end-to-end non-UTF-8 keying is out
  of scope and noted as a known limitation.

### D2 — Escape variable paths in `.env.portool` (#13)

The generated comment embeds the raw project name and worktree path; a
newline/control char in the path can escape the comment and inject a
shell-source-able line (the README recommends `source .env.portool`).
Sanitize: strip or backslash-escape newlines and control characters in the
comment line, guaranteeing the generated file is always safe to `source`.
Add a regression test with a newline-bearing path.

### D3 — Correct `branch` / `last_seen_at` semantics (#14)

The fast path is a full no-op when the ledger entry, manifest hash, and env
bytes all match — so a plain branch checkout leaves `branch` stale (the env
file doesn't encode the branch) and `last_seen_at` only advances when the
slow path runs. Fix: either (a) the fast path, on a cheap branch/`last_seen`
mismatch, falls through to a lightweight locked metadata refresh, or (b) the
fields are renamed/redocumented to their true meaning. Preference: **(a)** —
make `branch` reflect the current branch and `last_seen_at` a real
last-touched timestamp, since `doctor`/future GC will rely on them. Keep the
common no-op fast when branch *and* timestamp are already current.

- **Refresh granularity is one day (Fable review item 7).** Treating
  `last_seen_at` with second precision would make every checkout take the
  lock and destroy the fast path. The fast path falls through only when the
  branch differs OR `last_seen_at` is on an earlier calendar day than today
  (local tz). Day granularity is ample against `gc_days = 30`, and keeps the
  same-day common case lock-free.

### D4 — Boundary correctness (#16)

- Env-var port math (`block.0 + offset`) uses checked arithmetic; an offset
  that would exceed the block or overflow `u16` is a manifest error, not a
  wraparound.
- `config.range` rejects `0` on either bound (a port of 0 is never a real
  allocation; `bind(…,0)` picks an ephemeral port and must not be read as
  "0 is free").
- Guard the block-size clamp so a `u16::MAX` truncation can't yield an
  out-of-pool block.

### D5 — New operational subcommands (P1)

| Command | Purpose |
|---|---|
| `portool doctor` | Diagnose & repair: report blocks whose ports are occupied, schema-invariant issues, stale entries; rebuild ledger entries from live worktrees' `.env.portool` (C4). **Before re-importing a block it must verify the block does not overlap an already-imported one; on overlap, report and skip rather than re-import** — otherwise a corruption caused by a real bug that baked an overlap into `.env.portool` would be re-produced by a blind rebuild (Fable final note). |
| `portool check` | Validate the ledger and config; exit non-zero on any problem. Read-only, script-friendly. |
| `portool release` | Free the current worktree's block from the ledger (and remove its `.env.portool`). |
| `portool deinit` | Reverse `init`: remove portool's hook lines (idempotent, interpreter-aware) and the `.gitignore` entry. |
| `portool reallocate` | (from C3) force a fresh block for the current worktree. |

All new commands share the existing lock discipline and exit-code contract.

### D6 — Test hardening (P2)

Add coverage for the failure modes the current suite omits:

- Hook exits 0 even when `sync` fails; legacy-hook migration; append onto
  Python/Node hooks (skipped) and sh hooks (appended); symlinked hook;
  preserved perms.
- `post-merge` install is idempotent and symmetric with `post-checkout`;
  `deinit` removes portool's lines from **both** hooks (Fable final note —
  the current D6 draft is post-checkout-centric).
- Shared-scope `hooksPath` refusal.
- Config typo rejected; config parse error is fatal.
- Semantically-broken ledger (overlaps, bad version, port 0) treated as
  corrupt; `ls --json` non-zero on corruption.
- v1 → v2 migration preserves blocks.
- `exec` bind-recheck warns / `--strict` fails / `--reallocate-on-conflict`
  moves.
- `doctor` rebuild from `.env.portool`.
- Newline-bearing and non-UTF-8 worktree paths through `worktree_list` and
  `.env.portool`.
- 15+ concurrent processes preserve ledger validity and block
  non-overlap (extends the existing 8-process test).
- **Package `tests/` in the published crate:** widen `Cargo.toml`
  `include` so the integration tests ship with the crate (currently only
  `src/**/*` + README + LICENSE are included).

---

## Cross-cutting: exit-code contract (post-change)

| Code | Meaning |
|---|---|
| 0 | Success (including a no-op sync). |
| 1 | General error — outside a git repo, malformed `.portool.toml`/`config.toml`, unreadable/corrupt ledger, I/O failure. |
| 3 | Pool exhausted — no room for a block anywhere in the range. |
| 4 | Registry lock timeout (10s). |
| 64 | CLI usage error (clap). |
| 126 | `exec`: command found but not executable. |
| 127 | `exec`: command not found. |

Code **2 is retired** (`SubrangeExhausted` removed with the subrange model).

## Edge cases & risks

- **Foreign hook exit semantics (A1).** Appending `|| true` last means a
  foreign hook that relied on its final command's status now exits 0.
  Documented; never-block-Git takes priority.
- **A4 scope detection depends on `git config --show-scope`** (git ≥ 2.26).
  If unavailable, fall back to the conservative behavior: treat an absolute
  `Custom` dir outside the repo as shared and refuse. Note MSRV/git-version
  assumptions.
- **C1 preferred-position clustering is weaker than exclusive subranges.**
  Two projects may interleave in the pool. That is the intended trade to
  kill 14-repo exhaustion; `ls`/`doctor` remain the diagnostic.
- **C2 migration on a v1 ledger with pre-existing overlaps** (only possible
  if hand-edited): validation flags it corrupt → move-aside → `doctor`
  rebuild. No data-loss beyond what a corrupt file already implies.
- **C3 exec bind-recheck adds a bind sweep per exec.** Bounded by block size
  (≤ tens of ports); acceptable at an execution boundary, unlike per
  checkout.
- **D3 metadata refresh must not reintroduce a lock on every checkout.**
  Only fall through when branch/timestamp actually differ; the true no-op
  stays lock-free.
- **Schema v2 is a hard break for anyone reading `registry.json`
  directly.** Acceptable per the explicit go-ahead; `version` gates it.

## Testing strategy

TDD per change: a failing test that encodes the finding, then the fix.
Pure logic (alloc, config, registry validation, envfile escaping, hook-line
migration, porcelain parsing) is unit-tested; hook installation, `ls`
corruption exit codes, `exec` bind-recheck, migration, and concurrency are
integration-tested (`tests/`). `cargo fmt --check`, `cargo clippy
--all-targets -D warnings`, and `cargo test` gate every PR (CI already runs
these on Ubuntu + macOS).

## Sequencing (one PR per batch, default branch)

1. **A — Hook safety.** Self-contained; highest safety impact; no schema
   change. Lands first.
2. **B — Fail-closed & honesty** (validation seam + honest docs), landing
   the migration/validation seam that C fills in.
3. **C — Allocation overhaul & schema v2.** The breaking core change;
   depends on B's validation/migration seam.
4. **D — Robustness & tooling.** Small independent fixes + new commands +
   P2 tests; can be split further if a single PR grows too large.

## Implementation deviations (reconciled post-merge)

The shipped implementation departs from this spec's letter in three
correctness-neutral ways, confirmed by the post-merge implementation review:

- **C1 preferred position drops `project_id` from the hash.** `preferred_slot`
  (`src/alloc.rs`) uses `main`/`master` → pool start (slot 0), other branches
  → `FNV1a(branch) % slots` (detached → path), rather than the spec's
  `FNV1a(project_id ++ key)` base. This keeps the first project's `main` at
  the pool start (predictable, matching the test suite) and is simpler;
  allocation correctness, non-overlap, and pool-exhaustion behavior are
  unaffected (forward scan + overlap check resolve all collisions). The
  trade-off is weaker per-project clustering (all `main` worktrees prefer the
  low end). **The README's "How it works" describes the shipped behavior**;
  this spec section is the record of the divergence.
- **D4 env-var port math saturates instead of erroring.**
  `block.0.saturating_add(offset)` (`src/envfile.rs`) prevents a release-build
  wraparound; the "manifest error" path the spec described is unreachable in
  practice (a block is always sized to fit its manifest and validated to fit
  the pool), so saturation is defense-in-depth rather than a user-facing
  error.
- **D6 concurrency test lands at 16 processes** (raised from 8 in a follow-up)
  — satisfying the "15+" target.

## Out of scope (this engagement)

- Cross-user / system-wide (multi-`$HOME`) coordination — portool remains
  per-XDG-scope; docs state this rather than pretending otherwise.
- A daemon or filesystem watcher for `.portool.toml` edits not seen by
  `post-checkout` — `exec` + explicit `sync`/`doctor` remain the contract.
- Windows support (unchanged: macOS/Linux only).
- `pin`/`unpin` commands (ledger has room; not part of this hardening pass).
