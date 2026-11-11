# evorag

[English](README.md) | 日本語

Rust 製の、チャットと長期メモリのツールです。手元の llama-server (llama.cpp) だけを使って動くので、会話は PC の外に出ません。以前の会話のうち関係のあるところを、出典つきで応答に添えます。この先の版で、利用者についての事実を覚え、評価と訂正から少しずつ良くなるようにします。モデルの重みは学習しません。

> **最新の版は v0.1.2 (2026-11-11) です。** 配布物は [GitHub Releases](https://github.com/evoref/evorag/releases) にあります。Windows 11 (x86_64) 用の zip です。入れ方・最初の設定・更新と巻き戻し・署名の検証・既知の制約は、同梱の README ([dist/README.md](dist/README.md)) にあります。

## 状況

- **v0.1 (最小核)** は、2026-11-10 に v0.1.0 としてリリースしました。同じ日の v0.1.1 で、エラーの表示を直しました。v0.1.2 (2026-11-11) で、新しい版で書いた記録を含むセッションを、v0.1 でも開けるようにしました。
  - Windows 11 の Windows Terminal で動きます。画面は日本語です。
  - 手元の llama-server と会話し、応答を生成しながら表示します。中断と作り直しができます。
  - 会話はセッションとして残ります。データベースとジャーナルの 2 か所に書くので、強制終了しても消えません。1 日に 1 回退避を取り、`evorag restore` で戻せます。
  - 以前の会話から関係のある発言を探し (語彙の索引)、出典の番号つきで応答に添えます。
  - Linux と macOS には、まだ対応していません。
- **v0.2** の仕様を作っています。応答が始まるまでの待ち時間を縮め、リランカーで関係の無い記録を添えないようにし、埋め込みとベクトル検索を足し、利用者が操作していない間に会話を整理し、Linux に対応する版です。

v0.6 までの版ごとの予定は [docs/08-roadmap.md](docs/08-roadmap.md) にあります。

保存の形式は、v0.5 まで変わることがあります。移行は直前の版から行うので、版を飛ばさずに更新してください。

変更点は [CHANGELOG.md](CHANGELOG.md) にあります。

## 必要なもの

evorag は、モデルと推論エンジンを含みません。[llama.cpp](https://github.com/ggml-org/llama.cpp) の llama-server が動いていることと、GGUF 形式のチャットのモデルが必要です。`evorag setup` が llama-server を探し、能力を確かめて、設定を書きます。

既定では、同じ PC の llama-server (ループバック) にだけ接続します。

## 配布物の検証

リリースの `SHA256SUMS` には、minisign の署名 (`SHA256SUMS.minisig`) が付いています。公開鍵は下のとおりです。このリポジトリの [minisign.pub](minisign.pub) と同じものなので、両方を見比べてから使ってください。

```text
untrusted comment: minisign public key A3B00C77E1EA828E
RWSOgurhdwywo1k6KUE/J4IMo5HcwmhR6dsz/+OgROUdnDutFx3qxies
```

```sh
minisign -Vm SHA256SUMS -P RWSOgurhdwywo1k6KUE/J4IMo5HcwmhR6dsz/+OgROUdnDutFx3qxies
```

詳しい手順は、同梱の README にあります。

## 仕様書

仕様書は [docs/](docs/README.md) にあります。

**作業中の版として公開しています。** 内容は変わることがあり、「提案中」と書かれた事項は未確定です。

リリースした版の仕様は、その版のタグ (または `main`) の `docs/` です。開発中の版は `develop` の `docs/` です。

- 入口: [docs/README.md](docs/README.md)
- 決定記録 (ADR) と未決事項 (OQ): [docs/10-decisions.md](docs/10-decisions.md)
- ロードマップ: [docs/08-roadmap.md](docs/08-roadmap.md)

## ブランチ

- `main`: リリース専用です。リリースした版だけが入ります。
- `develop`: 開発の本線です。作業ブランチから PR で入れます。

詳細は [docs/11-dev-workflow.md](docs/11-dev-workflow.md) の §9.1 を参照してください。

## 貢献について

**外部からの Pull Request は、まだ受け付けていません。** バグ報告や意見は Issues で歓迎します。

脆弱性は公開の Issue に書かず、[SECURITY.md](SECURITY.md) の窓口に報告してください。

貢献の方針は [CONTRIBUTING.md](CONTRIBUTING.md) にあります。

## ライセンス

[Apache License 2.0](LICENSE) です。帰属表示は [NOTICE](NOTICE) を参照してください。

---

この README は日本語版が正です。英語版 ([README.md](README.md)) と食い違うときは、日本語版に従います。
