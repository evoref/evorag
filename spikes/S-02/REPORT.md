# S-02 llama-server の API の前提 (技術検証)

- 日付: 2026-11-09
- 対象: 08 §3 の S-02、06 の「(S-02)」の項目、OQ-14
- 判定: **合格** (06 の前提はおおむね正しい。違った点は下の「仕様に直す点」で 06・04・05 に反映する)

## 環境

- Windows 11、llama-server (llama.cpp を手元でビルド、`build_info` = `b1-0c0c221`、Vulkan)
- chat: Qwen3.5-9B (Q4_K_M)、`-np 2 -c 16384`、`--api-key`、`--jinja`
- embed: Qwen3-Embedding-0.6B (Q8_0)、`--embedding --pooling last`
- rerank: japanese-bge-reranker-v2-m3 (Q8_0)、`--reranking`
- 検査のスクリプト: `probe.py`、`probe2.py`、`probe3.py` (Python の標準ライブラリだけ)。結果: `probe-chat.json`、`probe-chat2.json`、`probe-embed-rerank.json`

## 結果

| 項目 | 06 の前提 | 結果 |
|---|---|---|
| `/health` | API キーなしで呼べる | 合う (200) |
| `/props` | `build_info`・`model_path`・`total_slots` を返す | 合う。API キーが要る (無いと 401) |
| `n_ctx` の位置 | `default_generation_settings` の中 | 合う。トップレベルには無い。値は **スロットごと** (`-c 16384 -np 2` で 8192) |
| `/v1/models` | モデルの名前と付帯情報 | `data[0].meta` に `n_params`・`size`・`n_ctx_train`・`n_embd`・`n_vocab` がある。別名を付けないと `id` はサーバ側の絶対パス |
| `/tokenize` | 使える | 合う (`with_pieces` も可) |
| `/slots` | `is_processing` がある | 合う。各スロットの `id`・`n_ctx`・`is_processing`・`id_task`・生成の設定などを返す |
| `/v1/chat/completions` での `id_slot` | 未確認 | **効く**。`id_slot: 1` の要求の間、スロット 1 だけが処理中になった |
| 逐次表示の接続を切ったときの停止 | 止まる | **止まる**。接続を閉じてから処理中でなくなるまで 0.15〜0.19 秒 (5 回)。サーバのログに `cancel task` |
| 逐次表示しない要求を切ったときの停止 | 未確認 | 止まる (切った 1.5 秒後に処理中でない) |
| 停止の条件 | — | **TCP の接続が実際に閉じること**。応答の読み取りのオブジェクトがソケットを持ったままだと、接続は閉じず、生成は最後まで続いた (検査の誤りで再現) |
| `/slots/{id}?action=erase` | 使える | **既定では使えない** (501: `--slot-save-path` を付けて起動したときだけ) |
| `response_format` の `json_schema` | 使える | 合う。`strict` の schema に合う JSON が返った |
| 決定性 (temperature 0・固定の seed) | — | **スロットを固定すると 5 回とも同じ** 出力 (`cache_prompt` の真偽、`top_k = 1` でも同じ)。スロットを固定しないと、別のスロットに割り当てられて出力が変わった |
| 思考の出力 | — | Qwen3.5 は既定で思考の部分を出すので、補助の呼び出しでは `chat_template_kwargs: {"enable_thinking": false}` が要った (モデルごとに違う) |
| `/v1/embeddings` | 使える | 合う。1024 次元、長さ 1 に正規化済み。chat のサーバは 501 で断る |
| `/v1/rerank` | `--reranking` が要る | 合う。`results[].relevance_score` は **0〜1 ではない生のスコア** (例: -5.9、-6.1、-10.9)。embed のサーバは 501 で断る |
| ルーターモード | — | 確かめていない (v0.2 の前、OQ-13) |
| `build_info` | build 番号で版の範囲を表す | 手元でビルドしたものは `b1-<commit>` になり、build 番号は版の目安にならない |

## 仕様に直す点

1. **06 §3**: `/props` には API キーが要る。`n_ctx` はスロットごとの値で、文脈の予算はこの値で計算する。`/v1/models` の `data[0].meta` を model_key の材料に使える (§5)。
2. **06 §4**: `id_slot` は `/v1/chat/completions` で効く。止めるときは応答の本体を捨てて接続を確実に閉じる (reqwest では応答を drop する)。停止は 0.2 秒ほどで、NFR-105 (1 秒以内) を満たす。
3. **06 §6**: 補助の呼び出しでは、思考を出すモデルの思考を切る設定 (`chat_template_kwargs`) を、モデルごとに設定で持つ。
4. **06 §10**: 手元でビルドした llama-server は build 番号が当てにならない。版の範囲の確認は、公式の配布物の build 番号のときだけ行い、それ以外は確認を省いて `doctor` に表示するだけにする。
5. **04 §7.2・06 §4**: スロットのキャッシュの消去は、既定の構成では使えない。`--slot-save-path` は KV キャッシュをディスクに書くので、evorag の案内では勧めない。スロットのキャッシュは次の要求で上書きされるので、消去は「約束しない範囲」に入れ、文言で伝える。
6. **05 §3.4**: リランクのスコアは生のスコアなので、しきい値はシグモイドで 0〜1 に写してから比べるか、リランカーのモデルごとに生のスコアのしきい値を持つ。
7. **13 §6.3**: 決定性はスロットの固定で得られることを確かめた (前提どおり)。
