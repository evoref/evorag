# リリースのチェックリスト — v<X.Y.Z>

この雛形を `docs/compat/<X.Y.Z>/release-checklist.md` に写して埋める (09 §8.2 のリリースのゲート、§9.2 の手順 3)。
**タグの前に済む行だけ** を載せる。タグの後の結果 (タグの検証・配布物・署名の検証・公開後の検証) は GitHub Release のノートの
「検証」の節に書き、このファイルには戻さない。

次の行は `release.yml` が読む (先頭の 1 か所だけ)。40 桁の SHA を書く。雛形のままだとタグの検証が失敗する。

判定コミット: <ゲートを判定した、版上げの後の develop の先頭の SHA (40 桁)>

- 版: <X.Y.Z>
- 判定した人 / 日付: <名前> / <YYYY-MM-DD>
- 判定コミットの確認 (`git merge-base --is-ancestor develop <SHA>` と `git merge-base --is-ancestor main <SHA>` が真): [ ]
- この後に入れてよい変更は `docs/compat/<X.Y.Z>/`・`docs/perf/<X.Y.Z>/`・`tests/ac-checklist.toml` だけ。ほかを入れたらゲートをやり直す。

判定は 合格 / 不合格 / 対象外 (理由) のいずれか。「根拠」には CI の実行の URL か記録のファイルを書く。基準機の型番など固有の情報は書かない (11 §9.7)。

| ゲート | 条件の正本 | 判定 | 根拠 | 確認者 | 日付 |
|---|---|---|---|---|---|
| 要件充足 | 09 §8.2。Must の AC がすべて合格。`tests/ac-checklist.toml` に `missing` が無い | | | | |
| データ保全 | 09 §8.2。判定コミットに対する `nightly.yml` の手動の実行で、障害の注入 (v0.1 は 1,000 回、v0.2 から 10,000 回) が失敗 0、`determinism` が通る | | | | |
| 互換 | 09 §8.2・§3.6。golden fixture と、新しい版のデータを読むだけで開く試験が CI で通る (v0.2 から直前の版からの移行) | | | | |
| 性能 | 09 §8.2・§5.1。基準機で `cargo xtask bench --out docs/perf/<X.Y.Z>` が目標内。NFR-204 は `cargo xtask dist` が判定 | | | | |
| 受入 | 09 §8.2・§3.8。`docs/compat/<X.Y.Z>/acceptance.md` がすべて合 (配布物は判定コミットで `cargo xtask dist` で作ったもの) | | | | |
| 実際の LLM | 09 §8.2・§3.4。`EVORAG_LIVE_LLM_URL=… cargo xtask live-llm` が合格 (使ったモデルの名前を根拠に書く) | | | | |
| セキュリティ | 09 §8.2。`cargo deny check advisories` がきれい。12 §6 の PC-1・PC-2・PC-6 | | | | |
| 依存 | 09 §8.2。`cargo xtask check-deps` (C を含む依存 0、推移的な依存が上限以内) | | | | |
| ドキュメント | 09 §8.2。`dist/README.md`・CHANGELOG・リリースノートに、既知の制約・更新と巻き戻し・データの移行の有無がある。リポジトリの `README.md` と `README.ja.md` の「最新の版」と「状況」がこの版と合っている | | | | |

## 所見・例外

(不合格・対象外の理由と、その扱い (繰り越し先など) を書く。)
