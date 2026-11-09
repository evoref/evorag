# S-03 Windows Terminal での IME (技術検証)

- 日付: 2026-11-09
- 対象: 08 §3 の S-03、07 §2.3
- 判定: **合格** (2026-11-09、人が画面を見て確かめた。下の表のすべての項目が 4 つの IME で合)

## 確認用の TUI

- 場所: `spikes/S-03/` (独立の cargo プロジェクト)。依存は ratatui 0.30.2、crossterm 0.29.0 (ratatui の再公開を使う)、unicode-width 0.2.3、unicode-segmentation 1.13.3。
- ビルド: `cargo build --release` (x86_64-pc-windows-msvc) は成功 (警告なし)。Linux 向けのクロスビルドは、リンカが無いので失敗 (コードの問題ではない)。
- 実行ファイル: `spikes\S-03\target\release\s03-ime.exe` (確認の後に `target` は消した。確かめ直すときは `cargo build --release` で作り直す)
- 作り:
  - 描画のたびに `set_cursor_position` で端末の本当のカーソルを入力位置に置き、隠さない。
  - Backspace と ←→ は書記素の単位で動く。全角の文字の幅を数えて折り返す (行末に入らない全角は次の行へ)。
  - bracketed paste を有効にし、`Event::Paste` も入力として扱う。IME の確定の文字列は `Char` の連続か `Paste` で来るので、たまった分をまとめて処理してから 1 回描画する。
  - Enter で送信、Alt+Enter で改行 (Windows Terminal は Shift+Enter を Enter と区別しないことがある)、Esc か Ctrl+Q で終了。
  - `--hide-cursor-test`: わざとカーソルを左上に置く比較用のモード (IME の窓が入力位置に出ない例)。

## 確認の手順 (人が行う)

1. Windows Terminal を開き、`spikes\S-03\target\release\s03-ime.exe` を起動する。
2. 下の表の IME ごとに、入力欄で日本語を入力・変換し、各行を確かめて 合 / 否 を書く。
3. 最後に `s03-ime.exe --hide-cursor-test` で同じ操作をし、候補の窓が入力位置に出ない (= 本当のカーソルで位置が決まっている) ことを確かめる。

Microsoft IME の新・旧の切り替え: 設定 > 時刻と言語 > 言語と地域 > 日本語 > 言語のオプション > Microsoft IME > キーボードのオプション > 全般 の「以前のバージョンの Microsoft IME を使う」をオン・オフする。切り替えたら Windows Terminal を開き直す。

| 確かめること | MS IME (新) | MS IME (旧) | Google 日本語入力 | ATOK |
|---|---|---|---|---|
| 変換の候補の窓が入力位置に出る | 合 | 合 | 合 | 合 |
| 変換中の文字 (未確定の文字列) が入力位置に出る | 合 | 合 | 合 | 合 |
| 確定した文字が正しく入る (欠けない・重ならない) | 合 | 合 | 合 | 合 |
| 全角・半角が混じった行での位置 | 合 | 合 | 合 | 合 |
| 折り返した 2 行目での位置 | 合 | 合 | 合 | 合 |
| `--hide-cursor-test` では入力位置に出ない (比較) | 合 | 合 | 合 | 合 |

- 合格の基準 (08 §3): 4 つの組み合わせで、変換の候補の窓が入力位置に出る。
- 結果: 4 つの組み合わせすべてで合格。07 §2.3 の v0.1 の組み合わせ (4 つ) をそのまま v0.1 の合格条件とする。
- v0.1 の TUI (M1.4) は、この TUI の作り (本当のカーソルを毎回入力位置に置く、確定の文字列をまとめて処理する、Release のイベントを捨てる) を引き継ぐ。

## 気付いた点

- crossterm には IME の窓の位置を直接扱う API が無く、位置は端末の本当のカーソルだけで決まる。
- crossterm 0.29 の Windows では、キーの離した時のイベント (Release) も届く。入力として扱わない。
