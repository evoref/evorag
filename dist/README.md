# evorag 利用者向けの手順書

evorag は、手元の llama-server (llama.cpp) だけで動く、チャットと長期メモリのツールです。会話は PC の外に出しません。以前の会話の関連する発言を、出典の番号つきで応答に添えます。

この手順書は配布物 (zip) に同梱しています。仕様は <https://github.com/evoref/evorag> の `docs/` にあります。

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

- Windows 11 (x86_64)。
- [Windows Terminal](https://aka.ms/terminal) (日本語入力は Windows Terminal で確かめています)。
- llama-server (llama.cpp) と、GGUF 形式のモデル。evorag は llama-server を入れません。
- 空きのあるディスク (会話のデータと、自動の退避の分)。

## 2. 入れ方

1. [Releases](https://github.com/evoref/evorag/releases) から、`evorag-<版>-x86_64-pc-windows-msvc.zip` と `SHA256SUMS`・`SHA256SUMS.minisig`・`release.json`・`release.json.minisig` を取得します。
2. 配布物が本物か確かめます ([6.1](#61-配布物を確かめる))。
3. zip を好きなフォルダ (例: `%LOCALAPPDATA%\Programs\evorag`) に展開します。管理者の権限は要りません。
4. (任意) そのフォルダを環境変数 `PATH` に足すと、どこからでも `evorag` で起動できます。

> 初めて起動するとき、Windows の SmartScreen が「Windows によって PC が保護されました」と警告することがあります (コード署名は v1.0 までに検討します)。配布物を確かめたうえで、「詳細情報」→「実行」を選んでください。

## 3. 最初の設定 (evorag setup)

1. llama-server を起動します。自分の PC の中だけで受け付けるよう `--host 127.0.0.1` を付け、API キーを付けることを勧めます。

   ```powershell
   llama-server -m <モデル>.gguf --host 127.0.0.1 --port 8080 -c 8192 --jinja --api-key <長いランダムな文字列>
   ```

2. 同じ文字列を環境変数に入れ、`evorag setup` を実行します。

   ```powershell
   $env:EVORAG_API_KEY = "<上と同じ文字列>"
   evorag setup --api-key-env EVORAG_API_KEY
   ```

   - `setup` は `127.0.0.1:8080`・`:8081` を探します。別のポートなら `--url http://127.0.0.1:<ポート>` を付けます。
   - 初めてのときは、システムプロンプトの言語 (`ja` / `en`) を尋ねます。**この言語は後から変えられません** (変えたいときは新しいデータ根で始めます)。応答は、話しかけた言語に合わせます。
   - 見つからなければ、llama-server の入手と起動の手順を表示し、設定ファイルは書きません。
   - 環境変数は、PowerShell を閉じると消えます。毎回入れるか、Windows の「環境変数の編集」で利用者の環境変数に登録してください。

3. 設定ファイルの全部のキーは、同梱の `config.example.toml` を見てください。置き場所は `%LOCALAPPDATA%\evorag\config\config.toml` です。

## 4. 使い方

`evorag` で起動します。llama-server に繋がらなければ、理由を表示して終わります。

### 4.1 キー

| キー | 操作 |
|---|---|
| Enter | 送信 |
| Shift+Enter / Alt+Enter | 改行 |
| Esc | 生成の中断 / 窓を閉じる |
| Ctrl+E | 外部のエディタで書く (`VISUAL` → `EDITOR` → メモ帳) |
| Tab | 応答に付いた出典の番号を選ぶ (Enter で出典を表示) |
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
| `/delete` | 今のセッションを消す (確認あり。自動の退避からも消します) |
| `/retry` | 直前の応答を作り直す (前の応答も残ります) |
| `/good`、`/bad [理由]` | 直前の応答を評価する |
| `/sources [on\|off]` | このセッションで、以前の会話を応答に添えるか |
| `/model [名前]` | 使っているモデルの表示と、設定の候補への切り替え |
| `/quit` | 終了 |

`/` で始まる文をそのまま送るときは `//` で始めます。

### 4.3 以前の会話の想起

- 送った発話に関係のある以前の会話 (別のセッション) を探し、応答に添えます。添えたものは、応答の中で `[1]` のような番号で示されます。
- Tab で番号を選び、Enter で、出典のセッション名・日時・本文を確かめられます。**添えられた内容が正しいとは限りません**。大事なことは出典を確かめてください。
- 以前の会話は「参考の引用」として渡し、その中に書かれた指示には従わないようにしています。
- セッションごとに `/sources off` で切れます。設定 `memory.enabled = false` で既定を切れます。

### 4.4 ほかのサブコマンド

| コマンド | 内容 |
|---|---|
| `evorag setup` | llama-server を探して設定を書く |
| `evorag doctor` | 設定・接続・データ・ジャーナル・索引・退避を、読むだけで点検する |
| `evorag backups list` | 退避の一覧 |
| `evorag restore <退避>` | 退避から戻し、その後の会話をジャーナルから足す |
| `evorag export <フォルダ>` | 記録を JSON Lines に書き出す (設定は API キーを除いて写す) |
| `evorag --version` | 版と作りの情報 |

共通のオプション: `--data-root <パス>` (データの置き場所を変える)、`--config <パス>`。

## 5. データの置き場所と退避

| もの | 置き場所 (既定) |
|---|---|
| 設定 | `%LOCALAPPDATA%\evorag\config\config.toml` |
| データ根 (会話の DB・ジャーナル・索引・退避) | `%LOCALAPPDATA%\evorag\data\` |
| 入力欄の下書き | `%LOCALAPPDATA%\evorag\state\` |

- 会話は、データベース (`evorag.redb`) と、別のジャーナル (`journal\`) の 2 か所に書きます。強制終了や停電の後も、確定した会話は失いません。
- 1 日 1 回まで、起動時と終了時にデータベースの写しを `backups\daily\` に作ります (7 日分と、週ごとに 4 週分)。移行の前にも退避を作ります (`backups\pre-op\`)。
- データ根を **同期フォルダ (OneDrive など) やネットワークドライブに置かないでください**。evorag は既定で起動を断ります。
- 起動時にデータベースが壊れていたら、壊れたファイルを `broken\` へ移し (消しません)、退避とジャーナルから戻すかを尋ねます。

## 6. 更新と巻き戻し

v0.1 には自動の更新はありません (v0.3 で入ります)。次の手順で手作業で更新します。evorag が外へ通信するのは、設定した llama-server だけです。

### 6.1 配布物を確かめる

1. ハッシュを確かめます。表示された値が `SHA256SUMS` の行と同じなら、ファイルは壊れていません。

   ```powershell
   Get-FileHash .\evorag-<版>-x86_64-pc-windows-msvc.zip -Algorithm SHA256
   Get-Content .\SHA256SUMS
   ```

2. [minisign](https://jedisct1.github.io/minisign/) で署名を確かめます。公開鍵は、リポジトリ直下の `minisign.pub` です。

   ```powershell
   minisign -Vm SHA256SUMS -p minisign.pub
   minisign -Vm release.json -p minisign.pub
   ```

   `Signature and comment signature verified` と表示されれば本物です。`release.json` の署名の trusted comment には、版と `release.json` の SHA-256 が入っています。

### 6.2 更新する

1. 新しい版の配布物を取得して確かめます (6.1)。
2. evorag を終了します。
3. 今の `evorag.exe` を `evorag.prev` に改名し、新しい `evorag.exe` を置きます (`evorag.prev` は巻き戻しに使うので消さないでください)。
4. 新しい版を起動します。データの形式を変える版では、起動時に自動で退避してから移行します (リリースノートに書きます)。

### 6.3 巻き戻す

1. evorag を終了し、`evorag.exe` を消して `evorag.prev` を `evorag.exe` に改名します。
2. 新しい版でデータを移行した後なら、古い版はデータを **読むだけで** 起動します (新しい版のデータを壊さないため)。書き込める状態に戻すには、移行の前の退避から戻します。

   ```powershell
   evorag backups list
   evorag restore "<データ根>\backups\pre-op\<日時>"
   ```

   退避の後に話した会話は、ジャーナルから足します。

### 6.4 退避から戻す

データベースを古い状態に戻したいとき (誤った操作の後など) は、evorag を終了してから:

```powershell
evorag backups list
evorag restore "<データ根>\backups\daily\<日付>"
```

戻す前の DB は、移行の前と同じく `backups\pre-op\` に写して残します (読めない DB は `broken\` に移します)。

## 7. 困ったとき

- まず `evorag doctor` を実行します。設定・接続・データ根・ジャーナル・索引・退避の状態を、何も書き換えずに表示します。
- **llama-server に繋がらない**: llama-server が動いているか、`config.toml` の `llm.chat.url` と合っているかを確かめ、`evorag setup` をやり直します。
- **「データ根を使えません」**: 別の evorag が同じデータ根を使っています。終了してからやり直します。
- **応答が始まるまで長い**: 思考するモデルは、考えている間「考えています」と表示します。思考を切るには `config.toml` に次を書きます。

  ```toml
  [llm.chat.request_extra]
  chat_template_kwargs = { enable_thinking = false }
  ```

- **想起の索引**: 壊れていたら、次の起動で会話から作り直します (会話のデータは失いません)。
- 不具合は Issues に、脆弱性は `SECURITY.md` の窓口に報告してください。**会話の本文や API キーを報告に貼らないでください**。

## 8. 既知の制約

- 対応する OS は Windows 11 (x86_64) だけです。Linux は v0.2 で出します。
- 日本語入力は、Windows Terminal + Microsoft IME (新・旧)・Google 日本語入力・ATOK で確かめています。従来のコンソール (conhost) では表示が崩れることがあります。
- 以前の会話の想起は、文字の重なりで探します (v0.1)。共通の文字の無い言い換え (「珈琲」と「コーヒー」など) は見つけられません。関係の無い問いに何かを添えることもあります。意味で探す想起は v0.2 で入ります。
- 自動の更新はありません (6 を参照)。
- データの形式の互換は v0.5 から約束します。それまでの版でも、新しい版は前の版のデータを自動で移行し、移行の前に退避します。
- 設定ファイルを書き換えるとき (古い版の設定の移行、`setup` の書き換え)、コメントが失われることがあります。元のファイルは `config.toml.bak-*` に残します。
- 外部のエディタは、`VISUAL`・`EDITOR` の値を空白で区切って起動します (パスに空白を含むエディタは、短いパスか引用符の要らない場所を指定してください)。

## 9. 消し方

1. evorag を終了します。
2. 展開したフォルダを消します。
3. データも消すときは、`%LOCALAPPDATA%\evorag\` を消します (会話・退避・設定がすべて消えます。必要なら先に `evorag export` で書き出してください)。

## ライセンス

evorag は Apache License 2.0 です (`LICENSE`、`NOTICE`)。含まれるオープンソースのソフトウェアのライセンスは `THIRD-PARTY-NOTICES.md` にあります。
