# core.hooksPath (Husky) 対応設計

日付: 2026-07-16
対象 issue: Support repositories that use core.hooksPath (e.g. Husky)

## 問題

`portool init` は無条件に `<git-common-dir>/hooks/post-checkout` へ hook を
インストールする。`core.hooksPath`（Husky 等が設定）があるリポジトリでは git は
そのディレクトリを参照しないため、hook は一切実行されず、しかも init は黙って
成功する。`portool sync` の `warn_if_hook_missing` も同じパスしか見ないため、
hook manager 利用者には誤った hint が出続ける。

## 方針（承認済み: 案A）

実効 hooks ディレクトリを検出し、hook manager と「奪い合わずに」統合する。
`core.hooksPath` の書き換え（chain-runner 方式）は行わない。Husky v9 の
`prepare` が npm install のたびに `core.hooksPath=.husky/_` を再設定するため、
同じ設定ノブを取り合う構造は idempotent な共存という要件と両立しないため。

## 検出ロジック（新モジュール `src/hooks.rs`）

`git config --type=path --get core.hooksPath` を worktree root で実行する
（`--type=path` により `~` は展開済み。相対パスは worktree root 基準で解決 —
githooks(5) の「hooks は working tree の top-level で実行される」に一致）。

分類（`enum HooksLocation`）:

| 状態 | 分類 | post-checkout の場所 |
|------|------|---------------------|
| 未設定 / 空文字 | `GitDefault` | `<common_dir>/hooks/post-checkout`（現行どおり） |
| 末尾 2 要素が `.husky/_` | `Husky` | `<...>/.husky/post-checkout`（ユーザー管理側） |
| その他でディレクトリ実在 | `Custom` | `<hooksPath>/post-checkout` |
| その他でディレクトリ不在 | `Missing` | 自動インストール不可 |

## `portool init` の挙動

- `GitDefault` / `Custom`: 対象ファイルへ idempotent に作成 or 追記し、0o755 を保証（現行ロジックをファイルパスでパラメータ化して共用）。
- `Husky`: `.husky/post-checkout` へ同様に作成 or 追記。`.husky/_`（生成物）と
  `<common_dir>/hooks` には一切触れない。これは Husky 公式の hook 追加方法で、
  `.husky/_/h` bootstrap（`HUSKY=0` / `~/.config/husky/init.sh` / PATH 追加 /
  `sh -e` / exit code 伝播）を迂回しない。加えて stderr に以下を案内する:
  - `.husky/post-checkout` は tracked file なのでコミットして共有すること
  - brand-new worktree では `.husky/_` が未生成のため初回 post-checkout では
    hook 自体が発火しない（fresh clone で Husky hook が効かないのと同じ性質）。
    新規 worktree で一度 `portool sync` を実行するか、package.json の
    `prepare` に `portool sync --quiet` を追加することを推奨
- `Missing`: 使われない `<common_dir>/hooks` へ黙ってインストールせず、
  設定値・解決済みパス・具体的な対処手順（ディレクトリ生成後に
  `portool init --hook-only` を再実行、または hook manager 側の post-checkout に
  1 行追加）を stderr に警告して正常終了する。

## hook スクリプトの内容変更

portool 未インストール環境で hook が非 0 で終了しないよう、`&&` 連結から
`if` ガードへ統一する（`sh -e` や exit code を伝播する hook manager 配下でも
無害になる）:

```sh
#!/bin/sh
# installed by portool
if command -v portool >/dev/null 2>&1; then
  portool sync --quiet
fi
```

既存 hook への追記行も同形の 1 行 `if ...; then ...; fi` とする。
idempotency 判定は従来どおり `portool sync` マーカー部分文字列で行うため、
旧形式（`&&`）でインストール済みの hook も重複追記されない。

## `portool sync` の warn 修正

`warn_if_hook_missing` は `HooksLocation` の解決結果に基づき、実効的な
post-checkout ファイルのマーカー有無を確認する。`Missing` はマーカー無し扱い
（init が詳細な警告を出す）。

## エッジケース

1. `core.hooksPath` 未設定 → 完全に現行挙動
2. 空文字設定 → 未設定と同一視
3. `~` を含む値 → `--type=path` で展開
4. 相対パス → worktree root 基準で解決（worktree ごとに異なる実体を指す）
5. 既存 post-checkout（マーカー無し）→ 末尾に追記、既存内容保持
6. マーカー有り → no-op（init 複数回実行で重複しない）
7. 非 UTF-8 の既存 hook → 触らない（現行踏襲）
8. portool 未インストールの worktree で hook 発火 → exit 0
9. Husky brand-new worktree 初回 post-checkout → 原理的に発火不可
   （`.husky/_` 未生成）。init 時の案内で緩和
10. `.husky/_` が存在しなくても config が Husky を指せば Husky 扱い
    （`.husky/` 自体は tracked なので通常存在する。無ければ作成）

## テスト計画

- `src/hooks.rs` 単体: 分類ロジック（未設定 / husky / custom 実在 / 不在 / 空文字 / 相対解決）
- `src/cmd/init.rs` 単体: パラメータ化した install の作成・追記・idempotency
- `tests/cli.rs` E2E:
  - 通常 repo: init → `git worktree add` の初回 post-checkout で
    `.env.portool` 生成（PATH に portool を入れ、隔離 env で hook 実行）
  - custom hooksPath repo: 実在 dir へインストール、`<common_dir>/hooks` は無傷、idempotent
  - Husky 風 repo: `.husky/_/post-checkout` 委譲 shim を用意し、
    `.husky/post-checkout` 経由で checkout 時に sync が走ること
  - hooksPath 不在 dir: 警告が出て、どこにもインストールされないこと
  - 既存 post-checkout との共存（追記・保持・重複なし）
