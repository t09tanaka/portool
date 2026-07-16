# webapp — a portool-free project layout

A minimal web project laid out so that **portool stays optional**: the
same files and the same npm scripts work on a machine with portool (every
worktree gets its own ports) and on a machine without it (CI, a fresh
laptop — the defaults apply). Nothing in this directory calls, detects,
or requires portool.

Three rules, applied everywhere:

1. **Scripts never mention portool.** `package.json`, `compose.yaml`, and
   `server.js` are all portool-free. The caller wraps a command from the
   outside, once, where portool is available.
2. **Every port reference carries a fallback.** `${DB_PORT:-5432}` in
   compose, `${TEST_DB_PORT:-5433}` in `.env.test`,
   `process.env.WEB_PORT ?? 3000` in code.
3. **The unwrapped commands are the contract.** Everything below must
   keep working without portool; if it doesn't, fix the fallback rather
   than adding a portool dependency.

## The files

| File | Role |
| --- | --- |
| [`.portool.toml`](.portool.toml) | Declares `web`, `db`, `test_db` (offsets 0–2). The only portool-specific file — inert without portool. |
| [`server.js`](server.js) | Reads `process.env.WEB_PORT ?? 3000` — the code-level fallback. |
| [`compose.yaml`](compose.yaml) | `${DB_PORT:-5432}`-style fallbacks; project name namespaced by `${PORTOOL_WORKTREE_ID:-default}`. |
| [`.env.test`](.env.test) | Committed test env. `DATABASE_URL` embeds `${TEST_DB_PORT:-5433}`, so one file serves both worlds. |
| [`package.json`](package.json) | npm scripts; none reference portool. |
| [`.gitignore`](.gitignore) | Ignores the generated `.env.portool` (what `portool init` would add). |

## With portool

Wrap each command once with [`portool exec`](../../README.md#portool-exec);
the worktree's allocation is injected and `${…}` references in env files
expand against it:

```sh
portool exec -- npm run dev                    # WEB_PORT = this worktree's block
portool exec -- npm run db:up                  # DB_PORT / TEST_DB_PORT + namespaced compose project
portool exec -e .env.test -- npm run test:int  # DATABASE_URL hits this worktree's test_db port
```

Run the same commands in a second worktree of the same clone and every
port (and the compose project name) differs — nothing collides.

## Without portool (CI, fresh laptop)

Run the exact same scripts unwrapped; every fallback default applies:

```sh
npm run dev        # listens on 3000
npm run db:up      # db on 5432, test-db on 5433, compose project "webapp-default"
npm run test:int   # DATABASE_URL falls back to 127.0.0.1:5433/app_test
```

No install step, no stub, no `if command -v portool` anywhere.

## What you'd add in a real repo

- Developers who use portool run `portool init` once in their clone. The
  installed `post-checkout` hook lives under `.git` and is guarded by
  `command -v portool`, so clones and teammates without portool are never
  affected. (Under Husky the hook file is tracked instead — still a no-op
  without portool.)
- If your test runner loads `.env.test` through dotenv-expand (Next.js,
  dotenv-cli, …), the `${TEST_DB_PORT:-5433}` default resolves there too —
  the same committed file keeps serving CI.
