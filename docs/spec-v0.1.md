# portool 仕様書 v0.1

> **注意（2026-07-17 追記）**: この文書は v0.1 設計時のスナップショットであり、
> **現行の実装とは一部乖離している**。ハードニングリリースで割当モデルが変更され、
> プロジェクト単位の `subranges` は廃止（台帳 schema v2）、ブロックはプールから直接割当、
> exit code 2 は廃止された。現行の挙動は [`README.md`](../README.md) と
> [`docs/superpowers/specs/2026-07-17-portool-hardening-design.md`](superpowers/specs/2026-07-17-portool-hardening-design.md) を参照。


> 名称 "portool" は仮。git worktree × ポート割り当ての台帳ツール。
> 本書は実装着手可能なレベルで v0.1 の仕様を凍結する。

---

## 1. 設計憲法

portool は次の2軸のみで勝負する。この軸から外れる機能は追加しない。

1. **グローバル台帳** — マシン全体・複数プロジェクト横断で単一のポート空間を調停する
2. **完全パッシブ** — ワークフローを乗っ取らない。素の `git worktree add` でも、agent が勝手に worktree を切っても、フック経由で自動的に正しい状態になる

### Non-goals(明示的にやらないこと)

- プロセス管理・起動・監視(スコープ外)
- リバースプロキシ / サブドメインルーティング
- TUI ダッシュボード
- worktree 作成のラッパー(`portool add` のようなコマンドは作らない)
- env の合成テンプレート(`derived`)— v0.2 以降で検討
- DB / compose プロジェクトの隔離(スコープ外)

---

## 2. 用語

| 用語 | 定義 |
|---|---|
| プロジェクト | ひとつの git リポジトリ。`git rev-parse --git-common-dir` の正規化(realpath)結果をキーとして識別する |
| worktree | main worktree / linked worktree を区別しない。worktree ルートの正規化パスをキーとする |
| マニフェスト | リポジトリにコミットされる `.portool.toml`。必要ポートの宣言 |
| ブロック | worktree に割り当てられる連続ポート区間 `[start, end]`(両端含む) |
| サブレンジ | プロジェクトに割り当てられるポート区間。ブロックはサブレンジ内から切り出す |
| プール | portool が管理するポート全域。デフォルト `[3000, 9999]` |
| 台帳 | 全割り当てを記録する単一 JSON ファイル |

---

## 3. ファイル配置

```
${XDG_STATE_HOME:-~/.local/state}/portool/registry.json       # 台帳
${XDG_STATE_HOME:-~/.local/state}/portool/registry.json.lock  # flock 用ロックファイル
${XDG_CONFIG_HOME:-~/.config}/portool/config.toml             # グローバル設定(任意)
<repo>/.portool.toml                                          # マニフェスト(コミットされる)
<worktree>/.env.portool                                       # 生成物(gitignore される)
```

### 3.1 グローバル設定 `config.toml`(すべて任意)

```toml
range = [3000, 9999]      # プール
subrange_size = 500       # プロジェクト初回確保時のサブレンジ幅
block_align = 5           # ブロックサイズの切り上げ単位 兼 最小サイズ
gc_days = 30              # クロスプロジェクト GC の last_seen 閾値(日)
```

---

## 4. マニフェスト `.portool.toml`

```toml
[ports]
web = 0    # ブロック内オフセット
api = 1
hmr = 2
db  = 3
```

- キーは `[a-z][a-z0-9_]*`。環境変数名は `大文字化 + "_PORT"` で導出する(`web` → `WEB_PORT`)
- 値はブロック内オフセット。重複はエラー。歯抜け(0,1,3)は許容
- **ブロックサイズ** = `max(最大オフセット + 1, 宣言数)` を `block_align` の倍数に切り上げ
  - 例: 宣言4個・最大オフセット3 → 4 → 切り上げて **5**
- マニフェストが存在しない場合、デフォルトブロック(サイズ = `block_align`、オフセット0のみ `PORT` として出力)にフォールバックする(段階的導入パス)
- `manifest_hash` = `.portool.toml` のバイト列の SHA-256 先頭12桁hex。不存在なら `null`

---

## 5. 台帳スキーマ `registry.json`

```jsonc
{
  "version": 1,
  "range": [3000, 9999],
  "projects": {
    // key = realpath(git rev-parse --git-common-dir)
    "/home/user/dev/myapp/.git": {
      "name": "myapp",                   // 表示用。common-dir 親ディレクトリ名から自動推定
      "subranges": [[3000, 3499]],       // 追加確保可能な配列。枯渇時に末尾へ追加
      "worktrees": {
        // key = realpath(worktree root)
        "/home/user/dev/myapp": {
          "block": [3000, 3004],
          "branch": "main",              // 参考情報。detached HEAD は null
          "manifest_hash": "a1b2c3d4e5f6",
          "pinned": false,
          "label": null,
          "allocated_at": "2026-07-15T10:00:00+09:00",
          "last_seen_at": "2026-07-15T12:00:00+09:00"
        }
      }
    }
  },
  "reservations": [
    // worktree 非依存の予約。v0.1 ではスキーマのみ定義し、書き込む CLI は提供しない
    // { "block": [5000, 5009], "label": "postgres-dev", "pinned": true }
  ]
}
```

### 設計判断(凍結)

- worktree の同一性キーは**パス**。branch は rename / detached があるため参考情報に留める
- `pinned` / `reservations` は v0.1 スキーマに含めるが、操作コマンド(`pin`/`unpin`)は提供しない
- 台帳は**単一ファイル + flock**。プロジェクト別分割はしない
- 台帳が存在しない・壊れている(JSON parse 失敗)場合は空台帳として再生成し、warning を stderr に出す。破損した旧ファイルは `registry.json.corrupt-<timestamp>` に退避する

---

## 6. 割り当てアルゴリズム

### 6.1 プロジェクト識別

```
common_dir = realpath($(git -C <cwd> rev-parse --git-common-dir))
```

main / linked worktree のどちらから実行しても同一プロジェクトに解決される。クローンが複数あれば別プロジェクト。

### 6.2 サブレンジ確保(プロジェクト初回)

プール内を先頭から走査し、既存の全 subranges / reservations と重ならない最初の `subrange_size` 幅の区間を確保する。プールに `subrange_size` 幅の空きがない場合は exit code 3(pool exhausted)。

### 6.3 ブロック割り当て

```
slots        = floor(subrange_width / block_size)          # ブロックスロット数
preferred    = branch が main/master → 0
               それ以外 → FNV-1a-32(branch ?? worktree_path) % slots
探索順       = preferred, preferred+1, ..., (mod slots で一周)
```

各候補スロットについて:

1. 台帳上で他エントリ・reservations と重複しないこと
2. ブロック内の全ポートについて `127.0.0.1` への TCP bind が成功すること(台帳外プロセス対策。bind 失敗したスロットはスキップ)

を満たす最初のスロットを確保する。サブレンジ内の全スロットが埋まっている場合は 6.2 に従い追加サブレンジを確保して再試行する。

- **規約**: main/master がスロット0を優先するため、各プロジェクトの「サブレンジ先頭 = main」が慣習として成立する
- bind チェックは TOCTOU を完全には防げない。割り当て後の衝突は実行時エラーに委ね、`portool ls` で診断できれば十分とする

### 6.4 ブロック再割り当て(マニフェスト変更)

`manifest_hash` 不一致を検知した場合:

- 新ブロックサイズが現ブロック内に収まる → ブロック据え置き、hash と `.env.portool` のみ更新
- 収まらない → 現ブロックを解放し 6.3 で再割り当て(worktree 単位で影響が閉じる)

---

## 7. `sync` の状態遷移(ホットパス設計)

`portool sync` は post-checkout から毎回呼ばれるため、最頻ケースを数msで返す。

```
[fast path — ロックなし・read-only]
1. common_dir / worktree_path を解決
2. 台帳を read-only で open・parse(失敗 → slow path)
3. 自エントリが存在し、
   manifest_hash 一致 かつ .env.portool の内容が期待値と一致
   → last_seen_at も更新せず exit 0        # 完全 no-op

[slow path — flock(排他)]
4. flock 取得(タイムアウト 10s → exit 4)
5. 台帳再読込(fast path 以降の変更を取り込む)
6. 必要に応じて: サブレンジ確保 → ブロック割り当て/再割り当て
7. 自プロジェクト内 GC(§8.1)を実行
8. last_seen_at 更新、台帳を atomic write(temp + rename)
9. .env.portool を atomic write
10. flock 解放、exit 0
```

- `last_seen_at` の更新は slow path でのみ行う(fast path を純粋な read に保つため)。精度は gc_days=30 に対して十分
- 台帳書き込みは必ず temp ファイル + rename。中途半端な JSON を残さない

### 7.1 `.env.portool` 生成形式

```dotenv
# generated by portool — DO NOT EDIT
# block: 3000-3004  project: myapp  worktree: /home/user/dev/myapp
WEB_PORT=3000
API_PORT=3001
HMR_PORT=3002
DB_PORT=3003
```

- マニフェスト不在時は `PORT=<block_start>` の1行のみ
- 読み込みは利用者側の責務: direnv なら `.envrc` に `dotenv .env.portool`、compose なら `--env-file .env.portool`、Vite 等はアプリ側で `process.env.X_PORT || fallback`

---

## 8. GC

### 8.1 暗黙 GC(sync の slow path 内・自プロジェクトのみ)

自プロジェクトの各エントリについて、以下を**すべて**満たすものを回収する:

1. `pinned == false`
2. worktree パスが `git worktree list --porcelain` に存在しない、かつディレクトリも存在しない
3. ブロック内の全ポートが listen されていない(bind チェック)

### 8.2 明示 GC `portool prune`

- `portool prune` — カレントプロジェクトに対して 8.1 と同条件で実行
- `portool prune --all` — 全プロジェクト横断。各プロジェクトの common_dir が存在すればそのプロジェクトの `git worktree list` で 8.1 を適用。common_dir 自体が消えていれば(リポジトリごと削除)、ポート未listenを確認の上プロジェクトエントリごと回収
- クロスプロジェクトの自動回収(sync 中に他プロジェクトを触る)は **v0.1 ではやらない**。`last_seen_at` と `gc_days` はスキーマに保持し、v0.2 で「last_seen が gc_days 超過 かつ 未listen なら回収候補として警告」から段階導入する

### 8.3 サブレンジの回収

プロジェクトの worktrees が空になってもサブレンジは保持する(再作成時の再現性優先)。`prune --all` でプロジェクトエントリごと回収された場合のみサブレンジも解放される。

---

## 9. CLI 仕様

```
portool init [--hook-only|--gitignore-only]
portool sync [--quiet]
portool ls   [--json] [--all]
portool prune [--all] [--dry-run]
```

### 9.1 `init`

1. `.git/hooks/post-checkout` を設置(§10)。既存フックがあれば portool 呼び出し行を追記(冪等: 既に含まれていれば何もしない)
2. `.gitignore` に `.env.portool` を追記(冪等)
3. 直後に `sync` を一回実行

### 9.2 `sync`

§7 のとおり。`--quiet` は正常時の出力を抑制(フック用)。警告・エラーは stderr。

- フック未設置を検知した場合、stderr に一行警告: `hint: run 'portool init' to install the post-checkout hook`

### 9.3 `ls`

デフォルトはカレントプロジェクトのみ、`--all` で全プロジェクト。

```
PROJECT  WORKTREE                          BRANCH        BLOCK      STATUS
myapp    ~/dev/myapp                       main          3000-3004  active
myapp    ~/dev/myapp-wt/feat-api           feat/api-v2   3005-3009  active
blog     ~/dev/blog                        main          3500-3504  stale?
```

- `STATUS`: `active`(worktree 存在) / `stale?`(worktree 不在 = 次回 GC 候補) / `pinned`
- `--json` は台帳スキーマに準じた機械可読出力。agent はこれを読む(MCP 化はこの上の薄皮として将来対応可能)

### 9.4 exit codes

| code | 意味 |
|---|---|
| 0 | 成功(no-op 含む) |
| 1 | 一般エラー(git リポジトリ外、マニフェスト parse 失敗等) |
| 2 | サブレンジ内枯渇から回復不能(追加確保も失敗) |
| 3 | プール枯渇 |
| 4 | flock タイムアウト |

---

## 10. フック

### 10.1 git post-checkout(init が設置)

```sh
#!/bin/sh
# installed by portool
command -v portool >/dev/null 2>&1 && portool sync --quiet
```

- common dir の hooks は全 worktree で共有されるため、一度の設置で全 worktree に効く
- `git worktree add` は内部で checkout を行うため、worktree 作成時にも発火する
- `command -v` ガードにより portool 未導入環境でもリポジトリは壊れない

### 10.2 Claude Code(推奨設定・ドキュメントで案内)

SessionStart フックに `portool sync --quiet` を一発。フックを迂回した worktree 作成や台帳の乖離に対する自己修復として機能する(冪等なので重複コストなし)。

### 10.3 direnv(推奨設定・ドキュメントで案内)

```sh
# .envrc
dotenv_if_exists .env.portool
```

`.envrc` から `portool` 自体は呼ばない(cd のたびに台帳アクセスさせない)。

---

## 11. 並行性

- 台帳変更は必ず `registry.json.lock` への flock(排他)下で行う。read-only の fast path はロックを取らない
- 想定並行度: 15+ の agent が同時に worktree 作成/checkout。ロック競合が起きるのは割り当て変更時のみで、保持時間は数十ms想定
- flock はブロッキング + 10s タイムアウト

---

## 12. エッジケース(凍結事項)

| ケース | 挙動 |
|---|---|
| detached HEAD | `branch: null`。ハッシュ初期値は worktree パスで計算 |
| ブランチ rename | エントリはパスキーなので影響なし。branch フィールドは次回 slow path で更新 |
| 同一ブランチの worktree 作り直し | ハッシュ初期値が同じため、空いていれば同一ブロックに戻る(ベストエフォート) |
| worktree の手動 `mv` | 旧パスのエントリは GC 対象、新パスで新規割り当て(= ポートが変わりうる)。v0.1 では追跡しない |
| `.portool.toml` の worktree ローカル変更(未コミット) | その worktree の manifest_hash だけが変わる。仕様どおり再割り当て判定。警告等は出さない |
| プール/サブレンジ設定の変更(config.toml) | 既存割り当ては尊重し、新規確保からのみ新設定を適用 |
| Windows | v0.1 は対象外(macOS / Linux のみ)。flock・realpath 依存のため |

---

## 13. 実装

- **言語: Rust**(単一バイナリ・起動数ms・flock/bind が素直)
- 外部プロセス呼び出しは `git rev-parse --git-common-dir` / `git worktree list --porcelain` の2種のみ。git2 クレートは使わない(バイナリサイズと挙動一致のため CLI 呼び出しを正とする)
- 主要クレート想定: `serde`/`serde_json`, `toml`, `fs2`(flock), `clap`
- テスト方針: 台帳操作はすべて純関数(現状 + 要求 → 新台帳 + 生成物)に寄せ、I/O 層を薄く分離。並行テストは flock を実プロセスで叩く統合テストを最低1本

## 14. v0.1 スコープ外(将来)

- `pin` / `unpin` コマンド、reservations の CLI 操作
- `derived` テンプレート(単純 `${name}` 置換に限定して v0.2 検討)
- クロスプロジェクト自動 GC(last_seen ベース)
- MCP サーバー(`ls --json` の薄皮)
- Windows 対応
- worktree `mv` の追跡
