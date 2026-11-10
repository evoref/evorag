# v0.1 手動の受入 (Acceptance)

| 項目 | 内容 |
|---|---|
| 文書ID | compat/0.1.0/acceptance |
| 状態 | 記録済み (v0.1.0 のゲート、2026-11-10) |
| 前提 | [09 §3.8](../../09-testing-quality.md#38-手動の受入) / [07 §2.3](../../07-tui-config.md#23-日本語入力) / [08 §2.2](../../08-roadmap.md#22-v01-最小核-会話保存保護語彙の想起) |
| 正本の概念 | v0.1 の手動の受入の結果 (端末と IME、setup の案内、エラーの表示、配布物の検証) |

> 自動の試験で判定できない受入基準 (検証の区分が M のもの) を、人が確かめて記録する。リリースゲート (09 §8.2) の「手動の受入」の判定に使う。結果は、確かめた日・版・人を添えて残す。個人を特定する情報 (PC の型番など) は書かない (11 §9.7)。

---

## 1. 端末と日本語入力 (07 §2.3)

v0.1 の合格条件の組み合わせ: Windows Terminal + Microsoft IME (新・旧)、Google 日本語入力、ATOK。

| 組み合わせ | 確かめること | 結果 | 確かめた日・版 |
|---|---|---|---|
| Windows Terminal + Microsoft IME (新) | 変換の候補の窓と変換中の文字が入力位置に出る。確定した文字が欠けず重ならない。全角・半角の混じった行と、折り返した 2 行目でも位置が合う | 合 | 2026-11-10、0.1.0 (判定コミット 7b712a02 の配布物)、利用者が確認 (初回は 2026-11-09、develop 0.0.4) |
| Windows Terminal + Microsoft IME (旧) | 同上 | 合 | 同上 |
| Windows Terminal + Google 日本語入力 | 同上 | 合 | 同上 |
| Windows Terminal + ATOK | 同上 | 合 | 同上 |

- 同じ作り (描画のたびに本当のカーソルを入力位置に置く) での技術検証の記録は `spikes/S-03/REPORT.md`。
- リリースのゲート (09 §9.2 の手順 3) で、判定コミットの作りでもう一度確かめる (画面に手を入れたとき)。

## 2. setup の案内 (NFR-603、AC-NFR-603-1)

| 確かめること | 結果 | 確かめた日・版 |
|---|---|---|
| llama-server の無い PC で、`evorag setup` の案内どおりに最初の会話まで進める | 合 | 2026-11-10、0.1.0 (判定コミット 7b712a02、`cargo xtask dist` で作り本物の鍵で署名した配布物)、利用者が確認 |

- llama-server が無いとき: 探した接続先 (8080・8081) と入手・起動の手順 (`--host 127.0.0.1` と `--api-key` を付けた例) を示し、設定ファイルを書かずに終わった。
- 案内どおりに API キーを付けて llama-server を起動し、`evorag setup --api-key-env EVORAG_API_KEY` で接続先と能力を示して設定を書き、`evorag` で最初の会話まで進めた。
- プロンプトの言語: 言語を記録済みのデータ根では尋ねず、データ根を消した新しい状態では尋ねた (07 §6.1 のとおり)。

## 3. エラーの表示 (NFR-604)

| 確かめること | 結果 | 確かめた日・版 |
|---|---|---|
| llama-server に繋がらない・データ根を使えない・DB が壊れた・設定の誤り・空きの不足のそれぞれで、何が起きたか・データは無事か・次に何をすればよいかが表示される | 合 (所見 3 件、v0.1.1 で直す) | 2026-11-10、0.1.0 (判定コミット 7b712a02、`cargo xtask dist` で作り本物の鍵で署名した配布物)、利用者が確認 |

| 場合 | 結果 | 表示 (要点) |
|---|---|---|
| llama-server に繋がらない | 合 (所見 1) | 接続先と理由、「llama-server を起動してから、もう一度 evorag を起動してください。接続先の設定は evorag setup で作れます」 |
| データ根を使えない (2 つ同時に起動) | 合 (所見 2) | 「データ根を使えません: the data root is in use by another evorag process (pid …)」。先に起動した方は動き続けた |
| 設定の誤り | 合 | ファイル名と行・桁つきの理由 (`duplicate key`) |
| DB が壊れた | 合 | 壊れたファイルを消さずに `broken\<時刻>\` へ移した旨と理由、戻すかの問い。`y` で退避とジャーナルから戻した |
| 空きの不足 | 対象外 | 手動では再現が難しい。自動の試験 `crates/evorag-core/tests/chat_path.rs::low_space_pauses_background_work_but_the_conversation_continues` で判定 |

所見 (どれも文言だけで、動きとデータには関わらない。利用者の判断で v0.1.0 は出し、直後のパッチ版 v0.1.1 で直す):

1. llama-server に繋がらないとき、「データは無事」の一言が無い。
2. データ根を使えないとき、理由が英語のままで、次にすること (先に起動した evorag を終える) が無い。
3. llama-server が API キーを拒んだ (HTTP 401) ときも「llama-server を起動してから…」と案内する。正しくは、キーが違う・設定されていないことと、`llm.chat.api_key_env` の環境変数を確かめることを案内する (受入の途中に見つかった)。

## 4. 配布物の検証 (12 §5)

| 確かめること | 結果 | 確かめた日・版 |
|---|---|---|
| `SHA256SUMS` と minisign の署名を、同梱の手順書どおりに確かめられる | 合 | 2026-11-10、0.1.0 (判定コミット 7b712a02、`cargo xtask dist` で作り本物の鍵で署名した配布物)、利用者が確認 |

- `Get-FileHash` の値が `SHA256SUMS` の行と一致した。
- `minisign -Vm SHA256SUMS -p minisign.pub` と `minisign -Vm release.json -p minisign.pub` の両方で `Signature and comment signature verified`。trusted comment は `evorag 0.1.0 release.json sha256 00de7153…`。公開鍵の鍵 ID は `A3B00C77E1EA828E`。
- `cargo xtask verify-release target\dist --require-signatures` も合格。
- 公開した配布物の検証は、09 §9.2 の手順 10 で行い、Release のノートに書く。
