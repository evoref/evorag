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

- 各リリースに、`SHA256SUMS` (配布物ごとの SHA-256) と **署名つきのマニフェスト** `release.json` (§5.3) を付け、どちらも minisign で署名する (`.minisig`)。
- `release.json` の署名の trusted comment は、`evorag <版> release.json sha256 <SHA-256>` とちょうど同じ文字列にする。`<版>` は `release.json` の `version`、`<SHA-256>` は `release.json` のバイト列の SHA-256 (小文字の 16 進 64 桁)。`cargo xtask dist` がこの文字列を示す。`evorag update` は、前後の空白や余分な語を含めて完全に一致することを確かめる (§7.2)。
- **鍵**: リリースの署名鍵は、カートリッジの署名鍵 (14) とは分ける。
  - 公開鍵はリポジトリ直下に置く。今の鍵は `minisign.pub`、次の鍵は `minisign.next.pub`。
  - 本体には **今の鍵と次の鍵の 2 本** を埋め込む。どちらかの鍵で署名が通れば合格とする。実装は鍵を何本でも持てるようにする。
  - 次の鍵は、v0.3.0 のリリースの前に、人間が手元で (オフラインで) 作り、公開鍵を `minisign.next.pub` に置く。エージェントは鍵を作らない。次の鍵が置かれるまで v0.3.0 をリリースしない (埋め込む鍵が 1 本だと、今の鍵を失ったときに `update` で更新できる版が無くなるため)。
  - 鍵の交代: 次の鍵で署名した版を出す。その版では、次の鍵を今の鍵 (`minisign.pub`) にし、さらに次の鍵を作って `minisign.next.pub` に置き、2 本を埋め込む。今の鍵が漏れたときも、次の鍵で署名した版で入れ替えられる。
  - 利用者は、設定 `update.trusted_keys` (07 §4.2) で鍵を足せる。埋め込んだ鍵は外せない。
- 署名は人間が手元で (オフラインで) 行う。秘密鍵を CI の secrets に置かない (11 §9.7)。
- コード署名 (Authenticode・Apple の公証) は v1.0 までに検討する。それまでは、初回の起動で SmartScreen・Gatekeeper の警告が出ることを README に書く。

### 5.3 `release.json` の形式

`release.json` は UTF-8 の JSON の object で、`cargo xtask dist` (09 §9.2 の手順 5) が作る。形式の版 (`schema`) は `1`。

| 項目 | 型 | 内容 |
|---|---|---|
| `schema` | 整数 | 形式の版 (`1`) |
| `name` | 文字列 | `"evorag"` |
| `version` | 文字列 | 版 (semver、先頭の `v` なし)。タグは `v<version>` |
| `date` | 文字列 | 公開の日付 (`YYYY-MM-DD`、UTC)。配布物の実行ファイルに埋め込む `EVORAG_RELEASE_DATE` と同じ値 (タグの指すコミットの日付。対象ごとに別の機械で作っても同じ値になる) |
| `upgrade_from` | 文字列 | `evorag update apply` でこの版へ更新できる、今の版の下限 (semver)。これより古い版からは手作業で更新する (§7.1) |
| `notes` | 文字列 | 変更の要点。CHANGELOG のその版の節から作る平文で、UTF-8 で 4 KiB 以内。無くてもよい |
| `artifacts` | 配列 | 対象ごとの配布物 (下の表) |
| `db_format` | 文字列 | この版が書く DB の形式の版 (03 §3.1.1 の `format.version` と同じ書き方) |
| `llama_server_min_build` | 整数か null | 対応する llama-server の build 番号の下限 (06 §10)。決めていなければ null |

`artifacts` の各要素:

| 項目 | 型 | 内容 |
|---|---|---|
| `target` | 文字列 | 対象 (Rust の target triple、§5.1) |
| `name` | 文字列 | 配布物のファイル名 (Releases の asset の名前) |
| `sha256` | 文字列 | 配布物の SHA-256 (小文字の 16 進 64 桁) |
| `size` | 整数 | 配布物の大きさ (バイト) |
| `exe` | 文字列 | 配布物の中の実行ファイルのパス (`/` で区切る。例: `evorag-0.3.0-x86_64-pc-windows-msvc/evorag.exe`)。`..`・絶対パス・`\` を含まない |
| `exe_sha256` | 文字列 | 実行ファイルの SHA-256 (小文字の 16 進 64 桁) |

- `cargo xtask dist` は、すべての項目を書く。
- 読む側 (`evorag update`、§7.2) は、知らない項目を無視する (後の版で項目を足せるように)。
- `schema` が無い、または `1` でないときは、更新を断り、手作業の更新の手順 (§7.1) を案内する。形式を変える (項目の意味を変える・必須の項目を足す) ときは `schema` を上げる。
- 必須の項目は、`schema`・`name`・`version`・`date`・`upgrade_from`・`artifacts` と、`artifacts` の各要素の 6 つ。欠けている・型が違う・同じ `target` の要素が 2 つある・`exe` のパスが上の条件に合わないときは断る。`notes`・`db_format`・`llama_server_min_build` は無くても断らない。
- v0.3.0 より前の版の `release.json` は `schema`・`upgrade_from`・`exe`・`exe_sha256` を持たない。それらの版は更新の先にならない (今と同じか古い版は拒否する、§7.2) ので、読めなくてよい。

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

### 7.1 v0.1〜v0.2: 手作業の更新

本体の更新機能は v0.3 で入れる (確定 (ユーザー回答)、[OQ-04](10-decisions.md#oq-04-更新機能の時期))。それまでは、同梱の README に次の手順を書く。

1. Releases から新しい配布物と `SHA256SUMS`・`.minisig` を取得する。
2. minisign で署名を確かめる (コマンドの例を README に書く)。
3. evorag を終了し、実行ファイルを置き換える。古い実行ファイルは `evorag.prev` として残す。`evorag.exe` が無くなったとき (置き換えの途中で止まったなど) は、`evorag.new` か `evorag.prev` を `evorag.exe` に改名して戻す (§7.3)。
4. 新しい版の初回の起動で、移行が要るときは自動で退避してから移行する (03 §12)。
5. 巻き戻すときは、`evorag.prev` に戻す。新しい版で移行した後なら、`evorag restore` で移行の前の退避 (`backups/pre-op/`) から戻す (戻さなければ、古い版は読むだけで起動する、03 §4 の規則 6)。

- データの移行と退避の仕組みは v0.1 で入れるので、手作業の更新でもデータは失わない。

### 7.2 v0.3 から: `evorag update`

| コマンド | 内容 |
|---|---|
| `evorag update check` | リリースの一覧を取得し、今の版より新しい版があれば、いちばん新しい版の `release.json` を検証して、版・公開の日付・変更の要点を示す。何も書き換えない |
| `evorag update apply [版]` | 配布物を取得し、検証してから置き換える。版を省くと、いちばん新しい版にする |
| `evorag update rollback` | `evorag.prev` に戻す |

**始める前の確かめ** (apply と rollback。どれかに当たれば、何も変えずに理由を示して断る。終了コードは 07 §5):
1. 動いている実行ファイルの名前が `evorag.exe` (Windows) か `evorag` (Linux・macOS) である (§7.3 の手順がこの名前を前提にするため)。
2. (apply だけ) 入れ方が zip である (§7.4)。
3. 状態のフォルダの `update/lock` を取れる (§7.3 の「更新のロック」)。取れなければ、ほかの `update` が動いている。
4. データ根の `lock` (03 §9) を取れる。取れなければ evorag が動いているので、閉じてから実行するよう案内する。取ったロックは操作の終わりまで持つ (置き換えの途中で evorag が起動しないように)。データ根がまだ無ければ取らない。
5. 状態のファイルに続きがあれば、先に続きを済ませる (§7.3 の「続きと片付け」)。
6. インストール先に書き込める (§7.3 の「書き込めないとき」)。
7. CPU の機能と証明書のルートがある (§7.5)。ここまでは通信しない。rollback は通信しないので確かめない。

- `check` は、7 だけを確かめてから通信する。データ根とインストール先には触れない。いちばん新しい版の `release.json` の検証は、apply の 2〜4 と同じ (版を比べる・対象を探すところまで)。通らなければ、その理由を示す。

**apply の流れ**:
1. 版を決める。版を指定しなければ、リリースの一覧 (§7.5) から、draft と prerelease を除いたいちばん新しい版を選ぶ。版を指定したときは一覧を引かない。
2. `release.json` と `release.json.minisig` を、§7.5 の固定の URL から取得する。取得したものはディスクに書かず、主記憶で扱う。
3. **署名を確かめる**: 本体に埋め込んだ鍵 (§5.2) と設定 `update.trusted_keys` のどれかで、`release.json` のバイト列の署名が通ること。trusted comment が `evorag <版> release.json sha256 <SHA-256>` と完全に一致し、`<SHA-256>` が取得したバイト列の SHA-256 であること (§5.2)。
4. **中身を確かめる** (署名が通った後に初めて JSON として読む。形式は §5.3):
   - `schema` が `1`。違えば、手作業の更新の手順 (§7.1) を案内する。
   - `name` が `evorag`。`version` が、trusted comment の版・取得した URL のタグの版・指定した版 (指定したとき) のすべてと同じ。
   - 版が今より新しい。**今と同じか古い版は拒否する** (巻き戻しは `rollback` だけ)。
   - `date` が、今の版の公開の日付 (実行ファイルに埋め込んだ `EVORAG_RELEASE_DATE`、§5.3) より古くない (凍結への備え、§7.5)。開発のビルド (日付を埋め込んでいない) では確かめない。
   - 今の版が `upgrade_from` 以上。下回れば、手作業の更新の手順 (§7.1) を案内する。
   - 今の OS と CPU の対象 (target triple) の要素が `artifacts` にある。
5. 変更の要点を示す。`notes` から制御文字 (改行を除く) を除いて示す。`notes` が無ければ、リリースのページの URL だけを示す。検証の前の内容は示さない。
6. 配布物を取得する。大きさの上限は、`size` と 64 MiB の小さいほう (§7.5)。大きさと SHA-256 が `size`・`sha256` と合わなければ、捨てて中止する。
7. 配布物を展開の処理に渡すのは、6 を通った後だけ (検証の済んでいないバイト列を圧縮の形式として読まない)。中から `exe` の 1 項目だけを取り出し、ほかは展開しない。取り出した実行ファイルの SHA-256 が `exe_sha256` と合うことを確かめる。
8. インストール先の `evorag.new` に書いて書き切る (Linux・macOS では実行の権限を付ける)。閉じてから開き直し、SHA-256 が `exe_sha256` と合うことを確かめ直す (検証と置き換えの間の差し替えと、書き込みの誤りを防ぐ)。
9. 状態のファイルに計画を書いてから、§7.3 の手順で置き換える。今の実行ファイルは `evorag.prev` として残す。
10. **置き換えの後の確かめ**: 新しいファイルを `evorag(.exe)` に置いた直後 (前の `evorag.prev` を消す・上書きする前、§7.3) に、新しい `evorag(.exe)` を `--version` で動かし、出力の版が `version` と合うことを確かめる。10 秒で打ち切る。`--version` は通信せず、データ根も開かない。合わない・動かない・時間切れのときは、§7.3 の失敗のときと同じく元に戻し、理由を示す。
11. 状態のファイルの計画を済みにする。
12. 新しい版の初回の起動で、移行が要るときは退避してから移行する (03 §12.1)。移行に失敗したら、新しい版は元の DB のまま読むだけで起動し (03 §12.1)、理由とともに `evorag update rollback` を案内する。

- 3〜8 のどこで断っても、インストール先のファイルは変わらない (`evorag.new` を書いた後なら消す)。

**rollback の流れ**:
1. `evorag.prev` が無ければ断る。
2. 戻す先を確かめる。状態のファイルの最後に済んだ操作の記録があり、その `current` (§7.2 の「状態のファイル」) の SHA-256 が `evorag.prev` と合えば、その版を示す。記録が無い・合わない (手作業で置いた、§7.1) ときは、`evorag.prev --version` (10 秒で打ち切る) で版を確かめて示す。evorag の版を出さなければ断る。
3. 状態のファイルに計画を書いてから、§7.3 の手順で入れ替える。今の実行ファイルは `evorag.prev` になる。
4. 状態のファイルの計画を済みにする。
5. **データの案内**: rollback はデータを自動では戻さない。データ根があれば、DB を読むだけで開いて `format.min_write` (03 §3.1.1) を読み、戻した版が書く DB の形式の版 (記録の `db_format`) と比べる。
   - 戻した版の形式の版が `min_write` 以上 (戻した版が書ける): そのまま使えることを示す。
   - 下回る (戻した版は読むだけで起動する、03 §4 の規則 6): 新しい版が移行の前に作った退避 (`backups/pre-op/`、03 §7.2) のパスと、戻した版で `evorag restore <退避>` を実行すれば書けるようになることを示す。
   - 記録が無く、戻した版の形式の版が分からないとき: 一般の案内を示す (新しい版で移行した後なら、`backups/pre-op/` の移行の前の退避から `evorag restore` で戻せる。戻さなければ、古い版は読むだけで起動する)。

**状態のファイル** (状態のフォルダの `update/state.json`):
- JSON で、所有者だけの権限で作る (NFR-503)。一時名に書いて書き切り、改名で置く。
- **計画** (`plan`): apply と rollback が、入れ替えを始める前に書き、段を進めるたびに書き直す。項目は、操作 (`op`: `apply` か `rollback`)、インストール先のフォルダ (`dir`)、進んだ段 (`stage`: §7.3 の手順の番号)、役割ごとの版・SHA-256・DB の形式の版 (分かれば)。役割は次の 3 つ。
  - `current`: 入れ替えの前の `evorag(.exe)`。apply では動いている自分で、DB の形式の版は自分が書く版。
  - `next`: `evorag(.exe)` に置くもの。apply では `release.json` の版と `exe_sha256` と `db_format`、rollback では入れ替えの前の `evorag.prev`。
  - `old_prev`: 入れ替えの前の `evorag.prev` (apply で、あれば)。
- **最後に済んだ操作** (`done`): 計画を済みにするときに、計画を移す (計画は消す)。次の rollback は、この `current` を `evorag.prev` の版として使う。
- 途中で落ちたら、次の `update` の操作と次の起動の片付け (§7.3) が、計画に従って続きを進める。
- 入れ替えをそのまま繰り返さない。途中で落ちた後にやり直すと、前の版を失うため (S-07)。

### 7.3 OS ごとの置き換え

**apply の置き換え** (手順の番号は、状態のファイルの段):

| OS | 手順 |
|---|---|
| Windows | 1. 新しいファイルを同じフォルダの `evorag.new` に書いて書き切る (§7.2 の apply の 8)。<br>2. 前回の `evorag.prev` があれば、`evorag.prev.old` に改名する (消さない)。<br>3. 動いている実行ファイル (apply を実行している自分自身) を `evorag.prev` に改名する。<br>4. 間を空けずに、`evorag.new` を `evorag.exe` に改名する。<br>5. 置き換えの後の確かめ (§7.2 の apply の 10)。<br>6. 済んだら `evorag.prev.old` を消す。<br>改名は上書きしない `MoveFileExW` (`MOVEFILE_WRITE_THROUGH`) で行う。途中で失敗したら (5 の確かめに通らないときを含む)、逆の順に改名して元に戻す (`evorag.exe` に置いたものを `evorag.new` に、`evorag.prev` を `evorag.exe` に、`evorag.prev.old` を `evorag.prev` に戻し、`evorag.new` を消す) |
| Linux | 1. 新しいファイルを `evorag.new` に書いて書き切る (§7.2 の apply の 8)。<br>2. 今の `evorag` の写しを `evorag.tmp` に書いて書き切る。<br>3. `rename` で `evorag.new` を `evorag` の上に置き、フォルダを同期する。<br>4. 置き換えの後の確かめ (§7.2 の apply の 10)。通らなければ、`rename` で `evorag.tmp` を `evorag` の上に戻し、フォルダを同期する。<br>5. `rename` で `evorag.tmp` を `evorag.prev` に置き (前回の `evorag.prev` を上書きする)、フォルダを同期する。<br>自分自身は改名しない (どの段で落ちても `evorag` がある)。動いている実行ファイルへ書き込むと失敗するので、上書きはしない |
| macOS | Linux と同じ (上書きすると署名の不一致で強制終了されることがあるため)。v0.6 の macOS 版のときに S-07 と同じ試験をする |

**rollback の入れ替え**:

| OS | 手順 |
|---|---|
| Windows | 1. 状態のファイルに計画を書く。<br>2. 動いている実行ファイル (自分自身) を `evorag.tmp` に改名する。<br>3. 間を空けずに、`evorag.prev` を `evorag.exe` に改名する。<br>4. `evorag.tmp` を `evorag.prev` に改名する。<br>改名は apply と同じ。途中で失敗したら、逆の順に改名して元に戻す |
| Linux | 1. 状態のファイルに計画を書く。<br>2. 今の `evorag` の写しを `evorag.tmp` に書いて書き切る。<br>3. `rename` で `evorag.prev` を `evorag` の上に置く。<br>4. `rename` で `evorag.tmp` を `evorag.prev` に置き、フォルダを同期する。<br>どの段で落ちても `evorag` がある |
| macOS | Linux と同じ |

- **Windows の手順の理由** (S-07):
  - 動いている実行ファイルの上には、`MoveFileExW(MOVEFILE_REPLACE_EXISTING)` でも POSIX の意味の改名 (`FileRenameInfoEx`) でも置けない (アクセスの拒否)。
  - exFAT は POSIX の意味の改名を持たない。
  - 自分を改名してから置く手順だけが、NTFS と exFAT の両方で動く。
- **Windows の隙間**: apply の手順 3 と 4 の間と、rollback の手順 2 と 3 の間 (と、元に戻すときの同じ 2 つの改名の間) は `evorag.exe` が無い。
  - その間に強制終了や電源断が当たると、起動できない。Windows では無くせない。
  - 2 つの改名の間には、ほかの処理 (書き込み・同期・表示) を入れない。
  - 戻し方を、同梱の README (§7.1) と `evorag update apply`・`rollback` の表示に書く: 「`evorag.exe` が無いときは、`evorag.new` (無ければ `evorag.prev`) を `evorag.exe` に改名する」。
- **ウイルス対策のロック**: ウイルス対策のソフトがロックしている (共有の違反・アクセスの拒否・ロックの違反) 間は、5 回まで再試行する。
  - 待ちは 200・400・600・800 ms (合わせて 2 秒)。
  - それでも失敗したら、元に戻して理由を示す。起動できる状態は保つ (S-07)。
- **更新のロック**: apply・rollback と、次の起動の片付けは、状態のフォルダの `update/lock` に OS のロック (03 §9 と同じ `File::try_lock`) を取ってから行い、終わるまで持つ。取れなければ、apply・rollback は断り、起動の片付けは行わずに進む。`update/` のフォルダとファイルは所有者だけの権限で作る (NFR-503)。
- **続きと片付け** (次の起動と、次の `update` の操作の前。更新のロックを持って行う):
  - 状態のファイルに計画が残っていれば、続きを進める。
    - ファイルの名前ではなく、SHA-256 で役割 (`current`・`next`・`old_prev`、§7.2 の「状態のファイル」) を見分ける。調べるのは、計画の `dir` の `evorag(.exe)`・`evorag.new`・`evorag.tmp`・`evorag.prev`・`evorag.prev.old` だけ。
    - `evorag(.exe)` が `next` なら、残りの段を進めて終える (`current` を `evorag.prev` に置き、apply なら `old_prev` を消す)。apply で置き換えの後の確かめが済んでいなければ、確かめてから進める (続きを進めているのが `next` 自身なら、済んだものとする)。
    - `evorag(.exe)` が無い (Windows の隙間) なら、`next` を `evorag(.exe)` に置いてから、同じように終える。`next` が見つからなければ、`current` を置いて元に戻す。
    - `evorag(.exe)` が `current` なら、元に戻す (`old_prev` を `evorag.prev` に戻し、`evorag.new` と `evorag.tmp` のうち `current`・`old_prev` でないものを消す)。操作はやり直さない (利用者がもう一度打つ)。
    - どの役割にも当たらないファイルには触れず、理由を示す (利用者が置いたものかもしれないため)。
    - 終えたら計画を済みにし (元に戻したときは計画を消し)、何をしたかを示す。
  - 計画が無ければ、名前で片付ける。
    - `evorag.prev` が無く `evorag.prev.old` か `evorag.tmp` があれば、それを `evorag.prev` に戻す。
    - 余った `evorag.new`・`evorag.tmp`・`evorag.prev.old` は消す。消せなければ (ロック)、次の機会に回す。
- **書き込めないとき**: インストール先に書き込めない (Program Files など) ときは、何も変えずに理由を示して断る。
  - 先に、一時のファイルを作って消せるかで確かめる。
- 技術検証は S-07 (08 §3、`spikes/S-07/REPORT.md`): Windows (NTFS・exFAT) と Linux (ext4) で、300 回の置き換えと巻き戻しのうち、起動できない状態は 0 回。

### 7.4 パッケージ管理で入れた場合

- 配布の経路 (zip、cargo、将来の winget・Homebrew など) は、ビルドのときの環境変数 `EVORAG_CHANNEL` で実行ファイルに埋め込む (パスから推測しない)。`cargo xtask dist` は `zip` を付け、付けないビルド (`cargo install`・開発のビルド) は `cargo` になる。`evorag --version` に表示する。
- 経路が `zip` でない evorag では、`apply` を通信の前に断り、その管理の仕組みでの更新を案内する。

### 7.5 通信

- evorag が外へ通信するのは、`update check` と `update apply` を利用者が打ったときだけ (確定 (ユーザー回答、2026-11-08)、[ADR-0011](10-decisions.md#adr-0011-外へ通信するのは-llama-server-と手動の更新だけ))。起動時や定期の自動の確認はしない。`update rollback` は通信しない。
- 確認の通信には、evorag の版と OS の種類以外を含めない。
  - User-Agent は `evorag/<版> (<OS>)` に固定する (OS は `windows`・`linux`・`macos`)。
  - Cookie を持たず、認証のトークンを送らない。
- 通信には HTTPS が要る。TLS は rustls と純 Rust の暗号の実装 (rustls-graviola) で組む (確定 (ユーザー回答、条件: S-06)、[OQ-05](10-decisions.md#oq-05-tls-と-c-を含む依存))。graviola は `=` で版を固定する (下の CPU の機能の一覧が版に結び付くため)。
- **接続先** (固定。設定では変えない):
  - リリースの一覧: `https://api.github.com/repos/evoref/evorag/releases` (1 ページ目だけ。新しい順に並ぶ)。1 回の操作で 1 回だけ引く (`check` と、版を省いた `apply`)。
  - `release.json`・`release.json.minisig`・配布物: `https://github.com/evoref/evorag/releases/download/v<版>/<名前>` (配布物の名前は `release.json` の `artifacts[].name`)。
- **転送** (redirect): 自動では辿らない。転送先を 1 つずつ確かめ、`https` で、ホストが `github.com`・`api.github.com`・`*.githubusercontent.com` のどれかのときだけ辿る。5 回まで。
- **大きさの上限**: リリースの一覧は 1 MiB、`release.json` は 64 KiB、`.minisig` は 4 KiB、配布物は `size` と 64 MiB の小さいほう。超えたら読むのを止めて中止する。
- **時間の上限**: 接続は 10 秒。リリースの一覧・`release.json`・`.minisig` は 1 つにつき合わせて 30 秒、配布物は合わせて 10 分。超えたら中止する。
- **プロキシ**: 環境変数 (`HTTPS_PROXY`・`ALL_PROXY`・`NO_PROXY`) に従う (ureq の扱い。M3.5 の実装で確かめ、同梱の README に書く)。プロキシを経ても、TLS は evorag と GitHub の間で結ぶ。
- **信頼の拠り所は署名**: 配布物が本物かどうかは、TLS ではなく minisign の署名つきのマニフェスト (§5.2) で確かめる。TLS が破られても、偽の配布物は署名の検証で拒否される。TLS が守るのは、通信の内容 (版と OS の種類) と、古いリリースの一覧を見せられる「凍結」への耐性である。
  - 凍結に備えて、`update check` は最新のリリースの公開の日付を表示し、`check` と `apply` は `release.json` の `date` が今の版の公開の日付 (`EVORAG_RELEASE_DATE`、§5.3) より古ければ拒否する (§7.2 の apply の 4)。
- **CPU の機能が足りないとき**: graviola は x86_64 / aarch64 で特定の CPU の機能を必須とし、無い CPU での代替の実装を持たない。足りないと、最初の暗号の処理で panic する。`evorag update` は通信の前に CPU の機能を確かめ (`is_x86_feature_detected!` などの実行時の判定)、足りなければ通信せずに理由を示し、手作業の更新の手順 (§7.1) を案内する。
  - 確かめる機能 (graviola 0.4.5 の `is_cpu_supported` と同じ。S-06): x86_64 は aes・pclmulqdq・bmi1・adx・avx・avx2、aarch64 は neon・aes・pmull・sha2。graviola の版を上げるときは、この一覧を確かめ直す。
- **証明書のルートが無いとき**: OS の検証器 (§2) は、Linux では OS の証明書ストア (`ca-certificates` など) を読む。見つからなければ通信せずに理由を示し、手作業の更新の手順 (§7.1) を案内する。
- GitHub の API の匿名の呼び出しの回数の制限 (1 時間に 60 回) に当たったら (403 か 429 で、残りの回数が 0)、制限が解ける時刻を示して終わる。

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
