//! 入力欄 (07 §2.1、§2.3)。S-03 の作りを引き継ぐ。
//!
//! - カーソルは書記素の境界のバイト位置。Backspace と ←→ は書記素の単位で動く (`unicode-segmentation`)。
//! - 幅は `unicode-width` で数え、行末に入らない全角は次の行へ折り返す。
//! - 画面は描画のたびに、端末の本当のカーソルを `layout` の位置に置き、隠さない
//!   (IME の候補の窓が入力位置に出るようにするため)。

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InputBox {
    text: String,
    cursor: usize,
}

/// 折り返した入力欄。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputLayout {
    pub lines: Vec<String>,
    /// カーソルの行 (0 から)。
    pub row: usize,
    /// カーソルの桁 (表示の幅で数える)。
    pub col: usize,
}

impl InputBox {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// 文字列を差し込む (IME の確定の文字列や貼り付け)。改行は `\n` にそろえる。
    pub fn insert(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map(|g| self.cursor + g.len())
    }

    pub fn backspace(&mut self) {
        if let Some(p) = self.prev_boundary() {
            self.text.replace_range(p..self.cursor, "");
            self.cursor = p;
        }
    }

    pub fn delete(&mut self) {
        if let Some(n) = self.next_boundary() {
            self.text.replace_range(self.cursor..n, "");
        }
    }

    pub fn left(&mut self) {
        if let Some(p) = self.prev_boundary() {
            self.cursor = p;
        }
    }

    pub fn right(&mut self) {
        if let Some(n) = self.next_boundary() {
            self.cursor = n;
        }
    }

    /// 今の行の先頭へ。
    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
    }

    /// 今の行の末尾へ。
    pub fn end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i);
    }

    /// 中身を取り出して空にする (送信)。
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// 中身を置き換える (送れなかった発話を戻すときなど)。カーソルは末尾。
    pub fn set(&mut self, text: &str) {
        self.text = text.replace("\r\n", "\n");
        self.cursor = self.text.len();
    }

    /// 幅 `width` で折り返す。
    pub fn layout(&self, width: usize) -> InputLayout {
        let width = width.max(2);
        let mut lines = vec![String::new()];
        let mut col = 0usize;
        let (mut row, mut ccol) = (0usize, 0usize);
        for (i, g) in self.text.grapheme_indices(true) {
            if g.contains('\n') {
                if i == self.cursor {
                    row = lines.len() - 1;
                    ccol = col;
                }
                lines.push(String::new());
                col = 0;
                continue;
            }
            let w = UnicodeWidthStr::width(g);
            if col + w > width {
                lines.push(String::new());
                col = 0;
            }
            if i == self.cursor {
                row = lines.len() - 1;
                ccol = col;
            }
            if let Some(l) = lines.last_mut() {
                l.push_str(g);
            }
            col += w;
        }
        if self.cursor >= self.text.len() {
            row = lines.len() - 1;
            ccol = col;
            if col >= width {
                // 行末ちょうどで終わるときは、次の行の先頭に置く。
                lines.push(String::new());
                row += 1;
                ccol = 0;
            }
        }
        InputLayout {
            lines,
            row,
            col: ccol,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(s: &str) -> InputBox {
        let mut b = InputBox::default();
        b.insert(s);
        b
    }

    #[test]
    fn moves_and_deletes_by_grapheme() {
        // 結合文字と絵文字の連結は 1 つの書記素。
        let mut b = boxed("かが\u{3099}👨‍👩‍👧x");
        b.backspace();
        assert_eq!(b.text(), "かが\u{3099}👨‍👩‍👧");
        b.backspace();
        assert_eq!(b.text(), "かが\u{3099}");
        b.left();
        b.backspace();
        assert_eq!(b.text(), "が\u{3099}");
        b.home();
        b.delete();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn wraps_wide_characters_to_the_next_line() {
        let b = boxed("あいう");
        let l = b.layout(5);
        // 幅 5 に全角 2 文字 (幅 4) は入り、3 文字目は次の行。
        assert_eq!(l.lines, ["あい", "う"]);
        assert_eq!((l.row, l.col), (1, 2));
    }

    #[test]
    fn cursor_at_an_exact_line_end_goes_to_the_next_line() {
        let l = boxed("abcd").layout(4);
        assert_eq!(l.lines, ["abcd", ""]);
        assert_eq!((l.row, l.col), (1, 0));
    }

    #[test]
    fn newlines_and_the_cursor_position() {
        let mut b = boxed("一行目\r\n二行目");
        assert_eq!(b.text(), "一行目\n二行目");
        b.home();
        let l = b.layout(20);
        assert_eq!(l.lines, ["一行目", "二行目"]);
        assert_eq!((l.row, l.col), (1, 0));
        b.left();
        let l = b.layout(20);
        assert_eq!((l.row, l.col), (0, 6));
        b.end();
        b.insert("!");
        assert_eq!(b.text(), "一行目!\n二行目");
    }

    #[test]
    fn take_and_set() {
        let mut b = boxed("送る");
        assert_eq!(b.take(), "送る");
        assert!(b.is_empty());
        b.set("戻す");
        b.insert("!");
        assert_eq!(b.text(), "戻す!");
    }
}
