# v0.4 のデータ根の見本 (`tests/fixtures/db/0.4.0/`)

v0.4.0 のコードの書き込みの経路で作ったデータ根。v0.5 の `evorag import-v04` の試験 (03 §12.4、AC-FR-219-1・AC-FR-219-4) が読む (09 §3.6 の「v0.4 のデータ根からの取り込み」、08 §2.6 の M5.1)。

- **変えない**。v0.4.0 が書いたバイト列のまま置く。中身が変わっていないことは `crates/evorag-store/tests/fixture_db_v04.rs` が `MANIFEST.sha256` で確かめる (開かない。開くのは取り込みの試験)。
- 試験は、この根を一時のフォルダに写してから使う (取り込みは古い根に書かないが、ロックのファイルを作るため。ファイルの更新時刻は Git が保たないので、AC-FR-219-1 の「古い根が変わらない」は写した後の値と比べる)。
- 会話はすべて架空で、実在の人物・個人情報・利用者の会話の実例を含まない (11 §9.7)。`memory.forget_key` は作ったときの OS の乱数で、秘密ではない。
- テキストのファイルも含めて、Git の改行の変換をしない (`.gitattributes` の `tests/fixtures/** -text`)。

## ファイル

| パス | 中身 |
|---|---|
| `root/` | データ根 (`evorag.redb`・`journal/`・`index/`)。v0.4.0 が作ったファイルのうち、`lock`・`lock.pid`・`reserve.space` (16 MiB の予備の領域)・`backups/` (日ごと・週ごとの退避、各 1 MiB) は除いた (どれも開くときに作り直されるか、取り込みが読まないもの) |
| `MANIFEST.sha256` | `root/` のすべてのファイルの SHA-256 (`sha256sum` の形。このフォルダで `sha256sum -c MANIFEST.sha256` で確かめられる) |
| `gen/gen_v04_root.rs` | 根を作った生成の器具 (v0.4.0 の木で `evorag-core` の example として走らせる。この workspace では作らない) |
| `gen/inspect_v04_root.rs` | 根を何も書かずに v0.4.0 の点検 (`evorag_store::inspect`) で読み、数と中身の要約を出す器具 |
| `gen/inspect.txt` | `inspect_v04_root` の出力 (下の数と ID の元。`memory.forget_key` の値は伏せた) |

大きさは合わせて 1,107,894 バイト (`evorag.redb` が 1,056,768、ジャーナルが 41,751、語彙の索引が 32 ファイルで 9,375)。

## 中身

`meta`: `format.version` = `0.3`、`created_by`・`last_writer`・`max_writer` = `0.4.0` (v0.4 で書き込みで開いた根、03 §12.4 の取り込める根の (1))、`prompt.language` = `ja`、`journal.last_seq` = `109` (ジャーナルの最大の連番も 109。取り込んでいない行は 0 行)、`forget.recheck` と `compact.pending` は無い。`memory.forget_key` はある (DB とジャーナルの両方)。

DB の記録の数 (v0.4.0 の `evorag doctor` と `inspect` の数):

| 種類 | 件数 | 取り込み (03 §12.4) |
|---|---|---|
| `session` | 5 | 写す |
| `turn` | 26 | 写す (除くもの 2 件、下の「除くもの」) |
| `signal` | 1 | 写す |
| `recall_feedback` | 2 | 写す |
| `setting` | 2 | 写す |
| `model_binding` | 2 | 写す |
| `threshold` | 1 | 写す |
| `fact_directive` | 10 | `directive` に写す |
| `fact` | 9 | 写さない (`import.v04_ended` の表を作るために読む) |
| `chunk` | 26 | 写さない |
| `note` | 5 | 写さない |
| `model_probe` | 1 | 写さない |
| `job` | 1 | 写さない (セッションの無いターンの抽出の job が残っている) |

ジャーナル: 1 ファイル (`2026-11.jsonl`)、91 行。消したセッション 2 つの削除の行がある。忘れるの前の本文の行は v0.4.0 が書き直しで除いた (伏せ字にした 3 つの値の文字列は、DB にもジャーナルにも無い)。

### 09 §3.6 の項目と、当たる記録

時刻は 2026-11-15T00:00:00Z から (`ManualClock`)。ID は `gen/inspect.txt` のもの。

| 項目 | 記録 |
|---|---|
| 会話 | セッション 5 つ: 暮らしの話 `01a1dc8a-17c0-7000-a698-4080bab12a02` (8 往復)・近況 `01a1dca1-e580-7000-880e-43ed5da9a60f` (1 往復)・観葉植物 `01a1dcac-e200-7000-9098-7a47c025a152`・車 `01a1dcb7-de80-7000-9211-e20229fc1eaa` (各 1 往復)・今日 `01a1dcc2-db00-7000-b6a3-dec45265c099` (1 往復と、添えて作り直した応答 1 つ) |
| UTC からのずれを持つターン | `Send` で書いた発話のターン 12 件がすべて `obs.tz_offset` = `540` (+09:00。作った機械の時間帯が日本時間のため)。応答のターンと、セッションの無いターンは持たない |
| 伏せ字にした忘れる | `forget` `01a1dccf-ac40-7003-9386-7f8a1972b8d6` (dietary の「かに」)。出典 `01a1dc93-3f80-7000-a70c-d6df5d49ce30` は `rev` 1 で「実は、[伏せ字]も食べられないんです」 |
| 伏せ字にしない忘れる | `forget` `01a1dcd1-8100-7003-b460-25fd97f61d68` (hobby の「将棋」、`redact` 偽、`hidden` 1 つ)。出典 `01a1dc95-1440-7000-aa3f-7fb9fcd4241d` の本文に値が残る |
| private とその解除 | `private` `01a1dcca-2e00-7000-b884-f50d388dc242` (pet の「猫」、`successors`) と、それを解く `private_off` `01a1dccc-02c0-7000-9b96-d654f1c64b88`。解かずに残る `private` `01a1dccd-d780-7000-8747-d9fb9bc44a54` (dietary の「えび」、`value_only`。事実 `01a1dc91-6ac0-7004-ac7c-5b1c60b890ff` は `private` 真) |
| 採る・戻す | residence の矛盾の「東京」を `accept` `01a1dcc6-8480-7000-a7c9-2cf10f4bb044` で今の値にし、履歴の「大阪」を `revert` `01a1dcc8-5940-7000-b0b4-b1b94408ab71` で戻した。終わりの状態は 大阪 `live`・東京 `superseded` |
| 印を解く | likes の「チョコレート」を伏せ字にして忘れ (`forget` `01a1dcd3-55c0-7003-a3db-a190ee1d952a`、出典 `01a1dc96-e900-7000-8eb6-7bb2bf528e01`)、`unblock` `01a1dcd5-2a80-7000-8e49-039ddbf556df` で印を解いた |
| セッションの削除 | 雑談 `01a1dced-e2a0-7000-a57b-445f1ff82bc7` (1 往復) を消した |
| 置き換えの連鎖の新しい側の削除 (今の値なし) | 暮らしの話で occupation の「パン屋」(事実 `01a1dc8f-9600-7004-9e7e-b00e4c9c9e35`)、仕事 `01a1dce0-2700-7000-baae-83a057f55ba0` で「転職して、今は花屋で働いています」(変化の語で置き換え) と言い、仕事を消した。「パン屋」は `superseded` (`by` は消えた事実 `01a1dce1-fbc0-7004-af12-2be4b8beb6f5`) のまま、occupation に今の値が無い (OQ-49) |
| 想起への操作 | 今日の応答 `01a1dcc4-afc0-7002-8722-d16d8c3f0f43` の候補の帯で、車の発話 `01a1dcb9-b340-7000-9dc4-43bfb29af542` に `irrelevant`、観葉植物の発話 `01a1dcae-b6c0-7000-80a7-6d853561f4e6` に `attach` (作り直した応答 `01a1dcc4-afc0-7007-86ed-f4f59f79586c`) |
| 答え方の既定 | `setting` の `answer.length` = `short`・`answer.format` = `plain` |
| モデルの記録 | `model_binding` の `chat` (`/m/chat.gguf`) と `rerank` (`/m/rr.gguf`)。架空のモデル |
| ほか | `signal` 1 件 (`good`、暮らしの話の最後の応答 `01a1dc98-bdc0-7002-ab7a-07aacd555fc9`。下の伏せ字の合わないターンへの応答で、応答のターン自身は写す)。`threshold` 1 件 |

### 除くもの (03 §12.4 の「除くもの」)

| 理由 | ターン | 作り方 |
|---|---|---|
| (a) セッションの無いターン | `01a1dd27-9040-7000-bdd7-32262feb6e95` (`session` は消した雑談の ID、状態 `pending`、`obs` 無し) | 古い版で戻したセッションに足した会話をまねて、v0.4.0 の `Store`・`Writer` で直接書いた (ターンの `put` と `in_session` の関係) |
| (b) 伏せ字の合わないターン | `01a1dc98-bdc0-7000-ad0d-723fd1859e5b` (birthday の「5月3日」を伏せ字にして忘れた `forget` `01a1dcd6-ff40-7003-b5b1-461ad985b3e8` の出典) | 伏せ字の後の `rev` 1 のまま、本文を「誕生日は[伏せ字]でした」に変えた (v0.4.0 の `Writer` の `Patch`)。v0.4.0 は合わないターンとして残し、`doctor` が 1 件と示す |
| (c) 対象のターンを写さなかった `signal`・`recall_feedback` | 無い | — |

期待する取り込み: ターン 24 件 (26 − 2)、`directive` 10 件 (元の ID の集合が `fact_directive` と同じ)。

### 断る根 (AC-FR-219-2) の作り方

断る根は同梱しない。取り込みの試験で、この根を写してから次のように変えて作る (v0.4.0 の書き手を使わずに、redb の `meta` の表かジャーナルのファイルを直接変える)。

- `forget.recheck` が立った根: `meta` に `forget.recheck` = `1` を書く。
- 取り込んでいないジャーナルの行がある根: `meta` の `journal.last_seq` を下げる (たとえば `108`。最後の行が取り込まれていないことになる)。
- v0.3 以前の版だけで使った根: `meta` の `max_writer`・`last_writer` を `0.3.0` にし、`writer.0.4.0` を消す。
- v0.2 の形式の根: `meta` の `format.version` を `0.2` にする (または `crates/evorag-store/tests/format_0_2.rs` と同じ作り方で 0.2 の根を作る)。

## 作り方

- コード: タグ `v0.4.0` (コミット `40e8cc3288073f3a18423283687439a047aec525`) を `git archive v0.4.0` で空のフォルダに展開した木。Rust 1.99.1 (x86_64-pc-windows-msvc)。作った日: 2026-11-27。作った機械の時間帯は日本時間 (Tokyo Standard Time)。
- 手順:
  1. `gen/gen_v04_root.rs` と `gen/inspect_v04_root.rs` を、展開した木の `crates/evorag-core/examples/` に写す。
  2. `cargo run -p evorag-core --example gen_v04_root --locked -- <空の出力先>/root` で根を作る。
  3. `cargo run -p evorag-core --example inspect_v04_root --locked -- <出力先>/root` で数と中身を確かめる (何も書かない。前後で `evorag.redb` とジャーナルの SHA-256 が変わらないことを確かめた)。
  4. `evorag.redb`・`journal/`・`index/` をこのフォルダの `root/` に写し、`MANIFEST.sha256` を作り直す。
- `gen_v04_root` の中身:
  - llama-server の代わりに、要求の型縛りの名前 (`response_format` の `json_schema.name`) で答えを選ぶ `Scripted` を使う。応答は「了解です」、抽出 (`fact_candidates`) は発話の語から決めた候補、ノート (`note_keys`) は `["会話"]`、訂正 (`correction`) は当たらない答え。リランカーは v0.4.0 の `ReplayClient` の再生 (共通の文字の数 − 10)。応答のモデルの点検の結果とリランカーのしきい値は、起動の前に `DataRootModels` で書く (v0.4.0 の試験と同じ)。
  - 1 回目の起動 (`Core::start`、版 `0.4.0`): コアの命令 (`NewSession`・`Send`・`Rate`・`ToggleIrrelevant`・`AttachBand`・`AcceptFact`・`ShowFactHistory`・`RevertFact`・`SetFactPrivate`・`UnsetFactPrivate`・`PreviewForget`・`ForgetFact`・`ShowForgotten`・`UnblockFact`・`OpenSession`・`DeleteSession`・`Style`) で上の会話と操作を行う。裏の処理 (抽出・統合・事実の表・ノート・チャンク) は v0.4.0 のものがそのまま動く。
  - 閉じた後に、v0.4.0 の `Store` と `Writer` で直接開いて、除くもの (a)・(b) を書く。
  - 2 回目の起動 (`Core::start`、版 `0.4.0`) で開き直して閉じる (ジャーナルの取り込みと忘れるの当て直しを済ませ、`forget.recheck` を残さない。起動の結果は合わないターンを 1 件と示す)。
- 決定性: 時刻は `ManualClock`、ID は種を決めた `UuidV7Gen` だが、裏の処理と並ぶ書き込みの順で ID の下位が変わることがあり、`memory.forget_key` と `data_root.id` は乱数なので、作り直すと同じバイト列にはならない。見本はこのフォルダのものを正とする。
- v0.4.0 の配布物 (`bins/evorag`、同じ木から作った `evorag 0.4.0`) の `evorag --data-root <写し> doctor` は、形式の版 0.3・最後に書いた evorag 0.4.0・上の種類ごとの件数・忘れた内容と本文が合わない発言 1 件・ジャーナルの次の起動で取り込む行 0 を示す。

## MANIFEST

| パス | バイト | SHA-256 |
|---|---|---|
| `root/evorag.redb` | 1056768 | `2263de57050de36ed05e45c5c9950240a57b751a424b1558912748d9632308ac` |
| `root/index/memory/chunk-lex/CURRENT` | 31 | `49fc1bba4e29cb0e634c99f19e3a4f2c1d2198443f11bb39dec5af3febbf9501` |
| `root/index/memory/chunk-lex/manifest-0000000000000080.json` | 367 | `a87c47f516e29c3367bcf139fc5e1e80eb09f802bf811bcdb718517c34d84fc1` |
| `root/index/memory/chunk-lex/seg-0000000000000045/header.json` | 197 | `14e930463f5d53d27f34e219b10996ccab50c1061f9851b813799aeb8ed97ae8` |
| `root/index/memory/chunk-lex/seg-0000000000000045/ids.bin` | 44 | `8ecf1dc4ff7d71d8c7b2adb5416477915808c7715a3b3fb592ef8b6eab7e29eb` |
| `root/index/memory/chunk-lex/seg-0000000000000045/lex.bin` | 639 | `bc26096c8084f5cd7a71becfc0dc1824fb7a264b8e40d9b4bac5521fd2e227d1` |
| `root/index/memory/chunk-lex/seg-0000000000000047/header.json` | 197 | `76bd859cde04d4bb2a503f72c501efe3ac06d7054df6e87f3b8b5863230183cf` |
| `root/index/memory/chunk-lex/seg-0000000000000047/ids.bin` | 44 | `d7dfefb9b32928e6ead83d356d42ded0b2b2843bd13a7f79975d531a1186e7c0` |
| `root/index/memory/chunk-lex/seg-0000000000000047/lex.bin` | 116 | `c425ac643ee66fcac2ca829e30a93c8b56f889acf6c68caeb8cae9697e15d608` |
| `root/index/memory/chunk-lex/seg-0000000000000049/header.json` | 197 | `4f742efec98bbc1ae5c00aebcef9ab30755f458c5bb933a24d9fd9b3a2a81855` |
| `root/index/memory/chunk-lex/seg-0000000000000049/ids.bin` | 44 | `581aa15b9251c4e52b02c08707f50bebe5fab0be9468668fb39071cd825bd36c` |
| `root/index/memory/chunk-lex/seg-0000000000000049/lex.bin` | 72 | `ae088f6c5f9097286e65bfa7e3a4ff6a00bfdf9c81874dae7696c7dea242ad05` |
| `root/index/memory/chunk-lex/seg-000000000000004b/header.json` | 197 | `df78e842d983f11001a6706182df2b3ec2ec5ec117139adc0b20ecc423015951` |
| `root/index/memory/chunk-lex/seg-000000000000004b/ids.bin` | 44 | `42db25a269cbf53dcccf4956994858b27771069ad4701fc9c4bb44a68c7b5760` |
| `root/index/memory/chunk-lex/seg-000000000000004b/lex.bin` | 116 | `c425ac643ee66fcac2ca829e30a93c8b56f889acf6c68caeb8cae9697e15d608` |
| `root/index/memory/chunk-lex/seg-000000000000004d/header.json` | 197 | `9a3aa70928fbf61712ad9da49b52380d710442449f33cdd09c72bf0efc1b1fbb` |
| `root/index/memory/chunk-lex/seg-000000000000004d/ids.bin` | 44 | `1da924b8d63ece6c62e91b727ffc6b5ffc45bf4aff599110e66ad0447b5efb6c` |
| `root/index/memory/chunk-lex/seg-000000000000004d/lex.bin` | 243 | `40dbbc0d19465a94960f19aa080c4d6a6b9b8733093abfe3d7ca2f396ebc4074` |
| `root/index/memory/chunk-lex/seg-0000000000000051/header.json` | 197 | `a9b193840294670fbadc55f4a8c0456d520c3f6b97bc838692358153c8ef4b30` |
| `root/index/memory/chunk-lex/seg-0000000000000051/ids.bin` | 44 | `e2b4a5147d97c27ebbe1e58a5ebbe913c348be07d0f2029d68276af96398f4fd` |
| `root/index/memory/chunk-lex/seg-0000000000000051/lex.bin` | 86 | `2464e8c3ffe65cf27cc9c29687d2f56ef28485341b07896ba888f37ab6c15366` |
| `root/index/memory/chunk-lex/seg-0000000000000055/header.json` | 197 | `411fe6a89d5d2fd9f3c58a0eb400d15b2def7f0b82ad52c0e0ddee912b3abdd8` |
| `root/index/memory/chunk-lex/seg-0000000000000055/ids.bin` | 44 | `f4e9d36aae7c71865016a643d8b0bb061ccd7c1c8ba817145baf3eb066f10f46` |
| `root/index/memory/chunk-lex/seg-0000000000000055/lex.bin` | 72 | `ae088f6c5f9097286e65bfa7e3a4ff6a00bfdf9c81874dae7696c7dea242ad05` |
| `root/index/memory/chunk-lex/seg-0000000000000065/header.json` | 198 | `0383021e0e7e7579bdf81ed5a16d23ad1e1ba5cb35e61e8b6212adac762297cd` |
| `root/index/memory/chunk-lex/seg-0000000000000065/ids.bin` | 836 | `f0ea7a7b7e673a0a62848f9f8024087a9ec2a5a346e8e9bb4539c63fb462a9c4` |
| `root/index/memory/chunk-lex/seg-0000000000000065/lex.bin` | 3175 | `ba0b6cd0a98202a9f3948bc1b4acf59c80d65189d79e7269f73a207e11ad7887` |
| `root/index/memory/chunk-lex/seg-000000000000007b/header.json` | 197 | `19b2eac3404b4809a7e08a41743a915f756ea8404fe0f9cdaf6963a7f4bda047` |
| `root/index/memory/chunk-lex/seg-000000000000007b/ids.bin` | 152 | `d35395c37f6d422b606b6c1270a39caf34a0a40081bf5976161959561db02701` |
| `root/index/memory/chunk-lex/seg-000000000000007b/lex.bin` | 1075 | `f2984c0a3916029ad1dfc90fa62fc87386988342c84e4976f21ccd862b03a4f1` |
| `root/index/memory/chunk-lex/seg-000000000000007f/header.json` | 197 | `6166c21b0a89ab7724ee5648bc7fad356e8ce531b38180afebfeea2c6a8aa0fb` |
| `root/index/memory/chunk-lex/seg-000000000000007f/ids.bin` | 44 | `9f8f94ecbecd01173b1ec7e2903b25658747d8b7d9c602a9e4b73ec10171b2b6` |
| `root/index/memory/chunk-lex/seg-000000000000007f/lex.bin` | 72 | `ae088f6c5f9097286e65bfa7e3a4ff6a00bfdf9c81874dae7696c7dea242ad05` |
| `root/journal/2026-11.jsonl` | 41751 | `3adc6e1772288e97eedd5ecbf047dadebd54c52ed2ccc44bc90156fe88a3560a` |
