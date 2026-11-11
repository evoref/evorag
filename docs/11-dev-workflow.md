# 11. 開発運用 (Development Workflow)

| 項目 | 内容 |
|---|---|
| 文書ID | 11-dev-workflow |
| 状態 | ドラフト |
| 前提 | [00-overview](00-overview.md) / [08-roadmap](08-roadmap.md) / [09-testing-quality](09-testing-quality.md) |
| 正本の概念 | リポジトリ構成、タスクと PR の形、`spec:` / `verifies:`、ブランチ・コミット・リリースの規約、`CLAUDE.md` / `.claude/` |
| 主対象 crate | 全体 |

> 仕様書駆動 ([ADR-0001](10-decisions.md#adr-0001-仕様書駆動での開発)) の運用面を定める。品質ゲートとリリース手順の正本は [09](09-testing-quality.md)。

---

## 1. リポジトリ構成

```
evorag/
  Cargo.toml            # workspace (M0 で置く)
  crates/               # evorag-* の lib crate (02 §4)
  bins/evorag/          # bin crate
  xtask/                # 開発用の作業コマンド (cargo xtask ...)
  tests/                # workspace 横断の試験・固定の見本 (golden fixture)
  locales/              # 文言カタログ (ja.toml、en.toml)
  docs/                 # 仕様書 (本書群。正本)
  .cargo/config.toml    # cargo の別名、Windows の CRT の静的リンク
  .config/nextest.toml  # cargo-nextest の profile (M1.1 で置く)
  .github/              # CI、Issue と PR のテンプレート、dependabot.yml
  .claude/              # エージェント向けの設定と hook
  deny.toml             # cargo-deny (依存のライセンス・脆弱性・取得元)
  deps-policy.toml      # 依存規則 (02 §4.3) の入力。xtask check-deps が読む (M1.1 で置く)
  rust-toolchain.toml   # Rust の版の固定 (§1.1)
  rustfmt.toml / clippy.toml
  LICENSE / NOTICE / README.md (英語) / README.ja.md (日本語、正) / CONTRIBUTING.md / SECURITY.md / CHANGELOG.md
```

- 改行は LF に固定する (`.gitattributes`)。試験データは `-text` で改行の変換を止める。
- 秘密鍵 (リリースの署名鍵など) はリポジトリに置かない (`.gitignore` で `.secret/`・`*.key` などを除外。§9.7)。
- `Cargo.lock` はリポジトリに入れる (配布物を作る bin の workspace のため)。

### 1.1 Rust の版

- 使う Rust の版は `rust-toolchain.toml` で 1 つに固定し、手元と CI で同じ版を使う (確定 (ユーザー回答、2026-11-09))。今の版は 1.99。
- workspace の `rust-version` (MSRV) は、固定した版と同じにする。古い版で作れることは約束しないので、MSRV の CI のジョブは持たない。
- 下限の理由: redb 4.5 は 1.90 以上、std の `File::try_lock` (03 §9) は 1.89 以上が要る。
- 版を上げるときは、`rust-toolchain.toml` と `rust-version` を同じ PR (`chore:`) で上げ、CI が全部通ることを確かめる。

## 2. 仕様と実装の対応 (`spec:` / `verifies:`)

- 仕様の節は `<文書番号> §<節>` (例: `03 §5.2`) で参照する。受入基準は `AC-<要件ID>-<n>` (01 §1.4)。
- コミットのフッタと試験のコメントに次を書く。
  - `spec: 03 §5.2` (実装の根拠の節)
  - `verifies: AC-FR-301-1` (その試験が判定する受入基準)
- v0.1 の間は、受入基準の一覧 `tests/ac-checklist.toml` (人が保守する) で、各 AC に試験か手動の確認手順が対応していることを確かめる。生成による突合 (`xtask trace`) は v0.2 で入れる (確定、[OQ-19](10-decisions.md#oq-19-トレーサビリティの機械化の時期))。

## 3. タスクの単位と PR の形

- **1 タスク = 1 作業ブランチ = 1 PR = `develop` 上の 1 コミット** (squash)。
- タスクは「動詞 + 対象」で 1〜3 日に収まる大きさに切る (例: 「封筒のチェックサムを検証する」)。
- PR の本文には、(1) 何を・なぜ、(2) 根拠の仕様の節、(3) 通した AC、(4) セルフレビューのチェックリスト (09 §8.1) を書く。
- **仕様の変更規律**: 実装中に仕様と食い違う判断が要るときは、コードではなく **仕様書を先に直す提案** をする (`spec/` ブランチの PR、または 10 に OQ を起票して `needs-oq` ラベル)。仕様を黙って外れた実装を入れない。

## 4. `CLAUDE.md` / `.claude/`

- リポジトリ直下の `CLAUDE.md` に、エージェント向けの要約 (仕様書駆動、ブランチ運用、コミットの規約、サブエージェントに任せるときの注意) を置く。本書が正本で、`CLAUDE.md` は要約だけを持つ。
- `.claude/settings.json` の PreToolUse hook (`.claude/hooks/guard-git.py`) が、§9.1 の運用を機械的に守らせる。
  - 拒否: `main` / `develop` への push と削除、force push (`--force`・`-f`・`+refspec`)、`main` / `develop` 上の commit / merge / rebase など、`v*` タグの作成と push、`git stash`。
  - 許可: 作業ブランチ (`feat/`・`fix/`・`spec/`・`chore/`・`spike/`・`agent/`) への `--force-with-lease` (作業ブランチの中の rebase の後の push)、作業ブランチのリモートのブランチの削除 (マージ後の片付け)。それ以外の名前の削除は確認。
  - 確認: `git reset --hard` / `git clean -f` / `git restore` / `git checkout -- <path>`、`gh pr merge`。
- hook に拒否されたら、回避せずに理由を人間に伝えて指示を待つ。

## 5. 決定的な試験

- 試験は時刻・乱数・ネットワークに依存させない。時刻は `Clock` の trait、ID は種を固定した生成器で注入する (02 §6)。
- llama-server を使う試験は、応答を記録して再生する形 (09 §3.3) を既定にする。実際のサーバを使う試験は別の入口 (`cargo xtask live-llm`) で、明示したときだけ流す。

## 6. コードレビューと CI の運用

- CI の車線と必須のチェックは [09 §6](09-testing-quality.md#6-ci-github-actions)。
- 速い車線 `cargo xtask check` (fmt・clippy・単体試験・依存規則) を、PR を出す前に手元で通す。
- ラベル: `spec` (仕様の変更)、`needs-oq` (仕様と違う判断がある)、`area:<crate>`、`changelog: none`。

---

## 9. ブランチ・コミット・リリースの規約

### 9.1 ブランチ

長命ブランチは **`main` (リリース専用) と `develop` (開発の本線) の 2 本だけ** (確定 (ユーザー回答)、[OQ-01](10-decisions.md#oq-01-ブランチと公開の運用))。

| ブランチ | 用途 | 規則 |
|---|---|---|
| `main` | リリース専用。リリースした版だけが入り、各リリースのマージコミットにタグ (`v0.1.0` 等) が付く。v0.1.0 のリリースまでは初期コミット (`LICENSE`・`README.md`・`.gitignore`) のまま変えない | 直接 push・force push・削除の禁止。PR は `develop` (リリース) からだけ受け、merge は人間の指示で行う (merge commit、下の「merge の実行」)。タグは人間が署名して付ける |
| `develop` | 開発の本線。作業ブランチの取り込み先で、リリースの起点。v0.1.0 のリリースまでは既定ブランチにする。Dependabot のバージョン更新の宛先 | 直接 push・force push・削除の禁止。PR は作業ブランチ (squash)・Dependabot (squash)・`main` (リリース後の取り込み、merge commit) からだけ受け、必須の CI を通す |
| `<種別>/<crate>-<短い説明>` | 作業ブランチ。種別は `feat` / `fix` / `spec` (docs) / `chore` / `spike`。`develop` から作り、PR の宛先は `develop` | 例: `feat/evorag-store-envelope`、`spec/05-retrieval-budget`。エージェントは `agent/<名前>/<Issue 番号>-<短い説明>`。merge 後に消す |
| `spike/S-nn` | 技術検証 (08 §3) | `spikes/S-nn/` 以下のみ。merge せず `REPORT.md` だけを `spec/` ブランチで取り込む |

**流れ**: 作業ブランチ → PR (squash) → `develop` → リリース時に PR (merge commit) → `main` → 人間がマージコミットに署名付きタグを付けて GitHub Releases に公開 (09 §9) → `main` を `develop` に取り込む (PR、merge commit)。

- **merge の実行** (確定 (ユーザー回答、2026-11-11)): PR を入れるかどうかは人間が決め、PR ごとに指示する。実行は、指示を受けたエージェントが `gh pr merge` で行う (人間が自分で同じコマンドを打ってもよい)。GitHub の画面など、ほかの方法では入れない。
  - 作業の PR は `gh pr merge <番号> --squash`、`develop` ↔ `main` の PR は `gh pr merge <番号> --merge`。コマンドの squash は、件名が PR の題、本文が元のコミットのメッセージになり、`Signed-off-by:` が残る。
  - ほかの方法をやめた理由: `gh pr merge` 以外の方法で squash したコミットのメッセージが「Merge pull request #N …」に置き換わり、`Signed-off-by:` が落ちることが 3 回あった (#70・#76・#78、08 §6)。3 回とも、merge commit で入れた PR の直後の squash だった。
  - `--delete-branch` は付けない (手元の作業ツリーを `develop` に切り替えるため)。作業ブランチは、入れた後に別に消す。
  - 入れた後に、取り込んだコミットのメッセージに `Signed-off-by:` があることと、`develop` の push の CI が通ることを確かめる。落ちていたら、次の作業の前に直す。
  - hook は `gh pr merge` のたびに確認を出す (§4)。人間が承認しなければ入らない。
  - タグの作成と push、配布物の署名、Release の公開は、人間だけが行う。
- **版番号**: v0.1.0 のリリースまでの workspace の版は内部カウンタ `0.0.x` で、`x` はマイルストーン (08 §4) を `develop` に取り込む PR で 1 つ上げる。`0.0.x` にはタグを付けない。リリース PR で workspace の版と CHANGELOG を `0.y.z` に直す。版の比較は数値順 (`git tag --sort=version:refname`)。
- **機能リリース** は `0.y.0` 単位、**bugfix リリース** はパッチ版 (`0.y.z` の z)。パッチ版を出せる条件は、前回のタグ以降の `develop` に `feat` が 1 件も入っていないこと。機能リリースの直後からパッチ版を出し終えるまでは `feat` の PR の merge を後回しにする。
- **凍結**: リリース手順の凍結 (09 §9.2 の手順 1) から昇格とタグ (手順 4) までは、`develop` への merge (Dependabot の PR を含む) を保留する。例外は `main` → `develop` の取り込みだけで、版上げ (手順 2) の前に限る。
- **プレリリース** (`-rc.N` など) は出さない (09 §9.1)。
- `dev/<版>`・`release/*`・`hotfix/*` のブランチは作らない。
- rebase は作業ブランチ内で自由。rebase の後の push は `--force-with-lease` で行う。保護したブランチへの force push は禁止 (GitHub のブランチ保護と `.claude/settings.json` の両方で拒否)。
- **ブランチ保護** (`main`・`develop`): PR 必須、直接 push・force push・削除の禁止、必須のステータスチェック (09 §6.2。存在するジョブだけを必須にし、ジョブを足したら加える)、管理者もバイパス不可。単独開発の間は承認必須を 0 にし、代わりにセルフレビューのチェックリストを PR に貼る。タグ `v*` はルールセットで保護する。
- **最初の push** (08 §2.1 の M0.2): リポジトリは最初から public にする ([OQ-28](10-decisions.md#oq-28-公開の時期と外部寄稿の受付))。(1) `main` に初期コミット (`LICENSE`・`README.md`・`.gitignore`) を作る。(2) `main` から `develop` を作り、docs/ と初期ファイル (`NOTICE`・`.gitattributes`・`CONTRIBUTING.md`・`SECURITY.md`・`CLAUDE.md`・`.claude/`・`.github/`・設定ファイル) をコミットする (DCO の署名を付ける)。(3) 公開前チェック ([12 §6](12-licensing-distribution.md#6-公開前チェック)) を作業ツリーと全履歴に対して行う。(4) `main` と `develop` を push する。(5) 既定ブランチを `develop` にし、ブランチとタグの保護と private vulnerability reporting を設定する。(1) と (2) は hook が `main` / `develop` 上の commit を拒否するため、人間が行う。

### 9.2 コミットメッセージ

Conventional Commits を基本にし、仕様の節と AC をフッタに置く。

```
feat(evorag-store): 封筒のチェックサムを読むたびに検証する

03 §8 のとおり、封筒の xxh3 が合わない記録は quarantine に移し、他の記録は読み続ける。

spec: 03 §3.2, 03 §8
verifies: AC-NFR-303-1
Closes #12
```

| 要素 | 規則 |
|---|---|
| 1 行目 | `<type>(<scope>): <要約>`。type は `feat` / `fix` / `perf` / `refactor` / `test` / `docs` / `spec` (仕様の変更) / `chore` / `ci` / `spike`。scope は crate 名 (`evorag-store`) |
| 本文 | 何を・なぜ。仕様の節を引く |
| フッタ | `spec:` / `verifies:`、`Closes #n`、仕様と違う判断があれば `needs-oq:` |
| エージェント | 本文末尾に `Agent: <名前>`。DCO の `Signed-off-by:` は **エージェントを操作した人間の名義**。`Co-Authored-By:` 行は残してよい (DCO の表明の代わりにはならない) |
| 言語 | 日本語を基本にする。英語でもよい。混在させない |

### 9.3 CHANGELOG とリリース

- `CHANGELOG.md` は Keep a Changelog 形式で、PR ごとに Unreleased へ 1 行足す。利用者に見えない変更は `changelog: none`。
- 版上げと CHANGELOG の確定は、`develop` に対するリリース PR (`chore(release): v0.1.0`) で、リリースゲートの判定の前に行う (09 §9.2 の手順 2)。`develop` → `main`・`main` → `develop` の PR は merge commit で、人間が作り、人間の指示で merge する (§9.1 の「merge の実行」)。
- リリースの前に、リリースノートの「更新の方法」とデータの移行の有無、利用者向け README の「更新と巻き戻し」の節がその版と合っているかを確かめる (09 §8.2)。
- **リポジトリの README** は、英語版 (`README.md`) を表に置き、冒頭に日本語版 (`README.ja.md`) へのリンクを置く (日本語版の冒頭にも英語版へのリンクを置く)。日本語版を正とし、2 つを同じ PR で直す。
  - **版を上げるたびに直す** (機能リリースとパッチ版の両方)。リリース PR (09 §9.2 の手順 2) で、冒頭の「最新の版」(版と日付、配布物の対象の OS) と「状況」の節 (その版で届いたもの、次の版の予定) を、その版に合わせる。
  - 版の間でも、README に書いてあること (対応する OS、必要なもの、公開鍵、ブランチ、寄稿の受付) を変える PR は、同じ PR で README を直す。

### 9.4 寄稿ポリシー (DCO)

- 寄稿は **DCO** (確定 (ユーザー回答)、[OQ-02](10-decisions.md#oq-02-寄稿ポリシー))。すべてのコミットに `Signed-off-by:` を付ける (`git commit -s`)。メンテナと AI エージェントのコミットも同じ。DCO の検査 (09 §6.2 の `dco` ジョブ) は最初の CI から入れる。Dependabot は署名を付けられないので、Dependabot が作ったコミットだけは検査から除く (人間がそれを squash で入れるときは、squash のコミットメッセージに `Signed-off-by:` を足す)。 ほかに、履歴を書き換えられない過去のコミットで、中身が DCO の表明つきのものは、`.github/dco-exempt` に理由つきで書いて除く (v0.1.0 の前に GitHub が作った `6b93267` と、squash で `Signed-off-by:` が落ちた `02e4b3e`・`1d716e7`・`185e52e`、08 §6)。squash は `gh pr merge --squash` で行う (§9.1 の「merge の実行」)。
- 公開してよい著者名とメールアドレスだけを使う (リポジトリの `user.email` を設定する)。
- 外部からの PR の受付は、受付の条件 ([12 §3](12-licensing-distribution.md#3-寄稿)) が整ってから始める。それまでは `CONTRIBUTING.md` に「まだ受け付けていない」と書く。

### 9.5 依存の追加

- 依存を足す PR は、採用の理由・代替案・ライセンス・C を含むか・推移的な依存の増分を本文に書く (00 §4 の P7)。
- `deny.toml` (ライセンスの許可リストと取得元) と `deps-policy.toml` (crate ごとの禁止依存、推移的な依存の上限、C を含む依存 0) を CI で検査する (09 §7)。

### 9.7 秘密情報の扱い

- リリースの署名鍵 (minisign) はリポジトリに置かない。置き場所と保管の方法は人間が決め、本書には書かない。公開鍵はリポジトリ直下の `minisign.pub` に置き、README からも参照できるようにする (12 §5)。
- llama-server の API キー、利用者の会話の実例、個人情報を docs・試験データ・Issue に書かない。試験の会話は作った例だけを使う。
- CI に gitleaks 相当の秘密の検出を入れる (09 §6.2)。

### 9.8 脆弱性の受付・対応・開示

- 受付は GitHub の private vulnerability reporting (`SECURITY.md`)。目安は受領の連絡 7 日以内、最初の判断 14 日以内、修正版は重大なもので 30 日以内。
- 修正版の公開と同時に GitHub Security Advisory を公開する。
- 利用者のデータの形式に影響する修正 (移行を含む) は、リリースノートに退避と巻き戻しの手順を書く。
