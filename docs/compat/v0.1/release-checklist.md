# v0.1 リリースのゲートの記録 (Release checklist)

| 項目 | 内容 |
|---|---|
| 文書ID | compat/v0.1/release-checklist |
| 状態 | 未記入 (リリース候補 `v0.1.0-rc.N` で埋める) |
| 前提 | [09 §8.2](../../09-testing-quality.md#82-リリースのゲート) / [09 §9.2](../../09-testing-quality.md#92-手順) / [12 §5・§6](../../12-licensing-distribution.md#5-配布) |
| 正本の概念 | v0.1 のリリースのゲートの判定の記録 |

> 09 §9.2 の手順 4 で、リリース候補ごとに判定して記録する。結果は、確かめた日・候補のタグ・人を添える。個人を特定する情報は書かない (11 §9.7)。

## 1. ゲート (09 §8.2)

| 項目 | 判定のしかた | 結果 | 日・候補 |
|---|---|---|---|
| Must の AC | `tests/ac-checklist.toml` で、すべての v0.1 の AC が `test`・`manual`・`review` のどれかで合格 (status = `missing` が残らない) | | |
| 障害の注入 | `EVORAG_FAULT_RUNS=1000 cargo nextest run -p evorag-core --test fault_injection` (nightly でも可) で失敗 0 | | |
| 互換 | 09 §3.6 の試験 (golden fixture・新しい版のデータを読むだけで開く) が CI で通る | | |
| 性能 | 09 §5.1 の目標を基準機で満たす。`cargo xtask bench` (NFR-101〜104・201、結果は `docs/perf/v0.1/`)。NFR-204 は `cargo xtask dist` が判定 | | |
| 実際の LLM | `EVORAG_LIVE_LLM_URL=… cargo xtask live-llm` で、以前の会話に紛れ込ませた指示に従わない (AC-NFR-504-1、5 回中 4 回以上) | | |
| 再現性 | nightly の `determinism` (試験を 2 回流して結果が同じ、AC-NFR-701-1) が通っている | | |
| 手動の受入 | `docs/compat/v0.1/acceptance.md` がすべて「合」 | | |
| 文書 | `dist/README.md` とリリースノートに、既知の制約・更新と巻き戻しの手順・データの移行の有無がある | | |
| 秘密 | 配布物・docs・試験データに秘密と個人情報が無い (12 §6 の PC-1・PC-2・PC-6) | | |

## 2. 配布物 (12 §5)

| 確かめること | 結果 | 日・候補 |
|---|---|---|
| `release.yml` の build が、下書きのリリースに zip・`SHA256SUMS`・`release.json` を置いた | | |
| 手元で `SHA256SUMS` と `release.json` に minisign で署名した (`cargo xtask dist` が示す命令。`-t` は `-m` の前) | | |
| `release.yml` の verify (手で動かす) が通った (AC-FR-801-1) | | |
| zip の中身: `evorag.exe`・`README.md`・`LICENSE`・`NOTICE`・`THIRD-PARTY-NOTICES.md`・`config.example.toml` | | |
| `evorag --version` の入れ方が `zip` | | |
