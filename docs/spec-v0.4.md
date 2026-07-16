# portool v0.4 — `portool exec` 仕様

portool をアプリ固有の設定ツールにせず、「割当値を環境変数としてコマンドへ安全に注入する」
ところまで担当させる。

## 1. 背景

portool は現在、worktree ごとのポートを割り当てて `.env.portool` に出力する受動的なツールである。
利用側では、次の処理をリポジトリごとに実装する必要がある。

- `.env.portool` の読み込み
- `.env.test` など既存 env との合成
- `TEST_DB_PORT` を含む `DATABASE_URL` の構築
- 子プロセスへの環境変数の引き渡し
- 終了コードやシグナルの透過

この重複を解消するため、portool に `exec` サブコマンドを追加する。

## 2. 目的

- portool の割当値を、任意のコマンドへ環境変数として渡せる
- `.env.test` などのデフォルト設定と割当値を合成できる
- `${TEST_DB_PORT}` のような変数参照を展開できる
- worktree ごとのランチャースクリプトを不要にする
- shell の `source` や `eval` に依存しない
- 子コマンドの標準入出力、終了コード、シグナルを透過する

## 3. 非目標

- `DATABASE_URL` などアプリ固有変数の意味を portool が理解すること
- `.env.test` を自動検出すること
- portool がインストールされていない環境でのフォールバック
- 秘密情報の保存・暗号化・同期
- shell 構文全般の解釈
- コマンド置換、算術展開、任意コード実行

## 4. CLI

基本形式:

```
portool exec [OPTIONS] -- <COMMAND> [ARGS...]
```

例:

```
portool exec -- npm run dev

portool exec \
  --env-file dashboard/.env.test \
  -- npm --prefix dashboard run test:int:base

portool exec \
  --env-file dashboard/.env.test \
  --env-file dashboard/.env.test.local \
  -- npm --prefix dashboard run test:int:base
```

オプション:

```
-e, --env-file <PATH>    読み込む env ファイル。複数指定可能
-h, --help               ヘルプを表示
```

`--` は必須とする。コマンドが指定されていない場合は usage error にする。

## 5. 実行フロー

`portool exec` は次の順で処理する。

1. 現在位置から Git worktree root を特定する
2. worktree root の `.portool.toml` を読み込む
3. `portool sync` 相当の処理を行う
4. 最新の portool 割当値を取得する
5. 指定された env ファイルを順番に読み込む
6. 親プロセスの環境変数と合成する
7. env ファイル内の変数参照を展開する
8. shell を介さず、指定されたコマンドを起動する

sync または env 構築に失敗した場合、子コマンドは起動しない。

## 6. 環境変数の優先順位

優先順位は次のとおりとする。

```
先に指定した env ファイル
  < 後に指定した env ファイル
  < 親プロセスの環境変数
  < portool が管理する変数
```

portool が管理する変数には次が含まれる。

- `.portool.toml` の `[ports]` から生成された変数
- `PORTOOL_PROJECT_ID`
- `PORTOOL_WORKTREE_ID`
- 将来 `.env.portool` に追加される portool 管理メタデータ

したがって、env ファイルや親プロセスに古い `TEST_DB_PORT` が残っていても、現在の worktree の
割当値が最終的に使用される。一方、`DATABASE_URL`、`JWT_SECRET` など portool 管理外の変数は、
親プロセスの値を最優先する。

## 7. 変数展開

env ファイルでは、以下の形式を利用できる。

```
DATABASE_URL=postgresql://postgres:password@localhost:${TEST_DB_PORT}/testdb
DATABASE_URL=postgresql://postgres:password@localhost:${TEST_DB_PORT:-5432}/testdb
```

対応する構文:

```
${NAME}
${NAME:-default}
```

ルール:

- 変数名は `[A-Za-z_][A-Za-z0-9_]*`
- unquoted または double-quoted な値を展開する
- single-quoted な値は展開しない
- `${NAME}` が未定義ならエラー
- `${NAME:-default}` は未定義または空文字の場合に default を使う
- env ファイル内の別変数も参照できる
- 循環参照はエラー
- `$NAME` 形式はサポートしない
- コマンド置換 `$()` やバッククォートは実行しない

展開には、優先順位を適用した後の最終的な変数セットを使用する。

## 8. env ファイルの扱い

- 相対パスは `portool exec` を実行したカレントディレクトリ基準
- 明示した env ファイルが存在しない場合はエラー
- env ファイルを指定しない場合は、親環境と portool 割当値だけを使用
- `.env` や `.env.test` を暗黙には読み込まない
- ファイル内の秘密値や完成した環境変数を標準出力へ表示しない
- parse error にはファイル名と行番号を含めるが、値そのものは表示しない

## 9. 子コマンドの実行

- shell を使用しない
- `COMMAND` と `ARGS` をそのまま OS のプロセス API へ渡す
- stdin、stdout、stderr を継承する
- カレントディレクトリを変更しない
- 子コマンドの終了コードをそのまま返す
- Unix では可能ならプロセスを置き換え、シグナルを自然に透過する
- コマンドが存在しない場合は終了コード `127`
- 実行権限がない場合は終了コード `126`

shell 機能が必要な場合、利用者が明示的に `sh -c '...'` を指定する。

## 10. エラー処理

以下の場合、子コマンドを起動せず失敗する。

- Git worktree 外で実行された
- `.portool.toml` が存在しない
- portool の割当または sync に失敗した
- env ファイルが存在しない
- env ファイルの構文が不正
- 展開対象の変数が未定義
- 変数参照が循環している
- コマンドが指定されていない

エラーには対処可能な情報を含めるが、env 値や秘密情報は含めない。

## 11. 後方互換性

- `init`、`sync`、`ls`、`prune` の挙動は変更しない
- `.portool.toml` と `.env.portool` の形式は変更しない
- `exec` を使わない既存プロジェクトには影響しない
- 新しいサブコマンド追加として `0.4.0` でリリースする
