# evorag 利用者向けの手順書

evorag は、手元の llama-server (llama.cpp) だけで動く、チャットと長期メモリのツールです。会話は PC の外に出しません。以前の会話の関連する発言を探し、出典の番号つきで応答に添えます。v0.2 からは、埋め込み (意味の近さ) とリランカー (関係の強さの判定) の llama-server を足すと、言い換えにも強くなります。

この手順書は配布物 (zip / tar.gz) に同梱しています。仕様は <https://github.com/evoref/evorag> の `docs/` にあります。

## 目次

1. [必要なもの](#1-必要なもの)
2. [入れ方](#2-入れ方)
3. [最初の設定 (evorag setup)](#3-最初の設定-evorag-setup)
4. [使い方](#4-使い方)
5. [データの置き場所と退避](#5-データの置き場所と退避)
6. [更新と巻き戻し](#6-更新と巻き戻し)
7. [困ったとき](#7-困ったとき)
8. [既知の制約](#8-既知の制約)
9. [消し方](#9-消し方)

## 1. 必要なもの

- Windows 11 (x86_64) か、Linux (x86_64)。
- 端末: Windows は [Windows Terminal](https://aka.ms/terminal)、Linux は GNOME Terminal (tmux の中でも使えます)。日本語入力は、Windows は Microsoft IME (新・旧)・Google 日本語入力・ATOK、Linux は fcitx5・ibus で確かめています。
- llama-server (llama.cpp) と、GGUF 形式のモデル。evorag は llama-server を入れません。
- 空きのあるディスク (会話のデータ、索引、自動の退避の分)。

### 勧める構成 (llama-server を 3 つ)

| 役割 | 例 | 無いとき |
|---|---|---|
| 応答 (chat) | 会話に使うモデル (Qwen3.5-9B など) | 使えません (必須) |
| 埋め込み (embed) | bge-m3 (Q8_0、約 0.6 GB) | 以前の会話を文字の重なりだけで探します |
| リランカー (rerank) | bge-reranker-v2-m3 (Q8_0、約 0.6 GB) | v0.1 と同じ規則で添えるかを決めます |

- GPU のメモリの目安: 応答のモデルの大きさと文脈の分に、埋め込みとリランカーで合わせて 2 GB ほどを足します。足りないときは、埋め込みとリランカーを CPU で動かしても使えます (応答が始まるまでが少し長くなります)。
- 埋め込みとリランカーは無くても動きます。足すと、言い換え (「珈琲」と「コーヒー」など) にも届き、関係の無い問いに添えることが減ります。

## 2. 入れ方

**Windows**

1. [Releases](https://github.com/evoref/evorag/releases) から、`evorag-<版>-x86_64-pc-windows-msvc.zip` と `SHA256SUMS`・`SHA256SUMS.minisig`・`release.json`・`release.json.minisig` を取得します。
2. 配布物が本物か確かめます ([6.1](#61-配布物を確かめる))。
3. zip を好きなフォルダ (例: `%LOCALAPPDATA%\Programs\evorag`) に展開します。管理者の権限は要りません。
4. (任意) そのフォルダを環境変数 `PATH` に足すと、どこからでも `evorag` で起動できます。

> 初めて起動するとき、Windows の SmartScreen が「Windows によって PC が保護されました」と警告することがあります (コード署名は v1.0 までに検討します)。配布物を確かめたうえで、「詳細情報」→「実行」を選んでください。

**Linux**

1. `evorag-<版>-x86_64-unknown-linux-musl.tar.gz` と、上と同じ `SHA256SUMS` などを取得し、確かめます ([6.1](#61-配布物を確かめる))。
2. 展開して、実行ファイルを `PATH` の通った場所に置きます。静的にリンクしているので、ほかのライブラリは要りません。

   ```sh
   tar -xzf evorag-<版>-x86_64-unknown-linux-musl.tar.gz
   install -m 755 evorag-<版>-x86_64-unknown-linux-musl/evorag ~/.local/bin/evorag
   ```

## 3. 最初の設定 (evorag setup)

1. llama-server を起動します。自分の PC の中だけで受け付けるよう `--host 127.0.0.1` を付け、API キーを付けることを勧めます。3 つの役割を別のポートで動かす例 (PowerShell。Linux でも同じ引数です):

   ```powershell
   # 応答
   llama-server -m <応答のモデル>.gguf --host 127.0.0.1 --port 8080 -c 8192 -np 2 --jinja --api-key <長いランダムな文字列>
   # 埋め込み
   llama-server -m bge-m3-q8_0.gguf --host 127.0.0.1 --port 8081 --embedding -c 8192 -b 4096 -ub 4096 --api-key <同じ文字列>
   # リランカー
   llama-server -m bge-reranker-v2-m3-Q8_0.gguf --host 127.0.0.1 --port 8082 --reranking -c 8192 -b 4096 -ub 4096 --api-key <同じ文字列>
   ```

   - 応答の llama-server の `-np 2` (スロット 2 つ) は、裏の処理 (ノートの検索の鍵を作る) が応答のじゃまをしないようにするためです。1 つでも動きます。
   - 1 つの llama-server で 3 つの役割を動かすルーターモード (プリセット) も使えます。`evorag setup` が見つけると、プリセットの例を示します。

2. 同じ文字列を環境変数に入れ、`evorag setup` を実行します。

   ```powershell
   $env:EVORAG_API_KEY = "<上と同じ文字列>"
   evorag setup --api-key-env EVORAG_API_KEY
   ```

   - `setup` は `127.0.0.1` の 8080・8081・8082 などを探し、API で役割 (応答・埋め込み・リランカー) を見分けて設定に書きます。別のポートなら `--url http://127.0.0.1:<ポート>` を付けます。見つからない役割には起動の例を示します。
   - 使うモデルをデータ根に記録し、性質を点検します (速さ、思考を切れるか、型縛りの出力が使えるか)。リランカーは、同梱の事例で添えるかを決める値 (しきい値) を求めます (1 分ほど)。
   - 初めてのときは、システムプロンプトの言語 (`ja` / `en`) を尋ねます。**この言語は後から変えられません** (変えたいときは新しいデータ根で始めます)。応答は、話しかけた言語に合わせます。
   - 環境変数は、端末を閉じると消えます。毎回入れるか、利用者の環境変数に登録してください。

3. 設定ファイルの全部のキーは、同梱の `config.example.toml` を見てください。

## 4. 使い方

`evorag` で起動します。応答の llama-server に繋がらなければ、理由を表示して終わります。埋め込みやリランカーに繋がらないときは、それらを使わずに動き、30 秒ごとに繋ぎ直します。

### 4.1 キー

| キー | 操作 |
|---|---|
| Enter | 送信 |
| Shift+Enter / Alt+Enter | 改行 |
| Esc | 生成の中断 / 窓を閉じる |
| Ctrl+E | 外部のエディタで書く (`VISUAL` → `EDITOR` → Windows はメモ帳) |
| Tab | 応答に付いた出典の番号と「関係しそうな記録」を選ぶ (Enter で詳細) |
| PageUp / PageDown | 会話のスクロール |
| Ctrl+C | 生成中は中断。入力欄が空なら、続けて 2 回で終了 |
| Ctrl+Q | 終了 |

### 4.2 コマンド

| コマンド | 内容 |
|---|---|
| `/help` | コマンドの一覧 |
| `/new [題名]` | セッションを作る |
| `/sessions` | セッションの一覧と再開 |
| `/rename <題名>` | 今のセッションを改名する |
| `/delete` | 今のセッションを消す (確認あり。索引と自動の退避からも消します) |
| `/retry` | 直前の応答を作り直す (前の応答も残ります) |
| `/good`、`/bad [理由]` | 直前の応答を評価する |
| `/sources [on\|off]` | このセッションで、以前の会話を応答に添えるか |
| `/memory` | 直前の応答に添えた以前の会話 (出典の番号)、関係しそうな記録、リランクの値としきい値との差、縮退の理由と次にすること、このセッションの出典 |
| `/style [項目 値]` | 答え方の既定 (言語・長さ・書式)。値なしで選ぶ窓を開く (すべてのセッションに効く) |
| `/model` | 記録したモデルと、繋がっているモデルの表示 (切り替えは `evorag setup` で行う) |
| `/quit` | 終了 |

`/` で始まる文をそのまま送るときは `//` で始めます。

### 4.3 以前の会話の想起

- 送った発話に関係のある以前の会話 (別のセッション) を探し、いちばん関係の強い往復 (発話と応答の組) を 1 つだけ応答に添えます。添えたものは、応答の中で `[1]` のような番号で示されます。
- 探し方: 文字の重なり (語彙) と意味の近さ (埋め込み) の両方で候補を集め、リランカーで関係の強さを測って、しきい値以上のものだけを添えます。関係の無い問いには何も添えません。
- しきい値の少し下の往復は、応答の下に「関係しそうな記録 (この応答には使っていません)」として薄く出します。Tab で選んで Enter で詳細を開き、`a` で添えて作り直し、`x` で「無関係」の印を付けられます (矢印と Enter でも選べます)。
- 会話の話題ごとに、検索の鍵 (ノート) を裏の処理で作ります。鍵は探すためだけに使い、添えるのは元の会話です。
- **添えられた内容が正しいとは限りません**。大事なことは出典を確かめてください (Tab → Enter)。以前の会話は「参考の引用」として渡し、その中に書かれた指示には従わないようにしています。
- 状態の行の「想起:」の欄: `標準` (埋め込みとリランカーの両方)、`簡易` (縮退して文字の重なりなどで動く)、`候補だけ` (しきい値が無く、自動では添えない)、`作り直し中 N%`。理由と次にすることは `/memory` と `/model` で示します。
- セッションごとに `/sources off` で切れます。設定 `memory.enabled = false` で既定を切れます。

### 4.4 裏の処理と、先に済ませる処理

- 操作の無い間 (既定 60 秒、`memory.idle_seconds`) に、以前の会話の埋め込みとノートの検索の鍵を作り、日をまたいで動いているときは自動の退避を作ります。キーを押すと 1 秒以内に止め、次の機会に続きから進めます。状態の行に「整理中」と出ます。
- 応答を書いた直後に、次の発話のための文脈を llama-server に先に処理させます (応答が始まるまでが短くなります)。
- 入力欄の文字が 0.5 秒ほど変わらないと、その内容で以前の会話を先に探します。**送っていない入力も、設定した llama-server に出ます** (外へは出ません)。切るときは設定 `retrieval.precompute = false` を書きます。`llm.allow_remote = true` のときは、書かなければ切っています。
- 電池で動いている間は `memory.on_battery` に従います (`reduce`: 入力が止まったときの処理を止め、裏の処理は 5 倍待つ / `normal` / `stop`: どちらも止める)。`memory.sleep_time = false` で裏の処理を止められます。止めても会話の保存と想起は動きます。

### 4.5 ほかのサブコマンド

| コマンド | 内容 |
|---|---|
| `evorag setup` | llama-server を探して設定を書き、モデルを記録して点検する。モデルの切り替えもここで行う |
| `evorag doctor` | 設定・接続・モデルの記録と点検の結果・データ・ジャーナル・索引・退避を、読むだけで点検する |
| `evorag backups list` | 退避の一覧 |
| `evorag restore <退避>` | 退避から戻し、その後の会話をジャーナルから足す |
| `evorag rebuild [--index]` | 派生データ (チャンク・ノート・埋め込み・索引・点検の結果) を会話から作り直す。実行の前に退避する。`--index` は索引だけ |
| `evorag export <フォルダ>` | 記録を JSON Lines に書き出す (設定は API キーを除いて写す) |
| `evorag update check` / `apply [版]` / `rollback` | 新しい版の確認・更新・巻き戻し ([6](#6-更新と巻き戻し)) |
| `evorag --version` | 版と作りの情報 |

共通のオプション: `--data-root <パス>` (データの置き場所を変える)、`--config <パス>`。

## 5. データの置き場所と退避

| もの | Windows (既定) | Linux (既定) |
|---|---|---|
| 設定 | `%LOCALAPPDATA%\evorag\config\config.toml` | `~/.config/evorag/config.toml` |
| データ根 (会話の DB・ジャーナル・索引・退避) | `%LOCALAPPDATA%\evorag\data\` | `~/.local/share/evorag/` |
| 入力欄の下書き・ログ (7 日分。会話の本文・API キーは書かない)・更新の記録 (`update/`) | `%LOCALAPPDATA%\evorag\state\` | `~/.local/state/evorag/` |

Linux は `XDG_CONFIG_HOME`・`XDG_DATA_HOME`・`XDG_STATE_HOME` があればそれを使います。

- 会話は、データベース (`evorag.redb`) と、別のジャーナル (`journal/`) の 2 か所に書きます。強制終了や停電の後も、確定した会話は失いません。チャンク・ノート・埋め込み・索引は会話から作り直せるので、ジャーナルには書きません。
- 1 日 1 回まで、データベースの写しを `backups/daily/` に作ります (7 日分と、週ごとに 4 週分)。移行・作り直し・復元の前にも退避を作ります (`backups/pre-op/`)。
- データ根を **同期フォルダ (OneDrive など) やネットワークドライブに置かないでください**。evorag は既定で起動を断ります。
- 起動時にデータベースが壊れていたら、壊れたファイルを `broken/` へ移し (消しません)、退避とジャーナルから戻すかを尋ねます。

## 6. 更新と巻き戻し

v0.3 から、`evorag update` で新しい版を確かめ、更新し、前の版に戻せます。evorag が外へ通信するのは、設定した llama-server と、`evorag update check`・`evorag update apply` を打ったときの GitHub だけです。起動のときや定期の自動の確認はしません (`evorag update rollback` は通信しません)。

| コマンド | 内容 |
|---|---|
| `evorag update check` | 新しい版があれば、その版・リリースの日付・変更の要点を示す。何も書き換えない |
| `evorag update apply [版]` | 配布物を取得し、署名を確かめてから実行ファイルを置き換える。今の実行ファイルは `evorag.prev` に残す。版を省くと、いちばん新しい版にする |
| `evorag update rollback` | `evorag.prev` に戻す。戻した版でデータを使えるかを示す |

`evorag update` を使えないとき (CPU の機能や証明書のルートが無い、通信できない、`release.json` の形式を読めない、など) は、[6.6](#66-手作業で更新する) の手作業の手順で更新します。

### 6.1 配布物を確かめる

手作業で入れるとき・更新するときは、取得した配布物が本物かを確かめます (`evorag update` は同じことを自動で行います)。

1. ハッシュを確かめます。表示された値が `SHA256SUMS` の行と同じなら、ファイルは壊れていません。

   ```powershell
   Get-FileHash .\evorag-<版>-x86_64-pc-windows-msvc.zip -Algorithm SHA256
   Get-Content .\SHA256SUMS
   ```

   Linux では `sha256sum -c --ignore-missing SHA256SUMS` を使えます。

2. [minisign](https://jedisct1.github.io/minisign/) で署名を確かめます。公開鍵は、リポジトリ直下の `minisign.pub` です。1 行目の末尾の鍵 ID が `A3B00C77E1EA828E` であることを確かめてください。

   ```powershell
   minisign -Vm SHA256SUMS -p minisign.pub
   minisign -Vm release.json -p minisign.pub
   ```

   `Signature and comment signature verified` と表示されれば本物です。`release.json` の署名の trusted comment には、版と `release.json` の SHA-256 が入っています。

### 6.2 evorag update で更新する

1. `evorag update check` で、新しい版があるかを確かめます。版・リリースの日付・変更の要点を示します (何も書き換えません)。
2. **evorag を閉じます** (データ根を使っている evorag があると、`apply` は終了コード 2 で断ります)。
3. `evorag update apply` を実行します。版を指定するときは `evorag update apply 0.4.0` のように書きます。
   - 署名つきの `release.json` を取得し、本体に埋め込んだ鍵で署名を確かめてから、配布物を取得し、大きさと SHA-256 を確かめます。合わなければ何も変えずに断ります (改ざんを疑うときは終了コード 5)。
   - 新しい実行ファイルを置いた後に、`--version` で動くことを確かめます。動かなければ元に戻します。
   - 今の実行ファイルは `evorag.prev` に残します (巻き戻しに使うので消さないでください)。
4. 新しい版を起動します。データの形式を変える版では、最初の起動で自動で退避してから移行します。移行に失敗したときは、元のデータのまま読むだけで起動し、`evorag update rollback` を案内します。

- 今と同じか古い版は `apply` で入れられません。前の版に戻すのは `rollback` だけです。
- 今の版から直接は更新できない版 (`upgrade_from`) は、先に入れる版を案内します (`evorag update apply <その版>` を先に実行します)。
- インストール先には、自分だけが書き込めるフォルダ (Windows は `%LOCALAPPDATA%\Programs\evorag` など) を使ってください。書き込めないフォルダ (Program Files など)、ほかの利用者も書き込めるフォルダ、実行ファイルの名前を `evorag(.exe)` から変えたときは、何も変えずに断ります。
- **cargo で入れた人** (`evorag --version` の `installed via: cargo`): `apply` と `rollback` は使えません。`evorag update check` で新しい版を確かめると、入れ直すコマンド (`cargo install --locked --git https://github.com/evoref/evorag --tag v<版> evorag`) を示します。先に入れる版があるときは、そのコマンドを先に示します。

| 終了コード | 意味 |
|---|---|
| 0 | 済んだ (新しい版が無いときも 0) |
| 1 | 断った・失敗した (通信できない、回数の制限、使えない版、インストール先の条件、置き換えの後の確かめに通らず元に戻した、など) |
| 2 | evorag が動いている (データ根を使っている) |
| 4 | 設定 `update.trusted_keys` の誤り |
| 5 | 改ざんを疑うので断った (署名が通らない、版やタグが合わない、配布物の大きさ・SHA-256 が合わない、など) |

### 6.3 巻き戻す (evorag update rollback)

1. evorag を閉じます。
2. `evorag update rollback` を実行します。`evorag.prev` を `evorag(.exe)` に戻し、今の版を `evorag.prev` に残します (もう一度 `rollback` すると、また入れ替わります)。
3. 戻した版でデータを使えるかを示します。
   - 新しい版でデータを移行した後なら、古い版はデータを **読むだけで** 起動するか、開けないことがあります (新しい版のデータを壊さないため)。そのときは、移行の前の退避 (`backups/pre-op/`) のパスと、戻すコマンド (`evorag restore <退避>`。戻した版で実行します) を示します。退避の後に話した会話は、ジャーナルから足します。
   - 戻さなければ、データは新しい版でなら書けます (`evorag update apply` で新しい版に戻れます)。
   - 設定ファイルを移行したときの写し (`config.toml.bak-v<旧版>`) があれば、そのパスも示します。

- v0.2 で使ったデータ根を v0.1.2 で開いても、セッションは開けます。ただし **v0.1.2 で消したセッションの、v0.2 が作ったもの (チャンク・ノート・埋め込み・索引) と、それを含む退避は、v0.2 で開き直すまで残ります** (v0.1.2 はそれらを知らないため)。v0.2 で開き直すと、DB・索引・退避から消します。

### 6.4 途中で止まったとき

置き換えの途中で強制終了や電源断があっても、次の `evorag update apply`・`rollback` か、次の evorag の起動が、状態のファイル (状態のフォルダの `update/state.json`) に従って続きを済ませるか、元に戻します。

**Windows の隙間**: Windows では、動いている `evorag.exe` を改名してから新しいファイルを置くので、その一瞬だけ `evorag.exe` が無くなります。そこで止まって `evorag.exe` が無いときは、次のように戻してから起動します (`apply`・`rollback` も始める前に同じことを示します)。

- `apply` の途中: `evorag.new` があればそれを、無ければ `evorag.prev` を `evorag.exe` に改名します。
- `rollback` の途中: `evorag.prev` を `evorag.exe` に改名します。

どちらの戻し方でも、次の `evorag update` の操作か evorag の起動が、残りを整えます。

**手で戻す手順** (状態のファイルが読めない、置くファイルが見分けられない、と表示されたとき):

1. evorag を終了します。
2. インストール先の `evorag(.exe)`・`evorag.new`・`evorag.tmp`・`evorag.prev`・`evorag.prev.old`・`evorag.rejected` を、それぞれ `--version` を付けて動かし、版を確かめます (Windows で拡張子の無いファイルは、`evorag.exe` という名前の写しを別のフォルダに作って動かします)。
3. 使いたい版のファイルを `evorag(.exe)` に、前の版のファイルを `evorag.prev` に改名し、残りを消します。
4. 状態のフォルダの `update/state.json` を消します (Windows は `%LOCALAPPDATA%\evorag\state\update\state.json`、Linux は `~/.local/state/evorag/update/state.json`)。巻き戻しの記録が無くなるので、次の `rollback` は一般の案内になります。

### 6.5 通信とプロキシ

- 接続先は `api.github.com` (リリースの一覧) と `github.com` (`release.json`・署名・配布物) だけで、配布物の取得は `*.githubusercontent.com` へ転送されることがあります。ほかへの転送は辿りません。
- 送るのは evorag の版と OS の種類 (User-Agent)、取得する配布物の名前だけです。Cookie や認証の情報は送りません。
- 本物かどうかは TLS ではなく minisign の署名で確かめます。通信の途中で書き換えられても、署名の合わない配布物は入れません。
- **プロキシ**: 環境変数 `HTTPS_PROXY`・`ALL_PROXY`・`HTTP_PROXY` をこの順に読み (それぞれ大文字の名前を先に、小文字の `https_proxy`・`all_proxy`・`http_proxy` を後に)、空でない最初の値のプロキシを、HTTP の CONNECT で使います。
  - 使えるのは `http://` と `https://` で始まるプロキシだけです。最初の値が `socks5://` などの SOCKS、ほかのスキーム、スキームの無い `proxy:8080`、読めない値のときは、直接には繋がずに、どの環境変数かを示して通信の前に止めます (終了コード 1)。その環境変数を直してください。
  - `NO_PROXY` (無いか空なら `no_proxy`) に書いた宛先は、プロキシを通しません。カンマ区切りで、`github.com`・`.github.com`・`*.github.com` はどれも `github.com` とその下のホスト (`api.github.com` など) に当たります。`*` はすべての宛先です。
  - Windows の「プロキシの設定」(WinINET) と自動構成のスクリプト (PAC) は読みません。

  ```powershell
  $env:HTTPS_PROXY = "http://proxy.example.com:8080"
  evorag update check
  ```

- 認証の要るプロキシは、URL に書いた Basic 認証 (`http://user:pass@proxy:8080`) だけに対応します。**NTLM などの認証には対応しません**。そのときは、手作業で更新します (6.6)。
- GitHub の API の匿名の呼び出しには回数の制限 (1 時間に 60 回) があります。当たったときは、制限が解ける時刻を示して終わります。
- Linux では、OS の証明書 (`ca-certificates` など) が要ります。見つからないときは通信せずに止めます。

### 6.6 手作業で更新する

`evorag update` を使えないとき (通信できない環境、認証の要るプロキシ、CPU の機能や証明書のルートが無い、など) の手順です。

1. 別の PC などで [Releases](https://github.com/evoref/evorag/releases) から新しい版の配布物と `SHA256SUMS`・`SHA256SUMS.minisig`・`release.json`・`release.json.minisig` を取得し、確かめます (6.1)。
2. evorag を終了します。
3. 今の実行ファイル (`evorag.exe` / `evorag`) を `evorag.prev` に改名し、新しいものを置きます (`evorag.prev` は巻き戻しに使うので消さないでください)。
4. 新しい版を起動します。データの形式を変える版では、起動時に自動で退避してから移行します (リリースノートに書きます)。

手作業で巻き戻すときは、evorag を終了し、今の実行ファイルを消して `evorag.prev` を元の名前に戻します。データを移行した後なら、6.3 と同じく、移行の前の退避から戻します。

```powershell
evorag backups list
evorag restore "<データ根>\backups\pre-op\<日時>"
```

### 6.7 v0.1 から更新する人へ

- **応答のモデルを記録します**: 最初の起動で、繋がっている応答のモデルをデータ根に記録します。以後、記録と違うモデルの llama-server で起動すると、画面を開かずに違いと取れる手 (記録したモデルで起動し直す、`evorag setup` で切り替える、別のデータ根で試す) を示して終わります (終了コード 6)。
- **`/model` は表示だけになりました**: モデルの切り替えは `evorag setup` で行います。設定の `[[llm.chat.models]]` は読んで無視し、一度だけ知らせます (v0.1 に戻す予定がなければ消してかまいません)。
- **埋め込みとリランカー**: `evorag setup` をもう一度実行すると、8081・8082 の llama-server を見つけて設定に書きます。v0.1 の会話の埋め込みは、更新の後に裏の処理で作ります (会話の量によっては時間がかかります。その間は状態の行に「想起: 作り直し中 N%」と出て、作ってある分と文字の重なりで探します)。
- **送る前の入力**: 入力が止まったときに先に探す処理で、送っていない入力が手元の llama-server に出ます (4.4)。切るときは `retrieval.precompute = false`。
- **索引を作り直します**: v0.1 の語彙の索引は使わず、最初の起動で作り直します (会話のデータは変わりません)。
- v0.1・v0.2 には `evorag update` がありません。v0.3 へは手作業で更新します (6.6)。

### 6.8 退避から戻す

データベースを古い状態に戻したいとき (誤った操作の後など) は、evorag を終了してから:

```powershell
evorag backups list
evorag restore "<データ根>\backups\daily\<日付>"
```

戻す前の DB は、移行の前と同じく `backups/pre-op/` に写して残します (読めない DB は `broken/` に移します)。

## 7. 困ったとき

- まず `evorag doctor` を実行します。設定・接続・モデルの記録と点検の結果・データ根・ジャーナル・索引・退避の状態を、何も書き換えずに表示します。
- **llama-server に繋がらない**: llama-server が動いているか、`config.toml` の `llm.chat.url` と合っているかを確かめ、`evorag setup` をやり直します。
- **「記録と違うモデル」で起動しない (終了コード 6)**: 記録したモデルで llama-server を起動し直すか、`evorag setup` で切り替えます (埋め込みのモデルを変えると全件を作り直し、リランカーを変えるとしきい値を求め直します)。
- **想起: 簡易 / 候補だけ**: `/memory` と `/model` で理由を見て、`evorag setup` で埋め込みとリランカーを設定します。リランカーの較正は、繋いだ後に裏で行います (1 分ほど)。
- **「データ根を使えません」**: 別の evorag が同じデータ根を使っています。終了してからやり直します。
- **`evorag update` が通信できない**: プロキシを使う環境なら `HTTPS_PROXY` を設定します ([6.5](#65-通信とプロキシ))。それでも通信できないときは、手作業で更新します ([6.6](#66-手作業で更新する))。
- **更新の途中で止まって `evorag.exe` が無い**: [6.4](#64-途中で止まったとき) の戻し方で戻します。
- **応答が始まるまで長い**: 思考するモデルは、考えている間「考えています」と表示します。思考を切るには `config.toml` に次を書きます (補助の呼び出しには `[llm.chat.aux_request_extra]` に同じものを書きます)。

  ```toml
  [llm.chat.request_extra]
  chat_template_kwargs = { enable_thinking = false }
  ```

- **想起の索引**: 壊れていたら、次の起動で会話から作り直します (会話のデータは失いません)。`evorag rebuild --index` で作り直すこともできます。
- 不具合は Issues に、脆弱性は `SECURITY.md` の窓口に報告してください。**会話の本文や API キーを報告に貼らないでください**。

## 8. 既知の制約

- 対応する OS は Windows 11 (x86_64) と Linux (x86_64) です。macOS は実機を確保したときに出します。
- 日本語入力は 1 に書いた組み合わせで確かめています。Windows の従来のコンソール (conhost) では表示が崩れることがあります。
- 文字をまったく共有しない言い換えは、意味の近さで候補には入りますが、リランカーの値がしきい値に届かず、添えないことが多いです (「関係しそうな記録」に出ることもあります)。
- 自動の更新はありません。更新は `evorag update` を打ったときだけで、認証の要るプロキシ (NTLM など) には対応しません (6 を参照)。
- データの形式の互換は v0.5 から約束します。それまでの版でも、新しい版は前の版のデータを自動で移行し、移行の前に退避します。
- 設定ファイルを書き換えるとき (古い版の設定の移行、`setup` の書き換え)、コメントが失われることがあります。元のファイルは `config.toml.bak-*` に残します。
- 外部のエディタは、`VISUAL`・`EDITOR` の値を空白で区切って起動します (パスに空白を含むエディタは、短いパスか引用符の要らない場所を指定してください)。

## 9. 消し方

1. evorag を終了します。
2. 展開したフォルダ (Linux は置いた実行ファイル) を消します。
3. データも消すときは、Windows は `%LOCALAPPDATA%\evorag\`、Linux は `~/.config/evorag/`・`~/.local/share/evorag/`・`~/.local/state/evorag/` を消します (会話・退避・設定がすべて消えます。必要なら先に `evorag export` で書き出してください)。

## ライセンス

evorag は Apache License 2.0 です (`LICENSE`、`NOTICE`)。含まれるオープンソースのソフトウェアのライセンスは `THIRD-PARTY-NOTICES.md` にあります。
