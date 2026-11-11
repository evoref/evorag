# CLAUDE.md — evorag

ローカル LLM (llama.cpp の llama-server) を使う、Rust 製のチャットと長期メモリのツール。**仕様書駆動** ([ADR-0001](docs/10-decisions.md#adr-0001-仕様書駆動での開発)) で開発する。仕様は `docs/` が正本 (入口は [docs/README.md](docs/README.md))。コードを書く前に該当する仕様書の節を読み、仕様と食い違うときは **コードではなく仕様書を先に直す提案** をする。ADR・OQ の状態は [docs/10-decisions.md](docs/10-decisions.md) を正とする。

ブランチ運用の正本は [docs/11-dev-workflow.md §9.1](docs/11-dev-workflow.md#91-ブランチ)。要点は次のとおり。

## ブランチ

- 長命ブランチは `main` と `develop` の 2 本だけ。
  - `main`: リリース専用。リリースした版だけが入る。**直接 commit / push しない**。
  - `develop`: 開発の本線。**直接 commit / push しない**。変更は作業ブランチ → PR で入れる。
- 作業ブランチは **`develop` から作る**。種別は `feat/` `fix/` `spec/` (docs) `chore/` `spike/`。名前は `<種別>/<crate>-<短い説明>` (例: `feat/evorag-store-envelope`、`spec/05-retrieval-budget`)。エージェントの作業は `agent/<名前>/<Issue 番号>-<短い説明>`。
- **1 タスク = 1 作業ブランチ = 1 PR**。PR は `develop` 宛て、squash で入れる。マージ後に作業ブランチを削除する。rebase は作業ブランチ内でだけ自由。
- `dev/<版>`、`release/*`、`hotfix/*` ブランチは使わない。
- merge するかどうかは人間が決め、PR ごとに指示する。実行は、指示を受けた私が `gh pr merge` で行う (作業の PR は `--squash`、`develop` ↔ `main` の PR は `--merge`。`--delete-branch` は付けない)。指示の無い PR は入れない。`gh pr merge` は実行前に確認が出る。
- 入れた後に、取り込んだコミットに `Signed-off-by:` があることと、`develop` の CI が通ることを確かめる ([docs/11 §9.1](docs/11-dev-workflow.md#91-ブランチ) の「merge の実行」)。

## リリースと版

- 機能リリースは `0.y.0` 単位 (v0.1.0、v0.2.0 …)。`develop` → `main` の PR (merge commit) を人間が入れ、`main` のマージコミットに **人間が署名付きタグ** を付けて GitHub Releases に公開する。**タグ (`v*`) は私が作らない・push しない**。
- bugfix リリースはパッチ版 (`0.y.z` の `z`)。経路は `fix/…` → `develop` → `main` (タグ) だけ。出せる条件は、前回のタグ以降の `develop` に `feat` が 1 件も入っていないこと。
- v0.1.0 のリリースまでの `0.0.x` は内部カウンタ (マイルストーンを develop に取り込む PR で +1、タグなし)。
- 版ごとの範囲は [docs/08-roadmap.md](docs/08-roadmap.md)。
- 版を上げる PR (パッチ版を含む) では、リポジトリの README も直す。英語版 `README.md` が表、日本語版 `README.ja.md` が正で、2 つを同じ PR で直す ([docs/11 §9.3](docs/11-dev-workflow.md#93-changelog-とリリース))。

## コミット

- DCO の `Signed-off-by:` を付ける (`-s`)。公開してよい著者名とメールアドレスだけを使う。
- コミットメッセージの形式は [docs/11 §9.2](docs/11-dev-workflow.md#92-コミットメッセージ) に従う (`spec:` / `verifies:` のフッタ)。
- 秘密情報・個人情報・利用者の会話の実例を docs やコードや試験データに書かない ([docs/11 §9.7](docs/11-dev-workflow.md#97-秘密情報の扱い))。

## 設計の要点 (実装するときに外さないもの)

- データを失わない (P1)。会話の本文と利用者の明示の操作は、DB とジャーナルの 2 か所に持つ ([docs/03](docs/03-storage.md))。
- 保存形式は後方互換の規則 ([docs/03 §4](docs/03-storage.md#4-後方互換の規則)) を守る。v0.4 までは形式を良くしてよいが、変えるときは直前の版からの移行を必ず入れる。v0.5 で凍結した後は項目を足すだけ。
- プロンプトの言語はデータ根ごとに固定で、途中で変えない ([docs/07 §6.1](docs/07-tui-config.md#61-プロンプトの言語))。
- 外へ通信するのは、設定した llama-server と、利用者が明示した更新の確認・取得だけ (P2)。
- コアは画面を知らない。コアの API (命令・イベント・照会) は `evorag-types` に置く ([docs/02 §3](docs/02-architecture.md#3-コアの-api))。

## 機械的な強制

- `.claude/settings.json` の PreToolUse hook (`.claude/hooks/guard-git.py`) が、次を拒否または確認にする。
  - `main` / `develop` への push と削除、force push (作業ブランチへの `--force-with-lease` だけは許す。リモートのブランチの削除は、作業ブランチの名前なら許し、それ以外は確認)
  - `main` / `develop` 上の commit / merge / rebase など
  - `v*` タグの作成と push
  - `git stash` (作業ツリーを動かし、改行コードや未追跡の変更を壊しやすいため)
  - `git reset --hard` / `git clean -f` / `git restore` / `git checkout -- <path>` (確認が出る)
  - `gh pr merge` (確認)
- GitHub のブランチ保護 (`main` と `develop`) も併用する。
- hook に拒否されたら、回避せずに理由を人間に伝えて指示を待つ。
- hook はコマンドの文字列を見て判定するので、heredoc や文字列の中の `git ...` にも反応する。文書を書くときは Write / Edit を使う。

## サブエージェントに作業を任せるとき

- 依頼文に、**作業ツリーを動かす git コマンド (`stash` / `checkout` / `reset` / `restore` / `clean`) を使わない** ことを明記する (hook でも拒否される)。
- 編集するファイルの範囲 (例: `docs/` のみ) と、コミットしないことを明記する。
- 各ファイルの改行コード (LF) を維持させる。Python では `newline=''` で読み書きする。
- 報告は信用しきらず、完了後に自分で `git status` / `git diff --stat` / リンク検査で確かめる。
