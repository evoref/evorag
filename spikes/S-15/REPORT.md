# S-15 llama-server のルーターモード (技術検証)

| 項目 | 内容 |
|---|---|
| 検証 | 08 §3 の S-15 (S-02 の残り)、OQ-13 |
| 関わる仕様 | 06 §2・§3・§4・§5、07 §4.2・§5 |
| 実施 | 2026-11-12 |
| 再現 | 計測のスクリプト (Python の標準ライブラリだけ) は使い捨てで、取り込んでいない。条件は §2 |
| 判定 | **合格**。1 つのルーターに chat・embed・rerank を置け、06 §3 の照合と §4 の譲り方を、モデルの名前を付けるだけで同じ規則で動かせる。違う点は §4 に書き、06 に反映した |

## 1. 結論

- **1 つの llama-server に、3 つの役割のモデルを置ける。** プリセットの INI (`--models-preset`) で、モデルごとに `embedding`・`pooling`・`reranking`・`c`・`np` を書ける。起動から 3 つの読み込みが終わるまで 14 秒 (初回)。
- **モデルの選び方は 2 通りで、どの API も同じ。** POST は本文の `model`、GET は問い合わせ文字列の `?model=`。`/props`・`/slots`・`/tokenize` は `model` が無いと 400 (`model name is missing from the request`) で断る。知らない名前は 400 (`model '…' not found`)。
- **照合に要る値は、直接つないだときと同じ形で返る。** `/props?model=…` は、そのモデルの `build_info`・`model_path`・`total_slots`・スロットごとの `n_ctx` を返す。`/v1/models` の各モデルの `meta` に `n_params`・`size`・`n_vocab` がある。ただし **読み込んでいないモデルは `meta` を返さない**。
- **譲り方 (06 §4) は同じ規則で動く。** `id_slot` が効き、逐次表示の接続を切ると 0.28 秒で処理中でなくなった。chat の生成中に埋め込みとリランクを出しても、どれも成功した。
- **プロンプトのキャッシュも効く。** 先頭が同じ 2 つ目の要求で、処理したのは 27 トークン (キャッシュから 663 トークン)。
- **降ろしたモデルは、要求で読み込み直される** (既定の autoload)。読み込み直しは rerank 1.8 秒、chat 3.6 秒 (OS のファイルのキャッシュに載っている状態。初回はもっと長い)。autoload を切った (`--no-models-autoload`) ルーターでは、降ろしたモデルへの要求は 400 (`model is not loaded`) になる。

## 2. 条件

- 機械: S-13 と同じ Windows 11 のノート PC (CPU 内蔵の GPU を Vulkan で使う、主記憶 64 GB)。
- llama.cpp: 手元でビルドしたもの (`build_info` = `b1-138789d`)。ルーターは `--models-preset <INI> --host 127.0.0.1 --models-max 4`。
- プリセット:

```ini
version = 1

[*]
ngl = 99

[chat]
model = <モデルのフォルダ>\Qwen3.5-9B-Q4_K_M.gguf
c = 16384
np = 2
load-on-startup = true

[embed]
model = <モデルのフォルダ>\Qwen3-Embedding-0.6B-Q8_0.gguf
embedding = true
pooling = last
load-on-startup = true

[rerank]
model = <モデルのフォルダ>\japanese-bge-reranker-v2-m3-v1-q8_0.gguf
reranking = true
load-on-startup = true
```

## 3. 結果

| 項目 | 結果 |
|---|---|
| `/health` (ルーター) | 200 (起動から 0.6 秒) |
| `/props` (`model` なし) | 200。`"role": "router"`、`max_instances`、`models_autoload`、`build_info`。`model_path` は `none`、`n_ctx` は 0 |
| `/props?model=chat` | 200。`build_info` `b1-138789d`、`model_path` (モデルのファイルの絶対パス)、`total_slots` 2、`n_ctx` 8192 (スロットごと)、`model_alias` `chat` |
| `/props?model=nope` | 400 `model 'nope' not found` |
| `/v1/models` | モデルごとに `id` (プリセットの節の名前)、`status.value` (`loaded` など)、`status.args` (子の llama-server の引数)、`meta` (`n_params`・`size`・`n_vocab`・`n_ctx_train`・`n_embd`・`ftype`)。降ろしたモデルは `meta` が無い |
| `/slots?model=chat` | 200。スロット 2 つ (`n_ctx` 8192、`is_processing`)。`model` なしは 400 |
| `/tokenize` | 本文の `model` で 200。`model` なしは 400 |
| `/v1/embeddings` (`model: embed`) | 200、1,024 次元、71 ms |
| `/v1/rerank` (`model: rerank`) | 200、生のスコア (関係のある文書 −5.06、無い文書 −10.9)、71 ms |
| `/v1/embeddings` (`model: chat`) | 501 (直接つないだときと同じ) |
| 先頭が同じ 2 つの要求 | 1 つ目 667 トークンを処理。2 つ目 27 トークンを処理、663 トークンはキャッシュ |
| 逐次表示の接続を切る | 0.28 秒で処理中でなくなった |
| chat の生成中の埋め込みとリランク | どれも 200 (chat の応答は 9.3 秒) |
| `/models/unload` の後の要求 | 自動で読み込み直す: rerank 1.8 秒、chat 3.6 秒 |
| autoload を切って降ろしたモデル | `/props`・`/v1/embeddings` とも 400 `model is not loaded` |

## 4. 仕様に直す点 (06 に反映した)

1. **ルーターの見分け方**: `/props` (`model` なし) の `role` が `router` なら、ルーターモードとして扱う。役割ごとに、設定の `llm.<役割>.model` (プリセットの節の名前) を、すべての要求に付ける (`/props`・`/slots`・`/tokenize` を含む)。名前の無い役割は、そのルーターでは使えないものとして縮退する。
2. **model_key**: 計算の仕方は変えない (`/props?model=…` の `model_path` と、`/v1/models` のそのモデルの `meta`)。読み込んでいないモデルは `meta` を返さないので、照合の前に読み込みを待つ (`/props?model=…` が autoload を起こす。読み込み中は「接続待ち」と同じに扱う)。
3. **降ろされたモデル**: ルーターは、数 (`--models-max`) や主記憶の余裕で、使っていないモデルを降ろす。降ろされたモデルへの次の要求は、読み込み直しの時間 (数秒) を待つ。応答の経路の埋め込みとリランクは、締切 (05 §3.4) を過ぎたら縮退する。autoload を切ったルーターでの 400 `model is not loaded` は、その役割が無いものとして縮退し、状態の行で知らせる。
4. **譲り方とスロット**: スロットはモデルごと。`id_slot` と `/slots?model=…` の `is_processing` を、モデルごとに使う。06 §4 の規則はそのまま当てる。
5. **setup**: ルーターを見つけたら、`/v1/models` の名前から役割を見分ける候補を出し、利用者に選ばせる。勧める構成 (3 つの役割) を 1 つのプロセスで動かす手段として、プリセットの例 (上の §2) を示す (llama-server を 3 つ起動する手間を減らす)。

## 5. 限界

- 機械は 1 台、llama.cpp は 1 つのビルド。ルーターモードは llama.cpp の新しい機能で、版で振る舞いが変わりうる (対応する版の下限を決めるときに確かめ直す、06 §10)。
- 主記憶の余裕による降ろし方 (`--models-memory-margin`) は試していない。
- API キーを付けたルーターは試していない (子の llama-server の API キーはルーターが扱う、と説明書にある)。
