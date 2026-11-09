# Changelog

利用者に見える変更を記録する。形式は [Keep a Changelog](https://keepachangelog.com/ja/1.1.0/)、版は [docs/11-dev-workflow.md §9.1](docs/11-dev-workflow.md#91-ブランチ) に従う。

## [Unreleased]

### 追加

- 手元の llama-server と、逐次表示で会話する。Esc で中断し、`/retry` で作り直す。失敗した発話は残して送り直せる。
- セッションの作成・一覧・再開・改名・削除 (`/new`・`/sessions`・`/rename`・`/delete`)。
- 応答の評価 (`/good`・`/bad`) と、再生成の記録。
- 以前の会話のうち関係のある発言を、出典の番号つきで応答に添える (語彙の索引)。Tab で番号を選び Enter で出典を表示する。`/sources on|off` でセッションごとに切り替える。
- 会話をデータベースとジャーナルの 2 か所に書き、強制終了しても確定した会話を失わない。
- 1 日 1 回の自動の退避、`evorag restore`・`evorag backups list`。壊れた DB は `broken/` へ移し、退避とジャーナルから戻す。
- `evorag setup` (llama-server を探して設定を書く)、`evorag doctor` (読むだけの点検)、`evorag export` (JSON Lines への書き出し)。
- 外部のエディタ (Ctrl+E)、入力欄の下書きの自動保存、応答の Markdown の装飾、`/model` での切り替え。
- 時刻を地方時で示す。思考するモデルが考えている間は「考えています」と示す。
- 同期フォルダ・ネットワークドライブのデータ根を既定で拒否する。空きが足りなければ裏の処理を止めて知らせる。
- `evorag --version` に作りの情報と入れ方を出す。古い版の設定ファイルは退避してから書き換える。
