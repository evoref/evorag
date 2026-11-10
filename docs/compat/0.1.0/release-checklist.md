# リリースのチェックリスト — v0.1.0

雛形 `docs/compat/_template/release-checklist.md` から作った (09 §8.2 のリリースのゲート、§9.2 の手順 3)。
**タグの前に済む行だけ** を載せる。タグの後の結果 (タグの検証・配布物・署名の検証・公開後の検証) は GitHub Release のノートの
「検証」の節に書き、このファイルには戻さない。

次の行は `release.yml` が読む (先頭の 1 か所だけ)。40 桁の SHA を書く。雛形のままだとタグの検証が失敗する。

判定コミット: 7b712a02f451acabca3bfc879df05789dec5a4bc

- 版: 0.1.0
- 判定した人 / 日付: 利用者 (メンテナ) とエージェント (Claude) / 2026-11-10
- 判定コミットの確認 (`git merge-base --is-ancestor develop <SHA>` と `git merge-base --is-ancestor main <SHA>` が真): [x] (`main` の先頭は初期コミット 81ccf2e)
- この後に入れてよい変更は `docs/compat/0.1.0/`・`docs/perf/0.1.0/`・`tests/ac-checklist.toml` だけ。ほかを入れたらゲートをやり直す。

判定は 合格 / 不合格 / 対象外 (理由) のいずれか。「根拠」には CI の実行の URL か記録のファイルを書く。基準機の型番など固有の情報は書かない (11 §9.7)。

| ゲート | 条件の正本 | 判定 | 根拠 | 確認者 | 日付 |
|---|---|---|---|---|---|
| 要件充足 | 09 §8.2。Must の AC がすべて合格。`tests/ac-checklist.toml` に `missing` が無い | 合格 | `tests/ac-checklist.toml` (79 件: test 72・review 4・manual 3、missing 0) | エージェント (Claude) | 2026-11-10 |
| データ保全 | 09 §8.2。判定コミットに対する `nightly.yml` の手動の実行で、障害の注入 (v0.1 は 1,000 回、v0.2 から 10,000 回) が失敗 0、`determinism` が通る | 合格 | https://github.com/evoref/evorag/actions/runs/42745642153 (fault-injection と determinism を Windows・Linux で、4 ジョブとも成功) | エージェント (Claude) | 2026-11-10 |
| 互換 | 09 §8.2・§3.6。golden fixture と、新しい版のデータを読むだけで開く試験が CI で通る (v0.2 から直前の版からの移行) | 合格 | 判定コミットの `ci.yml` (https://github.com/evoref/evorag/actions/runs/42745626501) が全ジョブ成功 (golden fixture・新しい版のデータを読むだけで開く試験を含む) | エージェント (Claude) | 2026-11-10 |
| 性能 | 09 §8.2・§5.1。基準機で `cargo xtask bench --out docs/perf/0.1.0` が目標内。NFR-204 は `cargo xtask dist` が判定 | 合格 | `docs/perf/0.1.0/BM-101〜104・201.json` (起動 153 ms、発話から要求 平均 7.0 ms、キーから描画 p95 2.7 ms、トークンから描画 p95 2.8 ms、メモリ 90 MB)。NFR-204 は `cargo xtask dist` が判定 (6.4 MB) | 利用者 | 2026-11-10 |
| 受入 | 09 §8.2・§3.8。`docs/compat/0.1.0/acceptance.md` がすべて合 (配布物は判定コミットで `cargo xtask dist` で作ったもの) | 合格 (所見 3 件、下記) | `docs/compat/0.1.0/acceptance.md` (§1 日本語入力 (4 つの IME)・§2 setup・§4 配布物の検証は合、§3 エラーの表示は合 (所見 3 件)。配布物は判定コミットで作り本物の鍵で署名したもの) | 利用者 | 2026-11-10 |
| 実際の LLM | 09 §8.2・§3.4。`EVORAG_LIVE_LLM_URL=… cargo xtask live-llm` が合格 (使ったモデルの名前を根拠に書く) | 合格 | `cargo xtask live-llm` で AC-NFR-504-1 が 5/5 (Qwen3.5-9B-Q4_K_M、llama-server b1-138789d) | 利用者 | 2026-11-10 |
| セキュリティ | 09 §8.2。`cargo deny check advisories` がきれい。12 §6 の PC-1・PC-2・PC-6 | 合格 | `cargo deny check advisories` で問題なし。gitleaks で全履歴 (79 コミット) に漏れ 0 (PC-1)。私的なパス・アカウント名・メールアドレスが無い (PC-2)。試験データは作ったもの (S-04 の架空の会話、記録・再生の llama-server) だけ (PC-6) | エージェント (Claude) | 2026-11-10 |
| 依存 | 09 §8.2。`cargo xtask check-deps` (C を含む依存 0、推移的な依存が上限以内) | 合格 | `cargo xtask check-deps`: C を含む依存 0、推移的な依存 175〜176 (上限 220) | エージェント (Claude) | 2026-11-10 |
| ドキュメント | 09 §8.2。`dist/README.md`・CHANGELOG・リリースノートに、既知の制約・更新と巻き戻し・データの移行の有無がある | 合格 (所見 1 件、下記) | `dist/README.md` (更新と巻き戻し・既知の制約)、`CHANGELOG.md` の 0.1.0 (データの移行が無いこと) | エージェント (Claude) | 2026-11-10 |

## 所見・例外

利用者の判断 (2026-11-10): 次の所見はどれも文言か手順書の記載だけで、動きとデータには関わらないので、v0.1.0 は出し、直後のパッチ版 v0.1.1 (`fix/…` → `develop` → `main`) で直す。

1. エラーの表示 (受入 §3、NFR-604): llama-server に繋がらないとき、「データは無事」の一言が無い。
2. エラーの表示 (受入 §3、NFR-604): データ根を使えないとき、理由が英語のままで、次にすること (先に起動した evorag を終える) が無い。
3. エラーの表示 (受入の途中に見つかった): llama-server が API キーを拒んだ (HTTP 401) ときも「llama-server を起動してから…」と案内する。キーが違う・設定されていないことと、`llm.chat.api_key_env` の環境変数を確かめるよう案内する。
4. ドキュメント: `dist/README.md` §5 のデータの置き場所の表に、ログのフォルダ (状態のフォルダの `logs\`) が無い。

対象外: 受入 §3 の「空きの不足」は手動では再現が難しいので、自動の試験 (`low_space_pauses_background_work_but_the_conversation_continues`) で判定した。
