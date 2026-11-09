# evorag

手元の llama-server (llama.cpp) だけで動く、Rust 製のチャットと長期メモリのツールです。会話は PC の外に出しません。以前の会話と、そこから覚えた事実を出典つきで応答に添え、評価と訂正から少しずつ良くなります (モデルの重みは学習しません)。

> **開発中です。まだ配布物はありません。** 最初の版 (v0.1) は Windows 11 向けで、リリースの準備をしています。

## 状況

- v0.1 (最小核) の機能は `develop` に入っています: 手元の llama-server と日本語で会話でき、会話は強制終了しても消えず、以前の会話を出典つきで思い出します。版ごとの予定は [docs/08-roadmap.md](docs/08-roadmap.md) にあります。
- 利用者向けの手順書 (入れ方・setup・更新と巻き戻し・既知の制約) は [dist/README.md](dist/README.md) です。配布物に同梱します。
- 配布物の署名は minisign で行い、公開鍵はリポジトリ直下の `minisign.pub` に置きます (最初のリリースで置きます)。

## 仕様書

仕様書は [docs/](docs/README.md) にあります。**作業中の版として公開しています**。内容は変わることがあり、「提案中」と書かれた事項は未確定です。

- 入口: [docs/README.md](docs/README.md)
- 決定記録 (ADR) と未決事項 (OQ): [docs/10-decisions.md](docs/10-decisions.md)
- ロードマップ: [docs/08-roadmap.md](docs/08-roadmap.md)

`main` はリリースした版の仕様だけを持ちます。開発中の版は `develop` の `docs/` です。

## ブランチ

- `main`: リリース専用です。リリースした版だけが入ります。
- `develop`: 開発の本線です。作業ブランチから PR で入れます。

詳細は [docs/11-dev-workflow.md](docs/11-dev-workflow.md) の §9.1 を参照してください。

## 貢献について

**外部からの Pull Request は、まだ受け付けていません。** バグ報告や意見は Issues で歓迎します。脆弱性は公開の Issue に書かず、[SECURITY.md](SECURITY.md) の窓口に報告してください。貢献の方針は [CONTRIBUTING.md](CONTRIBUTING.md) にあります。

## ライセンス

[Apache License 2.0](LICENSE) です。帰属表示は [NOTICE](NOTICE) を参照してください。
