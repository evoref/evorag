# メモリの回帰ケース (v0.5): 忘れるの網羅・統治・導出・攻撃・障害の注入

v0.5 の書き込みの経路 (区切り・台帳・関門・導出、04) と統治 (指示 `directive`、04 §7) の事例。09 §3.5.2 の「忘れるの網羅の試験」の一覧と、09 §4 の v0.5 の注入の点を、**実装の前に** TOML に書いたもの (08 §2.6 の M5.0)。

- 走らせる側 (Rust) は、まだ書かない。事例の `stage` に書いたステップ (M5.1〜M5.4) で、そのステップの API ができたときに書く (まだ無い API を参照する骨組みはコンパイルできないため)。
- 今の v0.4 の事例 (`tests/memory-cases/*.toml`) は、旧エンジンとの比較の台 (09 §3.5.1) で使うので変えない。この下の事例は、v0.4 の書式 (`../README.md`) を広げたもの。
- 会話はすべて架空で、実在の人物・個人情報・利用者の会話の実例を含まない (11 §9.7)。
- 項目名は 04 の Rust の長い名前 (`sentence`・`layer`・`entity`・`aspect`・`value`・`mood`・`tense`) で書く (04 §5.2 の「wire の名前 ↔ Rust の名前」、OQ-53 の 2)。台帳の要素も長い名前で書き、走らせる側が wire の形 (`s`・`l`・`e`・`a`・`v`・`m`・`tn`) に写す。
- LLM の出力 (B2 の台帳の `output`) は、事例の中に固定して書く。B2 の呼び出しは記録と再生 (09 §3.3) にし、LLM を使わずに記録の状態を完全に比べる。

## ファイル

| ファイル | ID | 中身 | 仕様 |
|---|---|---|---|
| `forget.toml` | `FG` | 忘れるの置き場所ごとの事例 (09 §3.5.2 の表)、5 つの事例の (1)〜(3)、`end`・`unblock`・訂正の候補の表 | 04 §7.1〜§7.6・§7.9 |
| `governance.toml` | `GV` | private と解除・採る・戻す・削除の指示・`import-v04` の `end`・古い根の処分。5 つの事例の (4) | 04 §7.4・§7.5、03 §12.4 |
| `derive.toml` | `DR` | 導出の決定性・指紋・プロファイル・辞書の版を上げた前後。5 つの事例の (5) | 04 §5.1・§5.3・§5.4・§7.6 |
| `attacks.toml` | `AT` | 同梱の攻撃の事例 (09 §3.5.2 の「同梱の攻撃の事例」、AC-NFR-504-3) | 04 §5.2・§5.4・§5.5・§7.8 |
| `faults.toml` | `FI` | 09 §4 の v0.5 の注入の点 (強制終了・停止) | 09 §4、03 §6・§10.2・§12.4 |

注: 09 §3.5.2 は攻撃の事例を `tests/memory-cases/attacks.toml` に置くと書き、09 §3.5.1 は v0.5 の事例を `tests/memory-cases/` に足すと書いている。M5.0 では、v0.4 の事例と分けるために `v0.5/` の下に置いた (09 の置き場所の書き方は、この PR の取りまとめで合わせる)。

## 共通の項目

| 項目 | 内容 |
|---|---|
| `id` | 事例の ID。ファイルの中で一意 |
| `title` | 何を確かめるか (1 行) |
| `lang` | データ根のプロンプトの言語 (`ja`・`en`)。省くと `ja`。伏せ字の印は `ja` が `[伏せ字]`・`en` が `[redacted]`、想起の伏せは `ja` が `[非表示]`・`en` が `[hidden]` (04 §7.3・§7.5) |
| `spec` | 根拠の仕様の節 |
| `ac` | 確かめる AC の並び |
| `stage` | 走らせる側を書くステップ (`M5.1`〜`M5.4`)。事例の全体が走るのは、このステップから |
| `modes` | 任意。`["redact", "keep"]` なら、事例を 2 回流す。1 回目は `redact` を書いていない `forget` の操作をすべて伏せ字にする (`redact = true`)、2 回目は伏せ字にしない (`redact = false`)。09 §3.5.2 の「伏せ字にする忘れるとしない忘れるの両方で流す」 |
| `points` | 任意 (`faults.toml` だけ)。注入の点の名前の並び。点ごとに事例を 1 回ずつ流す (下の「障害の注入」) |
| `markers` | 任意。試験の目印の文字列 (他の本文と重ならない値)。`expect` の `scan` で、置き場所のバイト列に無いことを確かめる |
| `tz` | 任意。ターンに記録する UTC からのずれ (`"+09:00"` の形)。省くと `"+09:00"` |
| `note` | 任意。解釈を決めた点など、事例を読む人への注 |

## 段 (`[[case.step]]`)

各段は、次のどれか 1 つの項目を持つ。書いた順に当てる (v0.4 の「続いた候補を束として並べ直す」規則は無い。台帳は区切りごとの 1 回の呼び出しの出力で、導出は全件を入力の順に畳む、04 §5.3)。

| 段 | 項目 | 意味 |
|---|---|---|
| ターン | `turn = { id, session, at, role, text, reply_to, tz, long_input }` | ターンを作る。`role` は `user` か `assistant` (省くと `user`)。`reply_to` は応答だけで、省くと同じセッションの直前の利用者の発話。`at` は時刻で、ID の順と同じ順にする。`tz` を `"none"` にすると UTC からのずれを記録しない (v0.4 より前の発話、04 §5.1 の `no_offset`)。`long_input = true` は、本文の全体が端末の 1 回の読み取りで届いた長い一度の入力 (暫定 400 文字を超える) として観測値に範囲を残す (04 §5.2 の前のふるい) |
| 区切り | `segment = { name, pairs, closed }` | 区切り (04 §2) の期待。`pairs` は往復の利用者の発話のターンの ID の並びで、区切りの決め方がちょうどこの往復の並びの区切りを作ることを確かめる (名前は事例の中だけのもの。区切りの ID は 04 §2 の決め方で決まる)。`closed` は省くと true (走らせる側は、閉じる時刻まで時計を進めてから区切りを決める) |
| 呼び出しの開始 | `call = { seg }` | その区切りの B2 の呼び出しを始める (入力を読み、要求を偽の llama-server に送り、記録する)。次にその区切りの `ledger` の段が来たときに、この時点で読んだ入力 (`sentences` の改訂の番号・`forgets_seen`・`corr`) で台帳を書く。呼び出しの途中の忘れるを作るために使う |
| 台帳 | `ledger = { name, seg, model, variant, fallback, status, rules, keys, units, corrections }` | B2 の出力。その区切りの呼び出しを (前に `call` が無ければこの時点で) 行い、この出力を返して、台帳を書く経路 (書く前の確かめと掃除、04 §4・§7.5) で書く。`name` は事例の中の台帳の名前 |
| 操作 | `op = { kind, ... }` | 下の表 |
| 期待 | `expect = { ... }` | その時点の状態 (下の表) |
| 細工 | `tamper = { id, text }` | 記録のターンの本文を、規則を通さずに (改訂の番号を変えずに) 書き換える (v0.4 の `tamper` と同じ。伏せ字の中止の試験だけに使う) |

`op` と `ledger` の段は、任意で `crash` を持てる。`crash = true` は `faults.toml` の `points` の回の点で、`crash = "<点>"` はその名前の点で、その段の操作の途中に強制終了し、起動し直さない (続く `restart` の段で起動し直す)。点の名前は下の「障害の注入」の表。

### 台帳 (`ledger`) の項目

| 項目 | 内容 |
|---|---|
| `model` | 呼んだモデルの名前 (事例の中だけの名前。`model_key` に写す)。省くと、その時点の今の選択のモデル |
| `variant` | 変種。省くと `v1` |
| `fallback` | true なら縮退で chat のモデルで呼んだ台帳 (`inputs` の `role_fallback`、04 §5.2.1)。`model` は chat のモデル (`chat`)、`role_model_key` は今の選択のモデル |
| `status` | `complete` (省いたとき)・`partial`・`failed` |
| `rules` | `now` (省いたとき) か `old`。`old` は、前のふるいの規則の版が今と違う台帳 (版の不一致の段、04 §5.2.1) |
| `keys` | 区切りの鍵の並び |
| `units` | 単位の候補の並び。各々 `{ sentence, layer, entity, aspect, value, mood, tense, old, when }`。省いた項目は読むときの型の既定値 (`layer` は `world`、`entity` は `self`、`mood` は `fact`、`tense` は `current`、`old`・`when` は "")。`sentence` は区切りの文番号 (前のふるいの後の利用者の発話を文の分け方 v1 で分け、区切りの中で 1 から通しで振ったもの、04 §5.2)。事例の発話は、文の数が分かるように書き、番号を注に書く |
| `corrections` | 訂正の判定の並び。各々 `{ id, verdict }`。`id` は、呼び出しの入力を作るときに字句の規則で見つけた訂正の候補 (`inputs` の `corr`) の番号で、区切りの発話の順に 1 から振る (04 §6) |

台帳の `sentences`・`inputs` (`through`・`refs`・`corr`・`forgets_seen`・`input_hash`) は、走らせる側が区切りと規則から作る (事例には書かない)。

### 操作 (`op`)

| `op.kind` | 項目 | 意味 |
|---|---|---|
| `derive` | `order`、`seed` | 全件の導出 (B3 → B4) を今行う (04 §5.3)。`order = "shuffle"` なら、台帳・指示を読む順 (区切りの到着の順・台帳の書かれた順・同じ区切りの複数の選択の台帳の順) を `seed` で乱す |
| `forget` | `name`、`target = { entity, aspect, value, free }`、`redact` (既定 true。`modes` があるときは段の側)、`history` (既定 false)、`echo` (既定 true)、`others` (既定 false) | 忘れる (04 §7.1)。見本を作り、計画のハッシュを添えて実行する。`aspect` は辞書の属性の名前か自由記述の属性の文字列で、`free` (省くと、辞書に同じ名前があれば false) で見分ける。`name` は後の `unblock` から指す名前 |
| `private` | `name`、`target`、`scope` (`successors` 既定 / `value_only`) | private にする (04 §7.5) |
| `private_off` | `target` | その行に効いているすべての private を解く |
| `accept` | `target` | `Contested` の値を今の値にする |
| `revert` | `target` | 履歴の値を今の値に戻す |
| `unblock` | `forget` | 名前で指した忘れるの指示の印を解く (04 §7.5) |
| `delete_session` | `session` | セッションを消す (04 §7.1) |
| `select` | `model`、`variant`、`fallback` | `memory` の役割の選択を替える (今の選択が直前の選択になる)。`fallback = true` は縮退に入る、`false` は縮退から出る (04 §5.2.1) |
| `dict` | `version`、`aliases = { <辞書の属性> = [<別名>…] }` | 同梱の辞書の版を上げ、別名を足す (04 §5.1)。全件を導き直す |
| `send` | `session`、`text` | その時点で chat に発話を送り、応答の要求 (文脈の全体) を偽の llama-server で記録する。発話のターンも作る (ID は `turn` の段で前もって決めない。期待では `chat_request` で見る) |
| `sleep` | `interrupt` | 短い待機の手順 (04 §4) を最後まで流す (`evorag sleep` と同じ段取り)。区切りの呼び出しが要るときは、続く `ledger` の段の出力を返す。`interrupt = "ctrl_c"` は最初の区切りの呼び出しの途中で Ctrl+C を送り、`"kill"` は強制終了する |
| `rules` | `name` | 同梱の規則の 1 つ (`prefilter`・`split`・`gates`・`reading`・`correction`・`dates`・`sensitive`・`derive`・`profile`) の版を上げる (04 §5.3 の指紋の成分) |
| `reset` | なし | `evorag reset --yes` (03 §10.2) |
| `journal_drop` | `group_line` | 直前の忘れる・削除の組の行のうち、名前の行 (`forget`・`end`・`turn`・`delete`・`private`) をジャーナルのファイルから除く (末尾でない場所の欠けた組を作る試験の細工) |
| `prefetch` | `session`、`text` | 先読み (05 §4.5) を始め、終わらせないまま次の段へ進む (飛んでいる先読み) |
| `backup` | `name` | 退避を作る (03 §7.1) |
| `restore` | `backup` | 名前で指した退避から戻し、ジャーナルを当て直す (03 §10) |
| `export` | なし | 書き出す (03 §11)。断られたら `expect` の `refused` で見る |
| `restart` | なし | 閉じて開き直す (起動の順、04 §7.2: 取り込み → 開くときの削除 → compact → 書き手 → 忘れるの当て直し) |
| `jobs` | なし | 残っている job (`purge_indexes`・`forget_backups` など) を最後まで流す |
| `quarantine` | `turn` か `ledger`、`copy` | 記録を隔離する (03 §8。読めない記録を模す)。`copy = true` は、記録を DB に残したまま、同じ ID の本体の写しを隔離に置く (前に壊れた版が隔離され、ジャーナルから戻った状態を模す) |
| `forget_key` | `available` | `memory.forget_key` が使える・使えないにする (03 §3.1.1 のフェイルクローズを模す) |
| `import_v04` | `v04`、`old_root` | v0.4 のデータ根から取り込む (03 §12.4)。前の段の `turn` は v0.4 の根の会話 (伏せ字は済んだ本文で書く)。`v04` は v0.4 の根の事実と指示の表 (下の「v0.4 の根」)。`old_root` は `kept` (既定) |
| `dispose_old_root` | `changed` | 古い根を処分する (03 §10.2)。`changed = true` は、取り込みの後に v0.4 で古い根を使った (要約が変わった) 状態にする |

`target` は `{ entity, aspect, value, free }` (`entity` を省くと `self`)。v0.4 の `s`・`a`・`v` の代わりに長い名前で書く。

### v0.4 の根 (`import_v04` の `v04`)

`v04 = { facts = [...], directives = [...] }`。`facts` は v0.4 の根の事実の表の各行 `{ entity, aspect, value, state, sources }` (`state` は `Live`・`Contested`・`Superseded`・`Retracted`。`Retracted` は `value` に忘れる前の値を書き、走らせる側が値の鍵だけを使う)。`directives` は v0.4 の `fact_directive` の各行 `{ op, target, redact, at }` (`at` は v0.4 の事実の出典の範囲 `{ turn, text }`。`text` はその範囲の文字列で、走らせる側がバイト位置に直す)。

## 期待 (`expect`)

書いた項目だけを比べる。`mode = "redact"` か `mode = "keep"` を書いた期待は、その回だけに当てる。`point = [...]` を書いた期待は、その注入の点の回だけに当てる。

| 項目 | 内容 |
|---|---|
| `units` | その時点のすべての単位 (完全に比べる)。各々 `{ layer, entity, aspect, free, value, state, tense, private, anchors, by, against, when }`。`layer` は省くと `world`、`entity` は `self`、`free` は false、`tense` は `current`、`private` は false。`state` は `world` だけ (`Live`・`Contested`・`Superseded`)。`anchors` は根拠のターンの ID の集合 (書いたときだけ比べる)。`by`・`against` は相手の単位の値 (`by = "(none)"` は `by` が無い。書いたときだけ比べる)。`when` は `{ from, to, basis }` (書いたときだけ比べる。`from`・`to` を省くと null) |
| `not_live` | `Live` であってはならない組 `"<entity>/<aspect>=<value>"` の並び (09 §3.5.2 の「忘れた値の前の値が Live でない」を目立たせるために書く。`units` と重ねて書いてよい) |
| `profile` | プロファイルの行 `"<entity>/<aspect>=<value>"` の並び (完全に比べる。`planned` は値の後に ` (予定)`)。属性の優先の順と entity の順は並びの順で比べ、同じ (属性, entity) の中の順 (値の鍵の順。データ根の鍵に依る) は集合で比べる。`[]` は行が無い。`{ absent = true }` は、プロファイルが文脈に置かれない (鍵が使えないとき) |
| `seg_keys` | `{ <区切りの名前> = [<鍵>…] }`。書いた区切りの、検索の索引に入っている区切りの鍵 (完全に比べる) |
| `unit_keys` | 単位の鍵の索引の項目 `"<aspect> <value>"` の並び (完全に比べる) |
| `ledgers` | 台帳の部分の期待の並び。各々 `{ name, present, unchanged, units, keys, corrections, corr, scrubbed, input_hash, status, rev }`。`units` は残った候補の `"<aspect>=<value>"` (主語が `self` でなければ `"<entity>/<aspect>=<value>"`)、`keys` は残った鍵、`corrections` と `corr` は残った判定と候補の表の `id`、`scrubbed` は掃除で除いた要素の数の累計、`input_hash` は `"null"` か `"set"`、`rev` は `sentences` の改訂の番号の組 `{ <turn> = <rev> }`、`unchanged = true` は前の期待の時点からバイト列まで同じ、`present = false` は台帳が無い |
| `turns` | ターンの本文 `{ id, text, rev }` (`rev` は改訂の番号。書いたときだけ比べる) |
| `recall` | 想起で添えるときの往復の本文 `{ turn, text }` |
| `history` | 今のセッションの履歴を chat に送るときの本文 `{ turn, text }` |
| `chat_request` | 直前の `send` の要求の本文 `{ present = [...], absent = [...], profile_same_as }`。`profile_same_as` は、前の `expect` の名前 (`label`) を指し、その時点の要求とプロファイルの部分がバイト列まで同じ |
| `b2_request` | 直前の区切りの呼び出しの要求の本文 `{ seg, present = [...], absent = [...] }` |
| `requests` | 前の期待の後に出た LLM の要求の数 (B2・chat・埋め込みを合わせる。埋め込みを分けるときは `embed_requests`) |
| `ends` | その時点のすべての `end` の指示 (完全に比べる)。各々 `{ entity, aspect, free, value, turn, origin }`。`value` は終える値 (走らせる側が値の鍵と照合の鍵で `vks` と比べる)、`turn` は終えた位置のターンの ID (指示の ID が位置のときは `"(self)"`)、`origin` は `user`・`delete`・`import` (省くと `user`)。`end` の `origin` は、忘れるで足したものを `user` とする |
| `directives` | 足された指示 (`end` を除く) の部分の期待の並び `{ op, entity, aspect, value, scope, origin }` |
| `hits` | 指示ごとの当たる単位の集合 `{ directive, units = ["<entity>/<aspect>=<value>"…] }` (完全に比べる。`directive` は `op` の `name`)。当たる単位は、その指示を除いて導いた単位のうち、その指示の当て方 (04 §7.5) で当たるもの |
| `prefetch_used` | 直前の `send` が先読みの結果を使ったか |
| `events_absent` | 直前の操作の中で起きてはならない観測の点の名前の並び |
| `counts` | 導出で数えた理由の件数 `{ <理由> = <数> }` (書いた理由だけ比べる。理由の名前は 04 §5.2・§6 の括弧の中のもの) |
| `fingerprint` | `"changed"` か `"same"` (前の期待の時点の導出の指紋と比べる) |
| `same_as` | 前の期待の `label` を指し、単位の集合・本体・導出の指紋がバイト列まで同じ |
| `label` | この期待の名前 (`same_as`・`profile_same_as` から指す) |
| `plan` | 直前の忘れるの見本 `{ affected = ["<entity>/<aspect>"…], others = <数>, warnings = [...] }` |
| `aborted` | true なら、直前の操作が中止され、状態が操作の前と変わらない |
| `refused` | 直前の操作が断られた理由 (`old_root_kept`・`reapply_pending`・`old_root_changed` など)。状態は変わらない |
| `quarantine` | 残っている隔離の記録 (`{ turn }` か `{ ledger }`) の並び (完全に比べる) |
| `journal` | `{ turn_puts = [{ id, revs }], no_value_in_directives }`。`revs` はジャーナルに残る、そのターンの `put` の行の改訂の番号の集合。`no_value_in_directives = true` は、`directive` の行に値の文字列が無い |
| `backups` | 退避ごとの期待 `{ name, rewritten, ledgers, units, fingerprint, turns }`。`rewritten` は写し直されたか (false は飛ばされた)、`units`・`fingerprint` は写しに単位・指紋があるか |
| `export` | `{ ledgers, units, directives }`。`ledgers` は台帳ごとの `{ units, keys }`、`units = false` は単位が書き出されない、`directives` は書き出された指示の `op` の集合 (`end` を除く) |
| `events` | 操作の中で起きた順の部分列 (`prefetch_stopped`・`background_stopped`・`generation_bumped`・`commit`・`index_purged`・`slot_erase:chat`・`slot_erase:memory`・`backups_rewritten` など。名前は走らせる側の観測の点) |
| `doctor` | `doctor` (または `doctor --deep`) が報告する項目の名前の並び (書いた項目があることを確かめる) |
| `needs_call` | 呼び直すかの判定 (`needs_call`) が真の区切りの名前の集合 (完全に比べる) |
| `chunks` | `{ turn, rev }` の並び。そのターンのチャンクが、その改訂の番号で索引にあり、前の改訂の番号のものが今と前の世代のどの索引にも無い |
| `scan` | `{ absent = [...], places = [...] }`。`absent` の文字列 (既定は事例の `markers`) が、`places` の置き場所のバイト列に無い。置き場所の名前は `turns`・`ledgers`・`units`・`seg_keys`・`unit_keys`・`embeddings`・`chunks`・`lex_index`・`vec_index`・`profile`・`prefetch`・`quarantine`・`journal`・`backups`・`export`・`b2_request`・`chat_request`・`observations`・`db_file` (compact の後の `evorag.redb` のファイル)・`old_root`・`backup_dir` |
| `old_root` | `{ intact, exists }`。`intact = true` は古い根のすべてのファイルの SHA-256 と更新時刻が取り込みの前と同じ |
| `new_root` | `import-v04` の新しい根の状態: `"swapped"` (入れ替わった)・`"swapped_or_absent"` (入れ替わったか、作りかけが消えているかのどちらか) |
| `root` | `{ exists }`。今のデータ根のフォルダ (と `*.deleting-*`) があるか |
| `others_intact` | true なら、ほかのデータ根・その退避・データ根のフォルダの外のファイルが変わらない |
| `config_kept` | true なら、設定 (`config.toml`) と状態のフォルダが残る |
| `exit` | 直前の `sleep` などのコマンドの終了コード (07 §5) |
| `stop_within_ms` | 停止の操作から、裏の要求が止まるまでの時間の上限 (ミリ秒) |

## 障害の注入 (`faults.toml`)

`points` の各点で、`crash = true` を持つ段の操作の途中に強制終了 (子プロセスを kill) を入れ、続く `op = { kind = "restart" }` で起動し直す。`expect` は、`point` を書いていなければすべての点に当てる。点の名前は次のとおり (09 §3.5・§4)。

| 点 | 止める所 |
|---|---|
| `call_in_flight` | 区切りの呼び出しの途中 (逐次表示の受信中) |
| `before_ledger_commit` | 台帳の `Immediate` のコミットの前 |
| `after_ledger_commit` | 台帳の `Immediate` のコミットの後 |
| `during_unit_write` | 単位と導出の指紋 (`None`) の書き込みの途中 |
| `after_forget_line` | 忘れるの 1 回の追記で、`forget` の行の後・`end` の行の前 (組がそろわない) |
| `after_end_line` | `end` の行を書いた後・伏せ字のターンの行の前 (組がそろわない) |
| `after_journal_group` | 組の行をすべて書いた後・DB のコミットの前 (v0.3 の「指示の行と伏せ字の後のターンの行を書いた後」) |
| `before_txn_commit` | 指示・伏せ字・台帳の掃除・導き直しの 1 つのトランザクションのコミットの前 (ジャーナルは書き終えている) |
| `after_commit_before_purge` | コミットの後・索引からの除去とスロットのキャッシュの消去の要求の前 |
| `after_commit_before_journal_rewrite` | DB のコミットの後・ジャーナルの書き直しの前 |
| `journal_rewrite_tmp_written` | ジャーナルの書き直しで、一時名のファイルを書いた後・入れ替えの前 |
| `journal_rewrite_after_swap` | 入れ替えの後・追記の取っ手を開き直す前 |
| `backup_rewrite_after_first` | 退避の写し直しで、1 つ目の退避を写した後・残りの前 |
| `backup_rewrite_during_scrub` | 退避の写し直しで、写しの中の台帳を掃除している途中 |
| `compact_midway` | compact の途中 (`compact.pending` が残る) |
| `reapply_after_turn_line` | 当て直しがターンの伏せ字の行をジャーナルに書いた後・そのコミットの前 |
| `after_delete_line` | 削除の 1 回の追記で、`delete` の行の後・`end`・`private` の行の前 |
| `import_during_write` | `import-v04` の新しい根の書き込みの途中 |
| `import_after_verify` | `import-v04` の照合の後・改名の前 |
| `import_after_first_rename` | `import-v04` の 1 つ目の改名の後 |
| `reset_after_rename` | `reset` の改名の後 |
| `reset_during_backup_delete` | `reset` の外の退避の置き場所の削除の途中 |
| `reset_during_content_delete` | `reset` の中身の削除の途中 |

## 走らせる側が全件に当てること

- **冪等**: `forget` の操作のある事例は、忘れるの指示を当て直しても (`restart`) 結果が変わらないことも確かめる (04 §7.2)。
- **導出の決定性** (09 §3.5.2): `forget` の操作のある事例は、各 `forget` の後の期待の時点で、導出を到着の順を乱して 2 回流し (`order = "shuffle"`、種は事例の ID から決める)、単位の集合・本体・指紋が同じになることを確かめる。
- **LLM の要求が出ない**: `forget`・`private`・`private_off`・`delete_session` の操作から次の期待までに、B2 の要求が出ない (04 §7.2 の台帳の掃除。AC-FR-318-2)。

## 09 §3.5.2 の一覧との対応

| 一覧の行 | 事例 |
|---|---|
| ターンの本文 | FG01 (伏せ字 / `[非表示]`)、GV01 (private の `[非表示]`) |
| 台帳 (すべての選択) | FG02 (掃除の規則 1〜4・位置の写し直し・`input_hash`・`scrubbed`・要求なし)、FG03 (今・直前・縮退・ほかの選択・`partial`)、FG31 (`unblock` の後)、GV02 (private は台帳を変えず `keys` を導出でふるう) |
| 単位 | FG20〜FG24 |
| 検索の鍵と語彙・ベクトルの索引 | FG06、GV01 (private は鍵を作らない) |
| 単位の索引 | v0.6 で作るので、v0.5 には事例を置かない |
| プロファイル | FG07、GV01 |
| 先読みの結果・スロットのキャッシュ | FG08 |
| `quarantine` | FG09 |
| ジャーナル | FG10 |
| 退避 | FG11 |
| 書き出し | FG12 |
| llama-server への要求 | FG13 |
| 観測値 | FG14 |
| 古い根 (取り込みの後) | GV13・GV14 |
| 再起動の後の DB のファイル | FG15 |
| 5 つの事例 (1)〜(5) | (1) FG20〜FG24、(2) FG30〜FG32、(3) FG40〜FG42、(4) GV01・GV02、(5) DR01・DR02 |
| 同梱の攻撃の事例 | AT01〜AT11 |
| 導出の決定性 | DR03・DR04 と、上の「走らせる側が全件に当てること」 |
| 09 §4 の v0.5 の注入の点 (と §3.5 の v0.3 の止める点) | FI01〜FI11 |
