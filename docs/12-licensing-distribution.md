# 12. ライセンス・配布・更新 (Licensing, Distribution and Update)

| 項目 | 内容 |
|---|---|
| 文書ID | 12-licensing-distribution |
| 状態 | ドラフト |
| 前提 | [00-overview](00-overview.md) / [11-dev-workflow](11-dev-workflow.md) |
| 正本の概念 | 本体と依存のライセンス、寄稿の受付、公開する範囲、配布物と署名、公開前チェック、**本体の更新** (確認・適用・巻き戻し)、名称 |
| 主対象 crate | `evorag-update`、全体 |

> 本書は法的な助言ではない。

---

## 1. 本体のライセンス

- 本体は **Apache License 2.0** 単独 (確定 (ユーザー回答))。リポジトリ直下に `LICENSE` と `NOTICE` を置く。
- 配布物に `LICENSE`・`NOTICE`・`THIRD-PARTY-NOTICES.md` (依存のライセンスの表示、`cargo about` で作る) を同梱する。

## 2. 依存のライセンス

- 許可リストは `deny.toml` が正本 (MIT、Apache-2.0、Apache-2.0 WITH LLVM-exception、BSD-2-Clause、BSD-3-Clause、ISC、Zlib、Unicode-3.0、Unicode-DFS-2016、0BSD、BSL-1.0、CC0-1.0)。GPL・AGPL・LGPL は使わない。
- MPL-2.0 は個別に承認する。承認したら `deny.toml` の `exceptions` に crate 名つきで足し、理由を PR に書く。TLS の暗号の実装は純 Rust の graviola を使う (OQ-05。ライセンスは Apache-2.0 OR ISC OR MIT-0 で、許可リストの Apache-2.0 で通る)。証明書のルートは、OS の検証器 (rustls-platform-verifier) を使い、同梱のルートの一覧 (webpki-roots・webpki-root-certs) は使わない (S-06。同梱の一覧のライセンス CDLA-Permissive-2.0 は許可リストの外)。

## 3. 寄稿

- 寄稿は DCO (11 §9.4)。inbound = outbound (寄稿も Apache-2.0)。
- **外部からの PR の受付**: PO が別途指示するまで受け付けない ([OQ-28](10-decisions.md#oq-28-公開の時期と外部寄稿の受付))。受付を始めるときは、完全な `CONTRIBUTING.md` (開発環境・試験の流し方・PR の書き方) を先に置く。
- 受付の前に届いた PR は、内容を確認せずに閉じることがある (`CONTRIBUTING.md` に書く)。

## 4. 公開する範囲

- リポジトリは **最初から public** で開発し、docs/ も最初の push から公開する (確定 (ユーザー回答、2026-11-09)、[OQ-28](10-decisions.md#oq-28-公開の時期と外部寄稿の受付))。
- 公開前チェック (§6) は、最初の push の前に作業ツリーと全履歴に行う。以後のコミットはすべて公開されるので、秘密と個人の情報を入れない (11 §9.7)。
- `main` の docs/ がリリースした版の仕様、`develop` の docs/ が作業中の版の仕様である。
- 公開しないもの: 署名の秘密鍵、利用者の会話の実例、開発者の PC の型番などの個人の情報 (11 §9.7)。

## 5. 配布物と署名

### 5.1 配布物

| 版 | 対象 | 形 |
|---|---|---|
| v0.1 | Windows 11 x86_64 (`x86_64-pc-windows-msvc`、C ランタイムを静的にリンク) | zip |
| v0.2 | Linux x86_64 (`x86_64-unknown-linux-musl`、静的) | tar.gz |
| v0.6 | macOS (`aarch64-apple-darwin`、実機を確保したとき) | tar.gz |

- 配布物の中身: 実行ファイル、`README.md` (利用者向け。インストール・setup・更新と巻き戻し・既知の制約)、`LICENSE`、`NOTICE`、`THIRD-PARTY-NOTICES.md`、`config.example.toml`。
- `cargo install --locked --git https://github.com/evoref/evorag --tag v0.y.z evorag` でも入れられる (crates.io への公開は v1.0 の前に決める)。

### 5.2 署名

- 各リリースに、`SHA256SUMS` (配布物ごとの SHA-256) と **署名つきのマニフェスト** `release.json` (§5.3) を付け、どちらも minisign で署名する (`.minisig`)。署名は prehash の形 (`minisign -S` の既定) で作る。
- `release.json` の署名の trusted comment は、`evorag <版> release.json sha256 <SHA-256>` とちょうど同じ文字列にする。`<版>` は `release.json` の `version`、`<SHA-256>` は `release.json` のバイト列の SHA-256 (小文字の 16 進 64 桁)。`cargo xtask dist` がこの文字列を示し、`cargo xtask verify-release` は完全に一致することを確かめる。
- **確かめ方** (`evorag update`、§7.2):
  - prehash でない古い形の署名 (legacy) は断る。
  - 署名の鍵の ID と同じ ID を持つ鍵を、すべて試す (同じ ID の鍵が複数あってもよい)。利用者が足した鍵 (`update.trusted_keys`) は、埋め込んだ鍵に足すだけで、置き換えない。
  - trusted comment は、全体の署名 (global signature) が通った後に比べる。前後の空白や余分な語を含めて完全に一致しなければ断る。
  - `update.trusted_keys` の鍵で通ったときは、その鍵の ID を示して警告する (埋め込んだ鍵ではないことを利用者が気づけるように)。
- **鍵**: リリースの署名鍵は、カートリッジの署名鍵 (14) とは分ける。
  - 公開鍵はリポジトリ直下に置く。今の鍵は `minisign.pub`、次の鍵は `minisign.next.pub`。README に 2 本の鍵の ID を書く。
  - 本体には **今の鍵と次の鍵の 2 本** を埋め込む。どちらかの鍵で署名が通れば合格とする。実装は鍵を何本でも持てるようにする。
  - 次の鍵は、v0.3.0 のリリースの前に、人間が手元で (オフラインで) 作り、公開鍵を `minisign.next.pub` に置く。エージェントは鍵を作らない。次の鍵が置かれるまで v0.3.0 をリリースしない (埋め込む鍵が 1 本だと、今の鍵を失ったときに `update` で更新できる版が無くなるため)。
  - 今の鍵と次の鍵の秘密鍵は、別々の場所に保管する (1 か所を失っても両方を失わないように)。
  - 鍵の交代: 次の鍵で署名した版を出す。その版では、次の鍵を今の鍵 (`minisign.pub`) にし、さらに次の鍵を作って `minisign.next.pub` に置き、2 本を埋め込む。今の鍵が漏れたときも、次の鍵で署名した版で入れ替えられる。
  - 漏れた・失った (失効した) 鍵は、二度と埋め込まない。
  - 利用者は、設定 `update.trusted_keys` (07 §4.2) で鍵を足せる。埋め込んだ鍵は外せない。
- **残るリスク (鍵の失効)**: 失効した鍵の一覧は持たない。漏れた鍵は、その鍵を埋め込んだ版を使っている間は信頼され続け、その鍵を外した版へ更新して初めて信頼されなくなる。漏れたときは、次の鍵で署名した版を出し、リリースノートと README で告知する。
- 署名は人間が手元で (オフラインで) 行う。秘密鍵を CI の secrets に置かない (11 §9.7)。
- コード署名 (Authenticode・Apple の公証) は v1.0 までに検討する。それまでは、初回の起動で SmartScreen・Gatekeeper の警告が出ることを README に書く。

### 5.3 `release.json` の形式

`release.json` は UTF-8 の JSON の object で、`cargo xtask dist` (09 §9.2 の手順 5) が作る。形式の版 (`schema`) は `1`。

| 項目 | 型 | 内容 |
|---|---|---|
| `schema` | 整数 | 形式の版 (`1`) |
| `name` | 文字列 | `"evorag"` |
| `version` | 文字列 | 版 (semver、先頭の `v` なし)。タグは `v<version>` |
| `date` | 文字列 | リリースの日付 (`YYYY-MM-DD`、UTC)。タグの指すコミットの committer の日時 (`git log -1 --format=%ct`) を UTC の日付にしたもの。配布物の実行ファイルに埋め込む `EVORAG_RELEASE_DATE` と同じ値で、対象ごとに別の機械で作っても同じ値になる |
| `upgrade_from` | 文字列 | `evorag update apply` でこの版へ更新できる、今の版の下限 (semver)。値は workspace の `Cargo.toml` の `[workspace.metadata.evorag]` の `upgrade_from` から取る |
| `notes` | 文字列 | 変更の要点。CHANGELOG のその版の節から作る平文で、UTF-8 で 4 KiB 以内。無くてもよい |
| `artifacts` | 配列 | 対象ごとの配布物 (下の表) |
| `db_format` | 文字列 | この版が書く DB の形式の版 (03 §3.1.1 の `format.version` と同じ書き方) |
| `db_min_write` | 文字列 | この版が移行した後の DB の `format.min_write` (03 §3.1.1)。無くてもよい |
| `llama_server_min_build` | 整数か null | 対応する llama-server の build 番号の下限 (06 §10)。決めていなければ null |

`artifacts` の各要素:

| 項目 | 型 | 内容 |
|---|---|---|
| `target` | 文字列 | 対象 (Rust の target triple、§5.1) |
| `name` | 文字列 | 配布物のファイル名 (Releases の asset の名前)。使える文字は `A-Z`・`a-z`・`0-9`・`.`・`_`・`+`・`-` だけで、`..` を含まない |
| `sha256` | 文字列 | 配布物の SHA-256 (小文字の 16 進 64 桁) |
| `size` | 整数 | 配布物の大きさ (バイト) |
| `exe` | 文字列 | 配布物の中の実行ファイルのパス (`/` で区切る。例: `evorag-0.3.0-x86_64-pc-windows-msvc/evorag.exe`)。`..`・絶対パス・`\` を含まない。配布物の中の項目と比べるためだけに使い、書き出す先のパスには使わない |
| `exe_sha256` | 文字列 | 実行ファイルの SHA-256 (小文字の 16 進 64 桁) |

- `cargo xtask dist` は、すべての項目を書く。`exe` と `exe_sha256` は、対象ごとのビルドの段で書く (`--manifest-only` で目録を作り直すときは、配布物を読んで求める)。
- **読む側の規則** (`evorag update`、§7.2):
  - 知らない項目は、最上位でも `artifacts` の要素の中でも無視する (後の版で項目を足せるように)。
  - 必須の項目は、`schema`・`name`・`version`・`date`・`upgrade_from`・`artifacts` と、`artifacts` の各要素の 6 つ。欠けている・型が違う・`name` や `exe` が上の条件に合わないときは断る。`notes`・`db_format`・`db_min_write`・`llama_server_min_build` は無くても断らない。
  - `schema` が無い、または `1` でないときは、更新を断り、手作業の更新の手順 (§7.1) を案内する。
  - `notes` が 4 KiB を超えるときは、断らずに 4 KiB (文字の境界) で切る。
- **配布物の選び方**: `target` が今の対象と同じで、`name` の末尾がこの版で読める形式のもの (v0.3 は、Windows の対象で `.zip`、Linux・macOS の対象で `.tar.gz`) を探す。ちょうど 1 つならそれを使い、0 か 2 つ以上なら断る。形式は `name` の末尾で決める (ビルドの `cfg` で決めない)。同じ対象に別の形の配布物 (インストーラーなど) を足すときは、この条件に当たらない名前にする。
- **`upgrade_from` の決め方**: v0.1〜v0.4 は、移行の関数を直前の版の分だけ持つ (03 §12.1) ので、`upgrade_from` は直前の機能リリースの `.0` 以上にする (例: v0.4.0 は `0.3.0` 以上)。今の版が下回るときは、`evorag update apply <upgrade_from>` を先に実行するよう案内する。
- **形式を変えるとき**: 項目の意味を変える・必須の項目を足すときは `schema` を上げ、新しい形式は別の名前 (`release.v2.json` と `release.v2.json.minisig`) で出す。`release.json` (schema 1) は、その版を schema 1 で書き表せる間は並べて出し続ける (古い版の `update` が読めるように)。
- v0.3.0 より前の版の `release.json` は `schema`・`upgrade_from`・`exe`・`exe_sha256` を持たない。それらの版は更新の先にならない (今と同じか古い版は拒否する、§7.2) ので、読めなくてよい。
- 各リリースの `release.json` は、試験の見本として `tests/fixtures/release/<版>/` に残し、schema 1 の読み手の約束を試験で保つ (09 §3.6)。

## 6. 公開前チェック

最初の push (08 §2.1 の M0.2) の前に全項目を、各リリースの前に PC-1・PC-2・PC-6 を行う。

| ID | 確かめること |
|---|---|
| PC-1 | 作業ツリーと全履歴に、秘密 (鍵・トークン・API キー) が無い (gitleaks で全履歴を走査する) |
| PC-2 | 個人の情報 (会話の実例、開発者の PC の型番、私的なパス) が無い |
| PC-3 | コミットの著者名とメールアドレスが公開してよいもの |
| PC-4 | `LICENSE`・`NOTICE`・`README.md`・`CONTRIBUTING.md`・`SECURITY.md` がそろっている |
| PC-5 | GitHub の設定: private vulnerability reporting を有効、ブランチ保護 (`main`・`develop`)、タグ `v*` の保護、既定ブランチ (`develop`)、Actions の権限を最小に |
| PC-6 | 試験データの会話がすべて作ったもの |

## 7. 本体の更新

### 7.1 手作業の更新 (v0.1〜v0.2 と、`update` を使えないとき)

本体の更新機能は v0.3 で入れる (確定 (ユーザー回答)、[OQ-04](10-decisions.md#oq-04-更新機能の時期))。v0.1〜v0.2 と、v0.3 からも `update` を使えないとき (CPU の機能や証明書のルートが無い、通信できない、`release.json` の形式を読めない、など。§7.2・§7.5) は、同梱の README の次の手順で更新する。

1. Releases (`https://github.com/evoref/evorag/releases`) から新しい配布物と `SHA256SUMS`・`.minisig` を取得する。
2. minisign で署名を確かめる (コマンドの例を README に書く)。
3. evorag を終了し、実行ファイルを置き換える。古い実行ファイルは `evorag.prev` として残す。`evorag.exe` が無くなったとき (置き換えの途中で止まったなど) は、§7.3 の「Windows の隙間」の戻し方に従う。
4. 新しい版の初回の起動で、移行が要るときは自動で退避してから移行する (03 §12)。
5. 巻き戻すときは、`evorag.prev` に戻す。新しい版で移行した後なら、`evorag restore` で移行の前の退避 (`backups/pre-op/`) から戻す (戻さなければ、古い版は読むだけで起動する、03 §4 の規則 6)。

- データの移行と退避の仕組みは v0.1 で入れるので、手作業の更新でもデータは失わない。

### 7.2 v0.3 から: `evorag update`

| コマンド | 内容 |
|---|---|
| `evorag update check` | リリースの一覧を取得し、今の版より新しい版があれば、いちばん新しい版の `release.json` を検証して、その版・リリースの日付・変更の要点を示す。何も書き換えない |
| `evorag update apply [版]` | 配布物を取得し、検証してから置き換える。版を省くと、いちばん新しい版にする |
| `evorag update rollback` | `evorag.prev` に戻す。通信しない |

- 終了コードは 07 §5。示す文字列は、検証した `release.json` から取ったものだけにする (リリースの一覧の `published_at` や本文は示さない)。通信の応答や子のプロセスの出力を示すときは、`notes` と同じ規則 (apply の 5) で除き、4 KiB で切る。

**check**:
- CPU の機能と証明書のルート (§7.5) だけを確かめてから通信する。ロックを取らず、片付け (§7.3) をせず、データ根・インストール先・`update/` のファイルに触れない。
- いちばん新しい版の選び方は apply の 1、検証は apply の 2〜4 と同じ。新しい版が無ければ、そう示して 0 で終わる。
- 検証で断ったときは、改ざんを疑う理由 (07 §5 の 5) なら 5、そうでなければ 1 で終わる。今の版が `upgrade_from` を下回るときは、断らずに版を示し、先に `evorag update apply <upgrade_from>` を実行するよう案内する (0)。

**始める前の確かめ** (apply と rollback。どれかに当たれば、インストール先を変えずに理由を示して断る):
1. 動いている実行ファイルの名前が `evorag.exe` (Windows) か `evorag` (Linux・macOS) である (§7.3 の手順がこの名前を前提にするため)。
2. 入れ方が zip である (§7.4)。
3. 状態のフォルダの `update/lock` を取れる (§7.3 の「更新のロック」)。取れなければ、ほかの `update` が動いている。
4. 状態のファイル (下の「状態のファイル」) を読む。
   - 計画が読めないときは断り、§7.3 の「手で戻す手順」を示す。
   - 計画の `dir` が、動いている実行ファイルのフォルダ (正規化したパス) と違うときは、その計画に触れずに断り、計画のフォルダを示す。
   - 計画が同じフォルダのものなら、データ根のロックを取ってから続きを済ませる (§7.3 の「続きと片付け」)。
5. 計画が無ければ (4 で済ませた後を含む) 名前で片付け (§7.3) をし、残ったファイル (`evorag.new`・`evorag.tmp`・`evorag.prev.old`・`evorag.rejected`・`evorag.probe`) を消せなければ断る (後の戻し方の案内が、残ったファイルで誤らないように)。
6. インストール先を確かめる (§7.3 の「インストール先の確かめ」)。
7. 設定のデータ根 (`--data-root`・`storage.data_root`) の `lock` (03 §9) を取れることを確かめ、すぐ放す。取れなければ evorag が動いているので、閉じてから実行するよう案内する。確かめるのは設定のデータ根だけで、ほかのデータ根を使う evorag が動いていても分からない。データ根がまだ無ければ確かめない。
8. (apply で版を指定したとき) 指定した版が今と同じか古ければ、通信の前に断る (古い版に戻すのは `rollback`、と案内する)。
9. (apply だけ) CPU の機能と証明書のルートがある (§7.5)。ここまでは通信しない。

**apply の流れ**:
1. **版を決める**: 版を指定しなければ、リリースの一覧 (§7.5) を 1 回引く。`tag_name` が `v<semver>` に完全に一致しないもの・draft・prerelease・semver の pre-release (`-` の付く版) を除き、semver でいちばん大きい版を選ぶ (一覧の並びの順には頼らない)。今の版より大きくなければ、新しい版は無いと示して 0 で終わる。版を指定したときは一覧を引かない。
2. `release.json` と `release.json.minisig` を、§7.5 の固定の URL から取得する。取得したものはディスクに書かず、主記憶で扱う。
3. **署名を確かめる** (§5.2): 本体に埋め込んだ鍵と設定 `update.trusted_keys` のうち、署名の鍵の ID と同じ鍵で、`release.json` のバイト列の署名が通ること (prehash の形だけ)。全体の署名が通った後に、trusted comment が `evorag <版> release.json sha256 <SHA-256>` と完全に一致し、`<SHA-256>` が取得したバイト列の SHA-256 であること。
4. **中身を確かめる** (署名が通った後に初めて JSON として読む。形式と読む側の規則は §5.3):
   - `schema` が `1`。違えば、手作業の更新の手順 (§7.1) を案内する。
   - `name` が `evorag`。`version` が、trusted comment の版・取得した URL のタグの版・指定した版 (指定したとき) のすべてと同じ。
   - 版が今より新しい。**今と同じか古い版は拒否する** (巻き戻しは `rollback` だけ)。
   - `date` が、今の版のリリースの日付 (実行ファイルに埋め込んだ `EVORAG_RELEASE_DATE`、§5.3) より古くない (凍結への備え、§7.5)。開発のビルド (`EVORAG_CHANNEL` が `zip` でなく、日付を埋め込んでいない) では確かめない。
   - 今の版が `upgrade_from` 以上。下回れば、先に `evorag update apply <upgrade_from>` を実行するよう案内する。
   - 今の対象の配布物を §5.3 の「配布物の選び方」で 1 つに決められる。
5. **示す**: 版、リリースの日付 (`date`)、変更の要点 (`notes`)。`notes` が無ければ、リリースのページの URL だけを示す。検証の前の内容は示さない。
   - `notes` から、Unicode の制御文字 (Cc) のうち LF 以外 (CR を含む) と、双方向の書式の文字 (U+202A〜U+202E、U+2066〜U+2069) を除く。4 KiB を超えれば切る (§5.3)。
   - `llama_server_min_build` が null でなければ、対応する llama-server の build 番号の下限を示す。
   - `db_min_write` が今の版の書く DB の形式の版より新しければ、「この版で移行した後に今の版へ戻すと、今の版ではデータが読むだけになる (書けるようにするには移行の前の退避から戻す)」と警告する。
6. **配布物を取得する**: 大きさの上限は、`size` と 64 MiB の小さいほう (§7.5)。大きさと SHA-256 が `size`・`sha256` と合わなければ、捨てて中止する。
7. **実行ファイルを取り出す**: 配布物を展開の処理に渡すのは、6 を通った後だけ (検証の済んでいないバイト列を圧縮の形式として読まない)。
   - 名前が `exe` と同じ項目を 1 つだけ取り出し、ほかは展開しない。`exe` は比べるためだけに使い、書き出す先のパスに使わない。
   - その項目が通常のファイルでない (tar のリンクなど)、同じ名前の項目が 2 つ以上ある、展開した大きさが 256 MiB を超える、のどれかなら断る。
   - 取り出した実行ファイルの SHA-256 が `exe_sha256` と合うことを確かめる。
8. **書く**: ここからデータ根のロックを取り、終わりまで持つ (置き換えの途中で evorag が起動しないように)。取れなければ断る。インストール先の `evorag.new` を §7.3 の「ファイルの作り方」で書いて書き切る。閉じてから開き直し、SHA-256 が `exe_sha256` と合うことを確かめ直す (検証と置き換えの間の差し替えと、書き込みの誤りを防ぐ)。これが §7.3 の apply の手順 1。
9. 状態のファイルに計画を書いてから (段 1)、§7.3 の手順 2 から置き換える。今の実行ファイルは `evorag.prev` として残す。
10. **置き換えの後の確かめ**: 新しいファイルを `evorag(.exe)` に置いた直後 (前の `evorag.prev` を消す・上書きする前、§7.3) に、新しい `evorag(.exe)` を絶対パスで `--version` を付けて動かす。
    - 標準出力の 1 行目が `evorag <version>` (07 §5) と同じであることを確かめる。読むのは 4 KiB まで。
    - 30 秒で終わらなければ、子のプロセスを止め、終わるのを待ってから元に戻す。
    - 合わない・動かない・時間切れのときは、§7.3 の失敗のときと同じく元に戻し (戻す新しいファイルは `evorag.rejected` にする)、理由を示して 1 で終わる。
    - `--version` は通信せず、データ根・設定・`update/` に触れず、片付けもしない。
11. 状態のファイルの計画を済みにする。
12. 新しい版の初回の起動で、移行が要るときは退避してから移行する (03 §12.1)。移行に失敗したら、新しい版は元の DB のまま読むだけで起動し (03 §12.1)、理由とともに `evorag update rollback` を案内する。

- 1〜8 のどこで断っても、インストール先のファイルは変わらない (`evorag.new` を書いた後なら消す)。

**rollback の流れ**:
1. `evorag.prev` が無ければ断る。
2. **戻す先を確かめる**:
   - 状態のファイルの最後に済んだ操作 (`done`) が読めて、その `current` の SHA-256 が `evorag.prev` と合えば、その版と DB の形式の版を使う。
   - 記録が無い・読めない・合わない (手作業で置いた、§7.1) ときは、`evorag.prev` を apply の 10 と同じ規則 (絶対パス、4 KiB、30 秒) で `--version` を付けて動かす。1 行目が `evorag <版>` でなければ断る。DB の形式の版は、`db format:` の行 (07 §5) があればそれを、無ければ v0.3 が持つ過去の版の表 (0.1.x は `0.1`、0.2.x は `0.2`) から決め、どちらも無ければ分からないとする。
3. **データの案内を求める** (入れ替えの前に求め、入れ替えの後に示す): データ根があれば、そのロックを取り (取れなければ断る)、終わりまで持つ。DB を読むだけで開いて `format.min_read` と `format.min_write` (03 §3.1.1) を読み、戻す版の DB の形式の版と比べる。
   - `min_write` 以上: 戻した版でそのまま使える。
   - `min_read` 以上で `min_write` 未満: 戻した版は読むだけで起動する (03 §4 の規則 6)。書けるようにするには、移行の前の退避から戻す。
   - `min_read` 未満: 戻した版では開けない。移行の前の退避から戻す必要がある。
   - 戻す必要があるときは、移行の前の退避 (`storage.backup_dir`、無ければデータ根の `backups/pre-op/`、03 §7.2。新しいものから 3 世代) を探し、見つかったパスと `evorag restore <退避>` (戻した版で実行する) を示す。退避の後の会話はジャーナルから足されること (03 §10) も示す。見つからなければ、見つからないと示す (データは新しい版でなら使える。`evorag update apply` で戻れる)。
   - `config.toml` の版が上がっていて `config.toml.bak-v<旧版>` (03 §12.3) があれば、そのパスを示す (戻した版が今の設定を読めないときに使う)。
   - 戻す版の形式の版が分からないときは、一般の案内を示す (新しい版で移行した後なら、`backups/pre-op/` の移行の前の退避から `evorag restore` で戻せる。戻さなければ、古い版は読むだけで起動する)。
4. §7.3 の手順で入れ替える。今の実行ファイルは `evorag.prev` になる。
5. 状態のファイルの計画を済みにし、3 の案内を示す。データの案内に警告があっても、入れ替えが済めば 0 で終わる。

**状態のファイル** (状態のフォルダの `update/state.json`):
- JSON で、所有者だけの権限で作る (NFR-503)。一時名に書いて書き切り、改名で置く。最上位に形式の版 `schema` (`1`) を持ち、知らない項目は無視する。
- **計画** (`plan`): apply と rollback が、§7.3 の手順 1 を済ませてから書き (段 1)、段を進めるたびに書き直す。項目は次のとおり。
  - `op`: `apply` か `rollback`。
  - `dir`: インストール先のフォルダ (正規化したパス)。
  - `stage`: 済んだ手順の番号 (§7.3 の表の番号。1 から数える)。
  - `reverting`: 置き換えの後の確かめに通らなかった・途中で失敗したので、元に戻している途中であること (真偽)。
  - 役割ごとの SHA-256・版・DB の形式の版 (分かれば)。役割を見分けるのは SHA-256 だけで、版は示すためだけに使う。
    - `current`: 入れ替えの前の `evorag(.exe)`。apply では動いている自分で、DB の形式の版は自分が書く版。
    - `next`: `evorag(.exe)` に置くもの。apply では `release.json` の `version`・`exe_sha256`・`db_format`、rollback では入れ替えの前の `evorag.prev`。
    - `old_prev`: 入れ替えの前の `evorag.prev` (apply で、あれば)。
- **最後に済んだ操作** (`done`): 計画を済みにするときに、計画を移す (計画は消す)。次の rollback は、この `current` を `evorag.prev` の版として使う。
- **読めないとき**:
  - ファイルが無い: 計画も記録も無い。
  - `plan` が無いことが読めて、`done` が読めない・`schema` を知らない: 記録が無いものとして扱う (後の版が書いたものを含む)。
  - それ以外 (JSON として読めない、`schema` を知らない版で `plan` がある、`plan` の項目が欠ける): 計画が読めないものとして、インストール先に触れずに断り、§7.3 の「手で戻す手順」を示す。
- 途中で落ちたら、次の `update apply`・`rollback` と次の TUI の起動の片付け (§7.3) が、計画に従って続きを進める。
- 入れ替えをそのまま繰り返さない。途中で落ちた後にやり直すと、前の版を失うため (S-07)。

### 7.3 OS ごとの置き換え

手順の番号は、状態のファイルの段 (`stage`) と同じ。手順 1 はどの表でも、名前のあるファイル (`evorag(.exe)`・`evorag.prev`) を動かさない準備で、計画はその後に書く。

**apply の置き換え**:

| OS | 手順 |
|---|---|
| Windows | 1. 新しいファイルを同じフォルダの `evorag.new` に書いて書き切る (§7.2 の apply の 8)。<br>2. 前回の `evorag.prev` があれば、`evorag.prev.old` に改名する (消さない)。<br>3. 動いている実行ファイル (apply を実行している自分自身) を `evorag.prev` に改名する。<br>4. 間を空けずに、`evorag.new` を `evorag.exe` に改名する。<br>5. 置き換えの後の確かめ (§7.2 の apply の 10)。<br>6. 済んだら `evorag.prev.old` を消す。<br>改名は上書きしない `MoveFileExW` (`MOVEFILE_WRITE_THROUGH`) で行う。<br>途中で失敗したら (5 の確かめに通らないときを含む)、計画を元に戻す途中 (`reverting`) にしてから、逆の順に改名して元に戻す: `evorag.exe` に置いた新しいファイルを `evorag.rejected` に、`evorag.prev` を `evorag.exe` に、`evorag.prev.old` を `evorag.prev` に戻し、`evorag.rejected` と `evorag.new` を消す |
| Linux | 1. 新しいファイルを `evorag.new` に書いて書き切る (§7.2 の apply の 8)。<br>2. 今の `evorag` の写しを `evorag.tmp` に書いて書き切る。<br>3. `rename` で `evorag.new` を `evorag` の上に置き、フォルダを同期する。<br>4. 置き換えの後の確かめ (§7.2 の apply の 10)。通らなければ、計画を `reverting` にしてから、`rename` で `evorag.tmp` を `evorag` の上に戻し (新しいファイルはここで消える)、フォルダを同期する。<br>5. `rename` で `evorag.tmp` を `evorag.prev` に置き (前回の `evorag.prev` を上書きする)、フォルダを同期する。<br>自分自身は改名しない (どの段で落ちても `evorag` がある)。動いている実行ファイルへ書き込むと失敗するので、上書きはしない |
| macOS | Linux と同じ (上書きすると署名の不一致で強制終了されることがあるため)。v0.6 の macOS 版のときに S-07 と同じ試験をする |

**rollback の入れ替え**:

| OS | 手順 |
|---|---|
| Windows | 1. `evorag.prev` の SHA-256 を求める (読むだけ。§7.2 の rollback の 2 と同じファイルであることを確かめる)。<br>2. 動いている実行ファイル (自分自身) を `evorag.tmp` に改名する。<br>3. 間を空けずに、`evorag.prev` を `evorag.exe` に改名する。<br>4. `evorag.tmp` を `evorag.prev` に改名する。<br>改名は apply と同じ。途中で失敗したら、`reverting` にしてから逆の順に改名して元に戻す |
| Linux | 1. 今の `evorag` の写しを `evorag.tmp` に書いて書き切る。<br>2. `rename` で `evorag.prev` を `evorag` の上に置く。<br>3. `rename` で `evorag.tmp` を `evorag.prev` に置き、フォルダを同期する。<br>どの段で落ちても `evorag` がある |
| macOS | Linux と同じ |

- **Windows の手順の理由** (S-07):
  - 動いている実行ファイルの上には、`MoveFileExW(MOVEFILE_REPLACE_EXISTING)` でも POSIX の意味の改名 (`FileRenameInfoEx`) でも置けない (アクセスの拒否)。
  - exFAT は POSIX の意味の改名を持たない。
  - 自分を改名してから置く手順だけが、NTFS と exFAT の両方で動く。
- **Windows の隙間**: apply の手順 3 と 4 の間、rollback の手順 2 と 3 の間、apply を元に戻すときの「`evorag.exe` を `evorag.rejected` に」と「`evorag.prev` を `evorag.exe` に」の間は、`evorag.exe` が無い。
  - その間に強制終了や電源断が当たると、起動できない。Windows では無くせない。
  - 2 つの改名の間には、ほかの処理 (書き込み・同期・表示) を入れない。
  - 戻し方を、同梱の README (§7.1) と、`evorag update apply`・`rollback` が始める前の表示に書く。
    - apply: 「`evorag.exe` が無いときは、`evorag.new` があればそれを、無ければ `evorag.prev` を `evorag.exe` に改名してください」。
    - rollback: 「`evorag.exe` が無いときは、`evorag.prev` を `evorag.exe` に改名してください」。
  - どちらの戻し方でも、次の `update` の操作か起動の片付けが、状態のファイルに従って残りを整える。
- **ファイルの作り方**: `evorag.new`・`evorag.tmp`・書き込めるかを試すファイル (`evorag.probe`) は、同じ名前のファイルがあれば先に消してから、排他の作成 (`create_new`。Unix では `O_NOFOLLOW` も付ける) で作る。Unix では、新しいファイルの権限を、今の `evorag` の権限から group と other の書き込みを除いたものにする。
- **インストール先の確かめ** (apply と rollback の前):
  - 書き込めないとき (Program Files など) は、何も変えずに理由を示して断る。先に `evorag.probe` を作って消せるかで確かめる。
  - `evorag(.exe)` がシンボリックリンク・リパースポイントなら断る。
  - Unix では、インストール先のフォルダか `evorag` が、動いている利用者のものでない、または group か other が書ける (sticky の無いフォルダ) なら断る。root で動いていて、インストール先のフォルダの持ち主が root でなければ断る (`sudo` で他人の置き場所を書き換えないため)。
- **ウイルス対策のロック**: ウイルス対策のソフトがロックしている (共有の違反・アクセスの拒否・ロックの違反) 間は、5 回まで再試行する。
  - 待ちは 200・400・600・800 ms (合わせて 2 秒)。
  - それでも失敗したら、元に戻して理由を示す。起動できる状態は保つ (S-07)。
- **更新のロック**: apply・rollback と、TUI の起動の片付けは、状態のフォルダの `update/lock` に OS のロック (03 §9 と同じ `File::try_lock`) を取ってから行い、終わるまで持つ。取れなければ、apply・rollback は断り、起動の片付けは行わずに進む。`update/` のフォルダとファイルは所有者だけの権限で作る (NFR-503)。
- **続きと片付け**: TUI の起動のときと、`update apply`・`rollback` を始める前 (§7.2 の始める前の確かめの 4・5) にだけ行う。`--version`・`update check`・ほかのサブコマンドは行わない。更新のロックを持って行う。
  - 計画が残っていて、計画の `dir` が動いている実行ファイルのフォルダ (正規化したパス) と同じなら、続きを進める。違えば触れずに、計画のフォルダを示す。
    - ファイルの名前ではなく、SHA-256 で役割 (`current`・`next`・`old_prev`、§7.2 の「状態のファイル」) を見分ける。調べるのは、計画の `dir` の `evorag(.exe)`・`evorag.new`・`evorag.tmp`・`evorag.prev`・`evorag.prev.old`・`evorag.rejected` だけ。
    - 計画が `reverting` なら、元に戻す向きに進める (`current` を `evorag(.exe)` に、`old_prev` を `evorag.prev` に戻し、`next` は消す)。
    - `evorag(.exe)` が `next` なら、残りの段を進めて終える (`current` を `evorag.prev` に置き、apply なら `old_prev` を消す)。apply で置き換えの後の確かめが済んでいなければ、確かめてから進める (続きを進めているのが `next` 自身なら、済んだものとする)。確かめに通らなければ `reverting` にして元に戻す。
    - `evorag(.exe)` が無い (Windows の隙間) なら、`next` を `evorag(.exe)` に置いてから、同じように終える。`next` が見つからなければ、`current` を置いて元に戻す。
    - `evorag(.exe)` が `current` なら、元に戻す (`old_prev` を `evorag.prev` に戻し、`evorag.tmp` のうち役割に当たらないものを消す)。操作はやり直さない (利用者がもう一度打つ)。
    - 役割に当たらないファイルには触れず、理由を示す (利用者が置いたものかもしれないため)。ただし、計画が置き換えの前 (段 1) で止まっているときに限り、役割に当たらない `evorag.new` は消してよい。
    - `evorag(.exe)` がどの役割にも当たらないときは、何も動かさず、計画を残し、理由と「手で戻す手順」を示す (apply・rollback は 1 で終わる)。
    - 終えたら計画を済みにし (元に戻したときは計画を消し)、何をしたかを示す。
  - 計画が無ければ、名前で片付ける。
    - `evorag.prev` が無く `evorag.prev.old` か `evorag.tmp` があれば、それを `evorag.prev` に戻す。
    - 余った `evorag.new`・`evorag.tmp`・`evorag.prev.old`・`evorag.rejected`・`evorag.probe` は消す。消せなければ (ロック)、起動の片付けでは次の機会に回し、apply・rollback は断る (§7.2)。
- **手で戻す手順** (状態のファイルが読めない、役割に当たらないとき。README にも書く):
  1. evorag を終了する。
  2. インストール先の `evorag(.exe)`・`evorag.new`・`evorag.tmp`・`evorag.prev`・`evorag.prev.old`・`evorag.rejected` を、それぞれ `--version` で動かして版を確かめる。
  3. 使いたい版のファイルを `evorag(.exe)` に、前の版のファイルを `evorag.prev` に改名し、残りを消す。
  4. 状態のフォルダの `update/state.json` を消す (記録が無くなるので、次の rollback は一般の案内になる)。
- 技術検証は S-07 (08 §3、`spikes/S-07/REPORT.md`): Windows (NTFS・exFAT) と Linux (ext4) で、300 回の置き換えと巻き戻しのうち、起動できない状態は 0 回。

### 7.4 パッケージ管理で入れた場合

- 配布の経路 (zip、cargo、将来の winget・Homebrew など) は、ビルドのときの環境変数 `EVORAG_CHANNEL` で実行ファイルに埋め込む (パスから推測しない)。`cargo xtask dist` は `zip` を付け、付けないビルド (`cargo install`・開発のビルド) は `cargo` になる。`evorag --version` に `installed via: <経路>` として表示する (07 §5)。
- `EVORAG_CHANNEL=zip` のビルドには、`EVORAG_RELEASE_DATE` (§5.3) が要る。無ければビルドを失敗させる (`build.rs`、`rerun-if-env-changed` を付ける)。`release.yml` は、配布物の `--version` の行と、ビルドに渡した日付が `release.json` の `date` と同じであることを確かめる (09 §9.2 の手順 6)。
- 経路が `zip` でない evorag では、`apply` と `rollback` を通信の前に断り、その管理の仕組みでの更新を案内する。`check` は使える。
- cargo で入れた利用者向けに、README に「`update check` で新しい版を確かめ、`cargo install --locked --git https://github.com/evoref/evorag --tag vX.Y.Z evorag` で入れ直す」と書く。

### 7.5 通信

- evorag が外へ通信するのは、`update check` と `update apply` を利用者が打ったときだけ (確定 (ユーザー回答、2026-11-08)、[ADR-0011](10-decisions.md#adr-0011-外へ通信するのは-llama-server-と手動の更新だけ))。起動時や定期の自動の確認はしない。`update rollback` は通信しない。
- 通信に含めるのは、evorag の版と OS の種類と、取得する配布物の名前 (URL に入る。対象の target triple を含む) だけ。
  - User-Agent は `evorag/<版> (<OS>)` に固定する (OS は `windows`・`linux`・`macos`)。
  - Cookie を持たず、認証のトークンを送らない。`Accept-Encoding` を送らない (ureq の圧縮の機能を入れない)。
- 通信には HTTPS が要る。TLS は rustls と純 Rust の暗号の実装 (rustls-graviola) で組む (確定 (ユーザー回答、条件: S-06)、[OQ-05](10-decisions.md#oq-05-tls-と-c-を含む依存))。graviola は `=` で版を固定する (下の CPU の機能の一覧が版に結び付くため)。
- **接続先** (固定。設定では変えない):
  - リリースの一覧: `https://api.github.com/repos/evoref/evorag/releases?per_page=20`。1 回の操作で 1 回だけ引く (`check` と、版を省いた `apply`)。選び方は §7.2 の apply の 1 (semver でいちばん大きい版)。
  - `release.json`・`release.json.minisig`・配布物: `https://github.com/evoref/evorag/releases/download/v<版>/<名前>` (配布物の名前は `release.json` の `artifacts[].name`)。
- **転送** (redirect): 自動では辿らない。転送先を 1 つずつ確かめ、`https` で、利用者の情報 (userinfo) を含まず、ポートが 443 (省略を含む) で、ホストが `github.com`・`api.github.com`・`*.githubusercontent.com` のどれかのときだけ辿る。5 回まで。
- **大きさの上限**: リリースの一覧は 1 MiB、`release.json` は 64 KiB、`.minisig` は 4 KiB、配布物は `size` と 64 MiB の小さいほう。実際に読んだバイト数で数え、`Content-Length` には頼らない。超えたら読むのを止めて中止する。
- **時間の上限**: 接続は 10 秒。リリースの一覧・`release.json`・`.minisig` は 1 つにつき合わせて 30 秒、配布物は合わせて 10 分。超えたら中止する。
- **プロキシ**: 環境変数 `HTTPS_PROXY`・`ALL_PROXY`・`NO_PROXY` (小文字の `https_proxy`・`all_proxy`・`no_proxy` も) に従う (ureq の扱い。M3.5 の実装で確かめる)。Windows の WinINET の設定と PAC は読まない。プロキシを経ても、TLS は evorag と GitHub の間で結ぶ。認証の要るプロキシ (NTLM など) には対応しない (README に書く)。
- **通信できないとき**: 理由を示し、プロキシを使う環境なら `HTTPS_PROXY` を設定すること、通信できない環境では手作業の更新 (§7.1) と Releases の URL を案内する。
- **回数の制限**: GitHub の API の匿名の呼び出しの回数の制限 (1 時間に 60 回) に当たったとき (403 か 429 で、残りの回数が 0) と、二次の制限 (403 で `Retry-After` がある) に当たったときは、制限が解ける時刻 (分かれば) を示して終わる。
- **信頼の拠り所は署名**: 配布物が本物かどうかは、TLS ではなく minisign の署名つきのマニフェスト (§5.2) で確かめる。TLS が破られても、偽の配布物は署名の検証で拒否される。
  - 凍結に備えて、`check` と `apply` は `release.json` の `date` が今の版のリリースの日付 (`EVORAG_RELEASE_DATE`、§5.3) より古ければ拒否する (§7.2 の apply の 4)。
- **残るリスク (署名で防げないこと)**: 署名は「evorag のリリースであること」を保証するが、「最新であること」は保証しない。
  - 通信の途中に入った者は、署名の正しい、今より新しいが最新ではない版へ誘導したり、新しい版を見せない (凍結) ことができる。これを防ぐのは TLS だけである。
  - 日付の確かめが防ぐのは、鍵の誤用や版の付け違いで今より古い日付のリリースが署名されたときに、それを入れることである。
  - 取り下げた版 (09 §9.3) は、prerelease にして `release.json.minisig` を消すので、`apply` は署名が無く断る。
- **CPU の機能が足りないとき**: graviola は x86_64 / aarch64 で特定の CPU の機能を必須とし、無い CPU での代替の実装を持たない。足りないと、最初の暗号の処理で panic する。`evorag update` は通信の前に CPU の機能を確かめ (`is_x86_feature_detected!` などの実行時の判定)、足りなければ通信せずに理由を示し、手作業の更新の手順 (§7.1) を案内する。
  - 確かめる機能 (graviola 0.4.5 の `is_cpu_supported` と同じ。S-06): x86_64 は aes・pclmulqdq・bmi1・adx・avx・avx2、aarch64 は neon・aes・pmull・sha2。graviola の版を上げるときは、この一覧を確かめ直す。
- **証明書のルートが無いとき**: OS の検証器 (§2) は、Linux では OS の証明書ストア (`ca-certificates` など) を読む。見つからなければ通信せずに理由を示し、手作業の更新の手順 (§7.1) を案内する。

### 7.6 更新の対象の整理

| 対象 | やり方 |
|---|---|
| evorag 本体 | §7.1 / §7.2 |
| データ (`evorag.redb`) | 新しい版の初回の起動で、退避してから移行する (03 §12) |
| 索引 | 形式の版が変わったら、古い索引を捨てて作り直す (その間は残りで答える) |
| 設定 (`config.toml`) | 版を持たせ、移行の関数で書き換える。書き換える前に元のファイルを退避する (03 §12.3) |
| カートリッジ | 新しい版を導入して切り替える (14、v0.6) |
| llama-server | evorag は更新しない。対応する版の範囲を外れたら知らせるだけ (06 §10) |

## 8. 名称

- 製品名・バイナリ名は `evorag`。crate の接頭辞は `evorag-`、環境変数の接頭辞は `EVORAG_`。
- 名称とロゴの使用の方針は v1.0 の前に決める。
