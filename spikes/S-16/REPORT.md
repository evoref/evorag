# S-16 端末の貼り付けと IME の確定の見分け (技術検証)

| 項目 | 内容 |
|---|---|
| 検証 | 08 §3 の S-16 (S-03 の続き) |
| 関わる仕様 | 04 §5.2 (前のふるい)、07 §2.3、10 の OQ-20 |
| 実施 | 2026-11-16 (Windows Terminal・conhost。Linux の端末と tmux は未実施) |
| 再現 | `spikes/S-16/` (独立の cargo プロジェクト。コードは `spike/S-16` ブランチにだけある。依存は crossterm 0.29.0 と、Windows だけ windows-sys 0.59.0)。貼り付けに使う文は `clips.txt` (架空) |
| 判定 | **Windows の分は判定した** (2026-11-16)。今の evorag-tui (crossterm 0.29) の読み方では、Windows Terminal・conhost とも貼り付けと IME の確定を見分けられない。Windows Terminal は、入力を VT の列として読めば見分けられる (§3.3)。Linux の端末 (ibus・fcitx5) と tmux、Google 日本語入力は、利用者の指示で省いた (未実施) |

## 1. これまでに分かったこと

### 1.1 crossterm 0.29 のソースから

- **Windows では `Event::Paste` は届かない。** `Event::Paste` を作るのは Unix の入力の解析 (`event/sys/unix/parse.rs` の `ESC[200~` … `ESC[201~`) だけで、Windows の側 (`ReadConsoleInputW` の記録を読む `event/sys/windows/parse.rs`) には無い。evorag-tui の `TermEvent::Paste` の分岐 (`crates/evorag-tui/src/run.rs`) は、Windows では通らない。S-03 の「IME の確定の文字列は `Char` の連続か `Paste` で来る」の `Paste` は、Windows については当たらない。
- Windows でも `EnableBracketedPaste` は、VT の出力が使える端末 (Windows Terminal) では `ESC[?2004h` を書き出す。そのとき端末が貼り付けを `ESC [ 2 0 0 ~` のキーの記録で包んで送るなら、crossterm は `Esc` と `Char('[')`・`Char('2')` … に分けて返す。この場合、今の evorag-tui では貼り付けの前後に `Esc` と `[200~` が届くことになるので、§3 で確かめる (試行の表の `ESC[200~` の列)。
- Windows の記録の仮想キー (`wVirtualKeyCode`) とスキャンコードは、crossterm の `KeyEvent` には残らない。見分けに使えるかは `raw` で調べる (IME の確定・貼り付けのどちらかだけが `VK_PACKET` (0xE7) やスキャンコード 0 になるか)。

### 1.2 tmux での模擬 (WSL の Ubuntu、tmux の `paste-buffer`)

人の操作の代わりに tmux の `send-keys`・`paste-buffer` で入力を送り、ツールが記録できることを確かめた。

| 送り方 | `events` | `raw` |
|---|---|---|
| `send-keys -l abc` (打鍵の代わり) | `Key` が 3 件、3 つの塊ではなく 1 つの塊 | バイト列 1 件 |
| `paste-buffer -p` (括弧付きの貼り付け) | `Event::Paste` が 1 件 (15 文字) | `ESC[200~` を含む |
| `paste-buffer` (括弧なし) | `Key` が 9 件、1 つの塊 | バイト列 1 件、`ESC[200~` なし |

- 括弧の印が無い貼り付けは、IME の確定の文字列や速い打鍵と、届き方 (塊の大きさ・間隔) では見分けられない。見分けに使えるのは印 (`Event::Paste`・`ESC[200~`、Windows では記録の特徴) だけとする (§4)。
- これは模擬で、外側の端末が貼り付けを括弧付きで tmux に送るか、IME の確定を括弧なしで送るかは、§3 の実機で確かめる。

## 2. 手順 (人が行う)

### 2.1 ビルド

- Windows: `spikes\S-16` で `cargo build --release`。実行ファイルは `target\release\s16-paste.exe`。
- Linux: `spikes/S-16` で `cargo build --release`。実行ファイルは `target/release/s16-paste`。

### 2.2 実行

組み合わせ (§3 の表の行) ごとに、次を順に起動する。どれも 9 つの試行を順に出す。

| 起動 | 何を記録するか |
|---|---|
| `s16-paste` | evorag の TUI と同じ読み方 (crossterm の raw mode と bracketed paste) で見えるイベント |
| `s16-paste raw` | crossterm を通さない入力。Windows は `ReadConsoleInputW` の記録、Linux は標準入力のバイト列 |
| `s16-paste raw --vt` | (Windows だけ) ENABLE_VIRTUAL_TERMINAL_INPUT を立てたときの記録 |

試行は次の順。終えたら Ctrl+N で次へ進む (途中で Ctrl+Q で終える)。貼り付けは、その端末の普通の方法 (Windows Terminal は Ctrl+V か右クリック、GNOME の端末は Ctrl+Shift+V) で行い、方法を §3 の備考に書く。

| ID | 操作 |
|---|---|
| `typing` | IME を切り、半角英字を打つ |
| `ime-short` | IME で「にほん」と打ち、変換して確定する |
| `ime-long` | IME で「きょうはとうきょうにいきます」と打ち、変換して確定する |
| `ime-kana` | IME で「てすと」と打ち、変換せずに Enter で確定する |
| `ime-predict` | 予測変換の候補を選んで確定する |
| `paste-ascii` | `clips.txt` の英字 1 行を貼り付ける |
| `paste-ja` | `clips.txt` の日本語 1 行を貼り付ける |
| `paste-multi` | `clips.txt` の 2 行を貼り付ける |
| `paste-short` | `clips.txt` の 1 文字を貼り付ける |

- 終えると、今のフォルダに `s16-<起動>-<秒>.jsonl` (すべての記録) と `.summary.md` (試行ごとの集計) ができる。入力した文が残るので、試行の文 (`clips.txt` と上の表) 以外を入力しない。記録は取り込まず、集計だけを §3 に写す。
- 環境の変数 (`TERM`・`VTE_VERSION`・`GTK_IM_MODULE`・`XMODIFIERS`・tmux の有無など) は集計の頭に出る。

## 3. 結果 (人が埋める)

各行で、`.summary.md` から次を写す。

- 貼り付けの印: 貼り付けの 4 試行のうち、印 (`Paste` が 1 以上か、`ESC[200~` が yes。Windows の `raw` は、IME と違う記録の特徴があればそれ) があった試行の数。
- IME の印: IME の 4 試行 (`ime-*`) のうち、同じ印があった試行の数。

| 端末 | IME | 経由 | 起動 | 貼り付けの印 (/4) | IME の印 (/4) | 見分け | 備考 |
|---|---|---|---|---|---|---|---|
| Windows Terminal | Microsoft IME | — | events | 0 | 0 | **否** | 2026-11-16。貼り付けは Ctrl+V。§3.1 |
| Windows Terminal | Microsoft IME | — | raw | 0 | 0 | **否** | 2026-11-16。§3.3 |
| Windows Terminal | Microsoft IME | — | raw --vt | 4 | 0 | **可** | 2026-11-16。§3.3 |
| Windows Terminal | Google 日本語入力 | — | raw | | | | |
| conhost | Microsoft IME | — | events | 0 | 0 | **否** | 2026-11-16 (3 回目)。§3.4 |
| conhost | Microsoft IME | — | raw | 0 | 0 | **否** | 2026-11-16 (3 回目)。§3.4 |
| conhost | Microsoft IME | — | raw --vt | — | — | 未 | 3 回とも試行の前に終わった。§3.4 |
| GNOME の端末 | ibus (mozc) | — | events | | | | |
| GNOME の端末 | fcitx5 (mozc) | — | events | | | | |
| GNOME の端末 | fcitx5 (mozc) | tmux | events | | | | |
| Windows Terminal | Microsoft IME | WSL の tmux | events | | | | |

### 3.1 Windows Terminal・events (2026-11-16)

- 貼り付けも IME の確定も、1 文字ずつの `Char` の `Press`・`Release` の組で届いた。`Event::Paste` は 0 件、`ESC[200~` も無い (Windows Terminal は、crossterm が書き出す `ESC[?2004h` があっても、キーの記録を印で包まない)。§1.1 の「貼り付けの前後に `Esc` と `[200~` が入る」不具合は起きない。
- 修飾・`KeyEventState` は、貼り付けと IME の確定で同じ (大文字だけ `SHIFT`)。違いは、貼り付けの後に Ctrl+V の `v` の `Release` が来ること (右クリックの貼り付けでは来ないので、見分けには使えない) と、別の窓からコピーしたときのフォーカスの出入りだけ。
- **貼り付けた複数行の改行は、`Enter` の `Press` で届く。** 今の evorag-tui は `Enter` を送信として扱うので、Windows Terminal で複数行を貼ると、1 行目で送ってしまう (送信の後の行は次の入力になる)。S-16 の範囲の外なので、別の `fix/` で扱う。
- 長い文の IME の確定 (`ime-long`) は、18 文字が 1 つの塊 (間が 15 ms 以内) で届き、貼り付け (`paste-ja` の 9 文字) と届き方で区別できない (§1.2 と同じ)。

### 3.2 raw で記録が取れなかった件 (2026-11-16)

- Windows Terminal の `raw` と `raw --vt` の 1 回目は、試行の記録が 0 件だった (Ctrl の記録だけで終了)。
- 原因は操作の誤りで、Ctrl+N が続けて入り、試行が一度に進んで終わった。ツールの作りの問題ではない (conhost の中で `WriteConsoleInputW` で入力を流し込み、`raw` でも記録が取れることを確かめた)。
- 同じ誤りで試行を飛ばさないように、直前の Ctrl+N から 0.5 秒以内の Ctrl+N を無視するようにした。取り直した結果は §3 の表に書く。

### 3.3 Windows Terminal・raw と raw --vt (2026-11-16、取り直し)

- **`raw --vt` (ENABLE_VIRTUAL_TERMINAL_INPUT を立て、`ESC[?2004h` を書き出す) では見分けられる。** 貼り付けの 4 試行はすべて `ESC[200~` … `ESC[201~` で包まれて届き、IME の確定の 4 試行はどれも包まれずに届いた。1 文字の貼り付け (`paste-short`) も包まれる。複数行の改行は、包みの内側の `CR` で届く (`Enter` のキーにはならない)。記録はすべて仮想キー 0・スキャンコード 0 の文字で、端末が VT の列を文字として送っている。
- **`raw` (VT の入力なし。crossterm 0.29 の Windows の読み方と同じ) では見分けられない。**
  - IME の確定の文字は、仮想キー 0・スキャンコード 0 で届く。
  - 貼り付けは、英字 (`paste-ascii`) だけは文字ごとに仮想キーとスキャンコードが付くが、日本語 (`paste-ja`・`paste-short`) は IME の確定と同じく仮想キー 0・スキャンコード 0 で届く。
  - `VK_PACKET` (0xE7) はどちらにも出ない。
- まとめると、Windows Terminal で見分けるには、入力を VT の列として読む必要がある。crossterm 0.29 は Windows で VT の入力を使わない (§1.1) ので、今の evorag-tui のままでは見分けられない。使うなら、Windows で ENABLE_VIRTUAL_TERMINAL_INPUT を立て、VT の列を自分で解析する入力の層が要る (07 §2 の端末の層の変更。S-16 の範囲の外)。

### 3.4 conhost (2026-11-16、3 回)

- IME の確定は、Windows Terminal と同じく、`events` では `Char` の連続、`raw` では仮想キー 0・スキャンコード 0 の文字で届いた。
- 貼り付けの試行は、1・2 回目は Ctrl+V で行い、文字が 1 つも届かなかった。conhost では、raw mode (ENABLE_PROCESSED_INPUT なし) で読むアプリに Ctrl+V がそのままキー (`Char('v')` + `CONTROL`、`raw` では文字 0x16) として渡り、貼り付けにならない。evorag-tui も同じ読み方なので、**conhost では Ctrl+V で貼り付けられない** (S-16 の範囲の外。§5)。
- 3 回目は Ctrl+V を使わずに貼り付け、文字が届いた。届き方は Windows Terminal の `events`・`raw` と同じで、見分けられない。
  - `events` では、貼り付けも IME の確定も `Char` の連続。複数行の改行は `Enter`。
  - `raw` では、英字の貼り付けには仮想キーが付く (大文字の前に Shift の記録も来る) が、日本語の貼り付けは IME の確定と同じく仮想キー 0・スキャンコード 0。
- `raw --vt` は、3 回とも試行の前に終わった (Ctrl の記録だけ)。原因は調べていない。conhost は best effort で合格条件に入らない (07 §2.3) ので、ここで打ち切る。

## 4. 判定の基準

- 組み合わせごとに、**貼り付けの 4 試行のすべてに印があり、IME の 4 試行のどれにも印が無い** とき「見分けられる」。
- 届き方 (塊の大きさ・間隔) は見分けに使わない。IME の長い文の確定も、貼り付けと同じく 1 つの塊で届くため (§1.2)。
- 08 §3 の合格の基準のとおり、見分けられる組み合わせの表ができたら完了とする。見分けられる組み合わせでだけ、貼り付けの範囲を前のふるいに使う (04 §5.2)。見分けられない組み合わせでは、今の 04 §5.2 のとおり除かない。

## 5. 仕様への反映 (判定の後)

- 04 §5.2 の「端末の貼り付けの範囲では除かない」の段落を、判定に合わせて直す。
- 08 §3 の S-16 の行に、判定と日付を書く。
- §1.1 の Windows の `ESC[200~` の扱いの不具合は、起きないことを確かめた (§3.1)。
- S-16 で見つかった evorag-tui の不具合は、別の `fix/` で扱う。
  - Windows Terminal で複数行を貼り付けると、改行が `Enter` で届き、1 行目で送ってしまう (§3.1)。
  - conhost では Ctrl+V で貼り付けられない (§3.4)。
