# リリースのチェックリスト — v0.2.0

雛形 `docs/compat/_template/release-checklist.md` から作った (09 §8.2 のリリースのゲート、§9.2 の手順 3)。
**タグの前に済む行だけ** を載せる。タグの後の結果 (タグの検証・配布物・署名の検証・公開後の検証) は GitHub Release のノートの
「検証」の節に書き、このファイルには戻さない。

次の行は `release.yml` が読む (先頭の 1 か所だけ)。40 桁の SHA を書く。雛形のままだとタグの検証が失敗する。

判定コミット: 81bc912aa9cb04bb06509deda1cfccce00f24f85

- 版: 0.2.0
- 判定した人 / 日付: 利用者 (メンテナ) とエージェント (Claude) / 2026-11-13
- 判定コミットの確認 (`git merge-base --is-ancestor develop <SHA>` と `git merge-base --is-ancestor main <SHA>` が真): [x]
- この後に入れてよい変更は `docs/compat/0.2.0/`・`docs/perf/0.2.0/`・`tests/ac-checklist.toml` だけ。ほかを入れたらゲートをやり直す。

判定は 合格 / 不合格 / 対象外 (理由) のいずれか。「根拠」には CI の実行の URL か記録のファイルを書く。基準機の型番など固有の情報は書かない (11 §9.7)。

| ゲート | 条件の正本 | 判定 | 根拠 | 確認者 | 日付 |
|---|---|---|---|---|---|
| 要件充足 | 09 §8.2。Must の AC がすべて合格。`tests/ac-checklist.toml` に `missing` が無い | 合格 (手動の 2 件は「受入」の行で判定) | `cargo xtask trace` (v0.2 の Must の AC: 試験 111 件・手順 9 件、対応の無いもの 0)。`tests/ac-checklist.toml` に `missing` 0。判定コミットの `ci.yml` の `trace` が成功。手順で判定する AC-FR-412-3・AC-NFR-402-1 は `acceptance.md` の §1〜§3 | エージェント (Claude) | 2026-11-13 |
| データ保全 | 09 §8.2。判定コミットに対する `nightly.yml` の手動の実行で、障害の注入 (v0.1 は 1,000 回、v0.2 から 10,000 回) が失敗 0、`determinism` が通る | 合格 (Linux は途中まで。下の所見、利用者の判断) | https://github.com/evoref/evorag/actions/runs/43250461996 (`chore/nightly-fault-runs` の定義で `runs=10000` を渡して起動。両方の job が判定コミット 81bc912a を取り出したことをログで確かめた)。Windows: 10,000 回で失敗 0 (17,754 秒)。Linux: 300 分の上限で打ち切られるまで失敗 0 (回数は出ない)。`determinism` は Windows・Linux とも成功 | 利用者・エージェント (Claude) | 2026-11-13 |
| 互換 | 09 §8.2・§3.6。golden fixture と、新しい版のデータを読むだけで開く試験が CI で通る (v0.2 から直前の版からの移行) | 合格 | 判定コミットの `ci.yml` (https://github.com/evoref/evorag/actions/runs/43248365408) が全ジョブ成功。形式の版 0.1 → 0.2 の移行 (#84)、golden fixture (`note` を足した、`embedding` の保存形は変わらない)、v0.2 のデータを v0.1.2 で開いたときの扱い (AC-NFR-307-2) の試験を含む | エージェント (Claude) | 2026-11-13 |
| 性能 | 09 §8.2・§5.1。基準機で `cargo xtask bench --out docs/perf/0.2.0` が目標内。NFR-204 は `cargo xtask dist` が判定 | 合格 | `docs/perf/0.2.0/` (起動 中央値 174 ms、発話から要求 平均 8.7 ms、埋め込みとリランクを含めて 平均 376 ms・p95 491 ms (目標 平均 500 ms)、キーから描画 p95 3.3 ms、トークンから描画 p95 3.9 ms、10 万件のベクトルの走査 中央値 11.8 ms、メモリ 103 MB)。BM-102 の v0.2 は bge-m3 と bge-reranker-v2-m3 (Q8_0、GPU)。NFR-204 は `cargo xtask dist` が判定 (Windows の実行ファイル 8.2 MB) | エージェント (Claude) | 2026-11-13 |
| 受入 | 09 §8.2・§3.8。`docs/compat/0.2.0/acceptance.md` がすべて合 (配布物は判定コミットで `cargo xtask dist` で作ったもの) | 未 | `docs/compat/0.2.0/acceptance.md` | 利用者 | |
| 実際の LLM | 09 §8.2・§3.4。`EVORAG_LIVE_LLM_URL=… cargo xtask live-llm` が合格 (使ったモデルの名前を根拠に書く) | 合格 | 応答 Qwen3.5-9B (Q4_K_M、思考を切る)、埋め込み bge-m3 (Q8_0)、リランカー bge-reranker-v2-m3 (Q8_0) で `cargo xtask live-llm`。AC-NFR-504-1 は 5/5、AC-FR-402-3 は正しく添えた 33/37・関係の無い問いで何も添えなかった 11/11、AC-FR-403-1 は正しい往復がリランクの最上位 5/5。検索の評価の基準値は `docs/perf/0.2.0/EVAL-retrieval.json` (recall@10 0.97、nDCG@10 0.93) | エージェント (Claude) | 2026-11-13 |
| セキュリティ | 09 §8.2。`cargo deny check advisories` がきれい。12 §6 の PC-1・PC-2・PC-6 | 合格 | `cargo deny --locked check advisories` で問題なし。判定コミットの `gitleaks` が成功 (PC-1)。私的なパス・アカウント名・メールアドレスが無い (PC-2)。足した試験データ (想起の事例・言い換えの事例) は作ったものだけ (PC-6) | エージェント (Claude) | 2026-11-13 |
| 依存 | 09 §8.2。`cargo xtask check-deps` (C を含む依存 0、推移的な依存が上限以内) | 合格 | `cargo xtask check-deps`: C を含む依存 0、推移的な依存 175 (Windows)・176 (Linux、上限 220) | エージェント (Claude) | 2026-11-13 |
| ドキュメント | 09 §8.2。`dist/README.md`・CHANGELOG・リリースノートに、既知の制約・更新と巻き戻し・データの移行の有無がある。リポジトリの `README.md` と `README.ja.md` の「最新の版」と「状況」がこの版と合っている | 合格 | `CHANGELOG.md` の 0.2.0 (データの形式を 0.2 に上げ、v0.1 のデータ根は最初の起動で退避してから移す)。`dist/README.md` に Linux の入れ方・v0.1 から更新する人へ・巻き戻しと v0.1.2 で消したものの扱い・既知の制約 (#122)。`README.md` と `README.ja.md` の「最新の版」と「状況」は v0.2.0 で、2 つの内容は同じ (#125) | エージェント (Claude) | 2026-11-13 |

## 所見・例外

- データ保全: Linux の障害の注入は、10,000 回を終える前に job の上限 (300 分) で打ち切られた。打ち切りまで失敗は無い (試験は 1 回でも失敗すると止まって失敗になる)。途中の回数は出ないので分からない。Windows の 10,000 回の合格と合わせて、利用者の判断で合格とした (2026-11-13)。1,000 回 (13 分) から見積もった 2 時間は外れた。データ根を回をまたいで使い続けるので、回が進むほど 1 回が重くなる。v0.3 までに、回数の途中の報告と、Linux を分けて流す形を考える。

- 障害の注入の 10,000 回は、`nightly.yml` を develop に入れずに、作業ブランチ `chore/nightly-fault-runs` の定義から起動した (判定コミットを動かさないため。試験するのは develop)。その定義 (回数の入力、上限 300 分、打ち切りの無い `cargo test`) はリリースの後に develop に入れる。
- `cargo xtask bench --out` に相対パスを渡すと、試験の作業フォルダ (各 crate の下) に書かれた。記録は `docs/perf/0.2.0/` に移した。xtask で根からのパスに直すのは、リリースの後に行う。
- v0.2 の Should の `doctor --deep` と `evorag reset` は v0.3 へ送った (08 §6、#123)。
