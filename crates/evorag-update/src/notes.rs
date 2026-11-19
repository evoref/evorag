//! 示す文字列の整え方 (12 §7.2 の apply の 5。`notes`・通信の応答・子のプロセスの出力に使う)。

/// 示す文字列の上限 (バイト、12 §5.3 の `notes`)。
pub const DISPLAY_MAX_BYTES: usize = 4096;

/// 除く文字: Unicode の制御文字 (Cc) のうち LF 以外 (CR を含む) と、双方向の書式の文字
/// (U+202A〜U+202E、U+2066〜U+2069)。
fn is_removed(c: char) -> bool {
    (c.is_control() && c != '\n') || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// 端末に示せるように整える: 除く文字を除き、`max_bytes` を超えれば文字の境界で切る。
#[must_use]
pub fn sanitize_for_display(text: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_bytes));
    for c in text.chars().filter(|&c| !is_removed(c)) {
        if out.len() + c.len_utf8() > max_bytes {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_and_bidi_characters_are_removed_except_lf() {
        let text = "a\r\nb\tc\u{1b}[31md\u{7f}e\u{85}f\u{202E}g\u{2066}h\u{2069}i\u{200B}j\n";
        // U+200B (Cf だが双方向の書式の文字ではない) は残す。
        assert_eq!(
            sanitize_for_display(text, DISPLAY_MAX_BYTES),
            "a\nbc[31mdefghi\u{200B}j\n"
        );
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        let text = "あ".repeat(2000); // 6000 バイト
        let cut = sanitize_for_display(&text, DISPLAY_MAX_BYTES);
        assert_eq!(cut.len(), 4095);
        assert!(cut.chars().all(|c| c == 'あ'));
        assert_eq!(sanitize_for_display("abc", 2), "ab");
        assert_eq!(sanitize_for_display("short", DISPLAY_MAX_BYTES), "short");
    }
}
