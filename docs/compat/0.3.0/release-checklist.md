# リリースのチェックリスト — v0.3.0

`docs/compat/_template/release-checklist.md` を写して埋めたもの (09 §8.2 のリリースのゲート、§9.2 の手順 3)。
**タグの前に済む行だけ** を載せる。タグの後の結果 (タグの検証・配布物・署名の検証・公開後の検証) は GitHub Release のノートの
「検証」の節に書き、このファイルには戻さない。

次の行は `release.yml` が読む (先頭の 1 か所だけ)。

判定コミット: 6f4d4df4ffd56bcd6d87ab9973dc4b4747d431b4

- 版: 0.3.0
- 判定した人 / 日付: エージェント (Claude) が計測し、受入は利用者 / 2026-11-19
- 判定コミットの確認 (`git merge-base --is-ancestor develop <SHA>` と `git merge-base --is-ancestor main <SHA>` が真): [x]
- この後に入れてよい変更は `docs/compat/0.3.0/`・`docs/perf/0.3.0/`・`tests/ac-checklist.toml` だけ。ほかを入れたらゲートをやり直す。

判定は 合格 / 不合格 / 対象外 (理由) のいずれか。「根拠」には CI の実行の URL か記録のファイルを書く。基準機の型番など固有の情報は書かない (11 §9.7)。

| ゲート | 条件の正本 | 判定 | 根拠 | 確認者 | 日付 |
|---|---|---|---|---|---|
| 要件充足 | 09 §8.2。Must の AC がすべて合格。`tests/ac-checklist.toml` に `missing` が無い | 合格 (手動の分は「受入」の行で判定) | `cargo xtask trace --version 0.3` (v0.3 の Must の AC: 試験 139 件・手順 9 件、後の版 1 件、対応の無いもの 0)。`tests/ac-checklist.toml` に `missing` は無い | エージェント (Claude) | 2026-11-19 |
| データ保全 | 09 §8.2。判定コミットに対する `nightly.yml` の手動の実行で、障害の注入 (v0.1 は 1,000 回、v0.2 から 10,000 回) が失敗 0、`determinism` が通る | 合格 | https://github.com/evoref/evorag/actions/runs/44351341399 (`--ref develop`、runs=10000)。OS ごとに 4 つの job で 2,500 回ずつ、Windows・Linux とも合わせて 10,000 回が失敗 0 (1 つの job は 47〜62 分)。`determinism` は Windows・Linux とも成功 | エージェント (Claude) | 2026-11-19 |
| 互換 | 09 §8.2・§3.6。golden fixture と、新しい版のデータを読むだけで開く試験が CI で通る (v0.2 から直前の版からの移行) | 合格 | 判定コミットの `ci.yml` (https://github.com/evoref/evorag/actions/runs/44351293648) が全ジョブ成功。形式 0.2 → 0.3 の移行と、v0.2 との行き来の試験 (AC-FR-307-6 など) を含む | エージェント (Claude) | 2026-11-19 |
| 性能 | 09 §8.2・§5.1。基準機で `cargo xtask bench --out docs/perf/0.3.0` が目標内。NFR-204 は `cargo xtask dist` が判定 | 合格 | `docs/perf/0.3.0/` (BM-101 起動 中央値 108 ms / 目標 500 ms、BM-102 発話から要求 平均 4.4 ms / 目標 100 ms、BM-103 キーから描画 p95 2.8 ms / 目標 16 ms、BM-104 トークンから描画 p95 4.1 ms / 目標 50 ms、BM-106 ベクトル 10 万件 中央値 10.2 ms / 目標 50 ms、BM-201 メモリ 106 MB / 目標 300 MB)。NFR-204: 判定コミットの `cargo xtask dist` で作った `evorag.exe` は 16.7 MB (上限 30 MB) | エージェント (Claude) | 2026-11-19 |
| 受入 | 09 §8.2・§3.8。`docs/compat/0.3.0/acceptance.md` がすべて合 (配布物は判定コミットで `cargo xtask dist` で作ったもの) | 対象外 (理由: 手動の受入で事実が抽出されず、忘れるの往復を実際の v0.2.0 で確かめられなかった。利用者の判断でリリースし、リリースの後に見直す。下の所見) | `docs/compat/0.3.0/acceptance.md` §1 (結果と所見)。v0.2.x との往復は自動の試験 (AC-FR-307-6) が CI で通っている。§2 (実際の取得) は次の版の公開後の検証 (09 §9.2 の手順 10) で行う | 利用者 | 2026-11-19 |
| 実際の LLM | 09 §8.2・§3.4。`EVORAG_LIVE_LLM_URL=… cargo xtask live-llm` が合格 (使ったモデルの名前を根拠に書く) | 合格 | 応答 Qwen3.5-9B (Q4_K_M)、埋め込み qwen3-embedding-8b (Q4_K_M)、リランカー bge-reranker-v2-m3 (Q8_0)、llama.cpp b1-622aa06 で `cargo xtask live-llm --out docs/perf/0.3.0`。事実の想起 (08 §2.4 の v0.3 の合格条件 (3)) は 30/30 (必要 27)、abstain 4/4、保存した事実の正しさ 22/22 (`EVAL-facts.json`)。LongMemEval の一部 (合格条件 (6)、基準値) は 50 問中 29 問 (0.58、`EVAL-longmemeval.json`) | エージェント (Claude) | 2026-11-19 |
| セキュリティ | 09 §8.2。`cargo deny check advisories` がきれい。12 §6 の PC-1・PC-2・PC-6 | 合格 | `cargo deny check advisories` で問題なし。gitleaks で全履歴 (238 コミット) に漏れなし (PC-1)。私的なパス・アカウント名が無い (PC-2)。試験データの会話は作ったものだけで、LongMemEval のデータはリポジトリに置かない (PC-6) | エージェント (Claude) | 2026-11-19 |
| 依存 | 09 §8.2。`cargo xtask check-deps` (C を含む依存 0、推移的な依存が上限以内) | 合格 | `cargo xtask check-deps`: C を含む依存 0、推移的な依存 最大 205 (上限 220) | エージェント (Claude) | 2026-11-19 |
| ドキュメント | 09 §8.2。`dist/README.md`・CHANGELOG・リリースノートに、既知の制約・更新と巻き戻し・データの移行の有無がある。リポジトリの `README.md` と `README.ja.md` の「最新の版」と「状況」がこの版と合っている | 合格 (リリースノートは publish の前に確かめる) | `dist/README.md`・`dist/README.en.md` (§6 更新と巻き戻し、§8 既知の制約)、`CHANGELOG.md` の 0.3.0 (データの形式 0.3、v0.2.x からは手作業の更新)、`README.md`・`README.ja.md` の最新の版と状況が v0.3.0 | エージェント (Claude) | 2026-11-19 |

## 所見・例外

- 08 §6 の v0.2.0 の繰り越し (Linux の障害の注入が job の上限で終わらなかった) は、OS ごとに 4 つの job に分けて流す形 (09 §6.1) で解消した。
- ゲートの準備の中で、手元の LLM での評価から 2 つの不具合を見つけて直した (#223〜#225 照合の形、#227〜#229 抽出の質)。どちらも判定コミットに含まれる。
- 確かめていない事例に、まだ怪しい事実が残る (職業 `work`、趣味 `読書の記録の付け方` など)。抽出の質を上げるのは v0.4 の課題とする。
- 手動の受入 (`acceptance.md` §1) では、v0.3.0 で話した事実が `/memory` に現れず、忘れる操作から先を実際の実行ファイルで確かめられなかった。月をまたぐ項目は対象外とした。利用者が、この状態でリリースし、抽出と忘れるの仕組みをリリースの後に見直すと判断した。
- LongMemEval の判定は決定論 (文字列の比べ) なので、元の論文の値 (GPT-4o の判定) とは比べない。複数のセッションにまたがる問い (3/13) が弱い。
