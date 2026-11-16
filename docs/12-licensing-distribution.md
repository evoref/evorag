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

- 各リリースに、`SHA256SUMS` (配布物ごとの SHA-256) と **署名つきのマニフェスト** `release.json` を付け、どちらも minisign で署名する (`.minisig`)。
- `release.json` の中身: 版、公開の日付、対象ごとの配布物の名前・SHA-256・大きさ、最低限の移行の情報 (DB の形式の版)、対応する llama-server の build 番号の下限。
- minisign の trusted comment にも、版と `release.json` の SHA-256 を入れる。
- **鍵**: リリースの署名鍵は、カートリッジの署名鍵 (14) とは分ける。公開鍵はリポジトリ直下の `minisign.pub` に置き、本体には **今の鍵と次の鍵の 2 本** を埋め込む (鍵の交代は、次の鍵で署名した版を出し、その版でさらに次の鍵を埋め込む)。
- 署名は人間が手元で (オフラインで) 行う。秘密鍵を CI の secrets に置かない (11 §9.7)。
- コード署名 (Authenticode・Apple の公証) は v1.0 までに検討する。それまでは、初回の起動で SmartScreen・Gatekeeper の警告が出ることを README に書く。

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
| `evorag update check` | 公開リポジトリのリリースの一覧を取得し、今の版より新しいものと変更の要点を示す |
| `evorag update apply [版]` | 配布物を取得し、検証してから置き換える |
| `evorag update rollback` | `evorag.prev` に戻す |

**apply の流れ**:
1. 状態のフォルダの `update/` に、`release.json`・`.minisig`・配布物を取得する。
2. `release.json` の署名を本体に埋め込んだ鍵 (と設定 `update.trusted_keys`) で確かめ、版が今より新しいこと、対象が今の OS と CPU であることを確かめる。**今より古い版は拒否する** (巻き戻しは `rollback` だけ)。
3. 配布物の SHA-256 を `release.json` と照らす。合わなければ捨てて中止する。
4. 置き換えの直前に、開いた同じファイルのハンドルから SHA-256 を確かめ直す (検証と適用の間の差し替えを防ぐ)。
5. §7.3 の手順で置き換える。今の実行ファイルは `evorag.prev` として残す。
6. 新しい版の初回の起動で、移行が要るときは退避してから移行する。

**rollback**: `evorag.prev` に戻す。新しい版で移行済みなら、移行の前の退避から戻すかを選ばせる (戻さなければ読むだけで起動する)。
- 始める前に、どちらの版を `evorag(.exe)` に、どちらを `evorag.prev` にするかを、状態のフォルダの `update/` の状態のファイルに書く。
- 途中で落ちたら、次の `update` の操作 (と次の起動の片付け、§7.3) が、その記録に従って続きから進める。
- 入れ替えをそのまま繰り返さない。途中で落ちた後にやり直すと、前の版を失うため (S-07)。

### 7.3 OS ごとの置き換え

| OS | 手順 |
|---|---|
| Windows | 1. 新しいファイルを同じフォルダの `evorag.new` に書いて書き切る。<br>2. 前回の `evorag.prev` があれば、`evorag.prev.old` に改名する (消さない)。<br>3. 動いている実行ファイル (apply を実行している自分自身) を `evorag.prev` に改名する。<br>4. 間を空けずに、`evorag.new` を `evorag.exe` に改名する。<br>5. 済んだら `evorag.prev.old` を消す。<br>改名は上書きしない `MoveFileExW` (`MOVEFILE_WRITE_THROUGH`) で行う。途中で失敗したら、逆の順に改名して元に戻す (`evorag.prev.old` も `evorag.prev` に戻す) |
| Linux | 前の版の写しを `evorag.tmp` に書いて書き切り、`rename` で `evorag.prev` に置く。次に、新しいファイルを `evorag.new` に書いて書き切り、`rename` で `evorag` の上に置き、フォルダを同期する。自分自身は改名しない (どの段で落ちても `evorag` がある)。動いている実行ファイルへ書き込むと失敗するので、上書きはしない |
| macOS | Linux と同じ (上書きすると署名の不一致で強制終了されることがあるため)。v0.6 の macOS 版のときに S-07 と同じ試験をする |

- **Windows の手順の理由** (S-07):
  - 動いている実行ファイルの上には、`MoveFileExW(MOVEFILE_REPLACE_EXISTING)` でも POSIX の意味の改名 (`FileRenameInfoEx`) でも置けない (アクセスの拒否)。
  - exFAT は POSIX の意味の改名を持たない。
  - 自分を改名してから置く手順だけが、NTFS と exFAT の両方で動く。
- **Windows の隙間**: 手順 3 と 4 の間は `evorag.exe` が無い。
  - その間に強制終了や電源断が当たると、起動できない。Windows では無くせない。
  - 3 と 4 の間には、ほかの処理 (書き込み・同期・表示) を入れない。
  - 戻し方を、同梱の README (§7.1) と `evorag update apply` の表示に書く: 「`evorag.exe` が無いときは、`evorag.new` (無ければ `evorag.prev`) を `evorag.exe` に改名する」。
- **ロック**: ウイルス対策のソフトがロックしている (共有の違反・アクセスの拒否・ロックの違反) 間は、5 回まで再試行する。
  - 待ちは 200・400・600・800 ms (合わせて 2 秒)。
  - それでも失敗したら、元に戻して理由を示す。起動できる状態は保つ (S-07)。
- **残ったファイルの片付け** (次の起動と、次の `update` の操作の前):
  - `update/` の状態のファイルに続きがあれば、その記録に従って続きから進める (§7.2)。
  - `evorag.prev` が無く `evorag.prev.old` か `evorag.tmp` があれば、それを `evorag.prev` に戻す。
  - 余った `evorag.new`・`evorag.tmp`・`evorag.prev.old` は消す。消せなければ (ロック)、次の機会に回す。
- **書き込めないとき**: インストール先に書き込めない (Program Files など) ときは、何も変えずに理由を示して断る。
  - 先に、一時のファイルを作って消せるかで確かめる。
- 技術検証は S-07 (08 §3、`spikes/S-07/REPORT.md`): Windows (NTFS・exFAT) と Linux (ext4) で、300 回の置き換えと巻き戻しのうち、起動できない状態は 0 回。

### 7.4 パッケージ管理で入れた場合

- 配布の経路 (zip、cargo、将来の winget・Homebrew など) は、ビルドのときに実行ファイルに埋め込む (パスから推測しない)。`evorag --version` に表示する。
- zip 以外の経路で入れた evorag では `apply` を断り、その管理の仕組みでの更新を案内する。

### 7.5 通信

- evorag が外へ通信するのは、`update check` と `update apply` を利用者が打ったときだけ (確定 (ユーザー回答、2026-11-08)、[ADR-0011](10-decisions.md#adr-0011-外へ通信するのは-llama-server-と手動の更新だけ))。起動時や定期の自動の確認はしない。
- 確認の通信には、evorag の版と OS の種類以外を含めない。
- 通信には HTTPS が要る。TLS は rustls と純 Rust の暗号の実装 (rustls-graviola) で組む (確定 (ユーザー回答、条件: S-06)、[OQ-05](10-decisions.md#oq-05-tls-と-c-を含む依存))。
- **信頼の拠り所は署名**: 配布物が本物かどうかは、TLS ではなく minisign の署名つきのマニフェスト (§5.2) で確かめる。TLS が破られても、偽の配布物は署名の検証で拒否される。TLS が守るのは、通信の内容 (版と OS の種類) と、古いリリースの一覧を見せられる「凍結」への耐性である。凍結に備えて、`update check` は最新のリリースの公開の日付を表示し、`release.json` の日付が今の版より古ければ拒否する。
- **CPU の機能が足りないとき**: graviola は x86_64 / aarch64 で特定の CPU の機能を必須とし、無い CPU での代替の実装を持たない。足りないと、最初の暗号の処理で panic する。`evorag update` は通信の前に CPU の機能を確かめ、足りなければ通信せずに理由を示し、手作業の更新の手順 (§7.1) を案内する。
  - 確かめる機能 (graviola 0.4.5 の `is_cpu_supported` と同じ。S-06): x86_64 は aes・pclmulqdq・bmi1・adx・avx・avx2、aarch64 は neon・aes・pmull・sha2。graviola の版を上げるときは、この一覧を確かめ直す。
- **証明書のルートが無いとき**: OS の検証器 (§2) は、Linux では OS の証明書ストア (`ca-certificates` など) を読む。見つからなければ通信せずに理由を示し、手作業の更新の手順 (§7.1) を案内する。
- GitHub の API の匿名の呼び出しの回数の制限 (1 時間に 60 回) に当たったら、理由を示して終わる。

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
