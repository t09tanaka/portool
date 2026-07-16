# `portool exec` 実装計画（v0.4.0 / docs/spec-v0.4.md）

ユーザー承認済みの仕様 `docs/spec-v0.4.md` を実装に落とす計画。

## モジュール構成

| ファイル | 役割 |
|---------|------|
| `src/envread.rs`（新規） | env ファイルの読み込み（quote 種別付き）と `${NAME}` / `${NAME:-default}` 展開。純関数群 + 単体テスト |
| `src/envfile.rs`（変更） | `variables()` を追加し、portool 管理変数を `Vec<(String, String)>` で返す。`render()` はそれを使う薄い層に |
| `src/cmd/sync.rs`（変更） | `ensure(ctx, quiet) -> Result<SyncOutcome>` を公開。fast path / slow path とも `SyncOutcome { block, manifest }` を返す。`run()` の外部挙動は不変 |
| `src/cmd/exec.rs`（新規) | 仕様 §5 のオーケストレーション。sync → env 合成 → 展開 → `execvp` |
| `src/error.rs`（変更） | `CommandNotFound`（127）/ `CommandNotExecutable`（126）を追加 |
| `src/main.rs`（変更） | `Exec` サブコマンド。`--env-file` 複数 + `#[arg(last = true, required = true)]` で `--` 必須 |
| `tests/exec.rs`（新規） | 仕様 §13 の統合テスト |

## 優先順位と展開のアルゴリズム

1. env ファイルを指定順にパース → `name -> (raw, quoting)`（後勝ち）
2. 最終マップを構築: env ファイル < 親環境（literal、展開しない） < portool 変数（literal）
3. env ファイル由来の unquoted / double-quoted 値のみ展開。参照解決は**最終マップ**に対して行い、
   メモ化 + visiting セットで循環検出
4. 子プロセスへは「親環境を継承 + env ファイル由来で親環境に無い変数を追加 + portool 変数を上書き」。
   `env_clear()` はしない（非 UTF-8 な親環境変数を壊さないため）

## 列挙したエッジケースと判断

- `.portool.toml` 欠如 → 仕様 §10 どおりエラー（sync 単体はマニフェスト無しを許すが、exec は要求する）
- `--` 無し / コマンド無し → clap usage error
- env ファイル相対パスは CWD 基準（worktree root ではない）。サブディレクトリから実行するテストを含める
- 同一ファイル内の重複キーは後勝ち（dotenv 慣行）
- `${NAME:-default}` は未定義**または空文字**で default
- default 部の入れ子 `${A:-${B}}` は展開する
- 自己参照・相互参照の循環 → エラー。ただし親環境が同名変数を持つ場合は親が勝つため env ファイル行は使われない
- `$NAME`（brace 無し）はリテラル。`$(`・バッククォートもリテラル（実行しない）
- 未終端の `${` / 不正な変数名 → ファイル名 + 行番号付き parse error（値は表示しない）
- CRLF は行末 `\r` を除去
- exec 失敗: NotFound → 127、それ以外（権限等） → 126
- 成功時は `execvp` でプロセス置換（Unix 前提のクレート）。シグナル透過は exec により自然に成立
- exec 内の sync は `quiet` 相当で実行（stdout を子コマンドのものに保つ）
- inline comment（`FOO=bar # c`）は値の一部として扱う（仕様が無言のため、切り捨てによる事故を避ける）
- 親環境の非 UTF-8 変数: 展開の参照元としては無視、子プロセスへはそのまま継承

## テスト（仕様 §13 対応）

単体（envread.rs 内）: パース、quote 規則、展開、default、再帰、未定義、循環、`$NAME` リテラル。

統合（tests/exec.rs）:
- sync 後のポート値が子コマンドに渡る（`sh -c 'echo $WEB_PORT'`）
- env ファイル無しでも実行できる
- 複数 env ファイルの優先順位 / 親環境が通常変数を上書き / portool 変数が全てを上書き
- `${NAME}` / `${NAME:-default}` の展開、single quote では展開しない
- 未定義変数・循環参照・env ファイル欠如・worktree 外・`.portool.toml` 欠如で失敗し子を起動しない
- 終了コード透過、command not found = 127、実行権限なし = 126
- `$$` 比較でプロセス置換（= シグナル透過）を確認
- parse error 出力に値を含まない
- 2 つの実 worktree で異なるポートと展開済み URL が渡る

## リリース

- この PR は feature のみ。バージョンの 0.4.0 bump は従来どおり別 PR（chore/release-v0.4.0）で行う
- README: exec セクション追加、"does not manage processes / template your .env files" の文言を再調整
