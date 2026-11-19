//! 応答の Markdown の最低限の装飾 (FR-109、07 §2.1): 見出し・太字・コードブロックだけ。
//!
//! 色に頼らない (太字・下線・薄い表示を中心にする)。コードブロックの中は装飾せず、そのまま出す。
//! 伏せ字と `[非表示]` の印 (04 §7.3) は、本文のバイト位置の範囲に重ねる装飾 (`overlay`) で見分けられるようにする。

use std::ops::Range;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// 本文の範囲 (UTF-8 のバイト位置) に重ねる装飾。
pub type Overlay = [(Range<usize>, Style)];

/// 幅 `width` で折り返した、装飾つきの行 (頭に `indent` を付ける)。
pub fn render(text: &str, width: usize, indent: &str) -> Vec<Line<'static>> {
    render_marked(text, width, indent, &[])
}

/// [`render`] に、本文の範囲の装飾 `overlay` を重ねる (伏せ字と `[非表示]` の印)。
pub fn render_marked(
    text: &str,
    width: usize,
    indent: &str,
    overlay: &Overlay,
) -> Vec<Line<'static>> {
    let width = width.max(2);
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            out.extend(wrap_spans(
                text,
                vec![(raw, Style::new().add_modifier(Modifier::DIM))],
                width,
                indent,
                overlay,
            ));
            continue;
        }
        let spans = if in_code {
            vec![(
                raw,
                Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC),
            )]
        } else if let Some(h) = heading(trimmed) {
            vec![(
                h,
                Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )]
        } else {
            bold_spans(raw)
        };
        out.extend(wrap_spans(text, spans, width, indent, overlay));
    }
    out
}

/// 装飾をせずに折り返し、`overlay` だけを重ねる (利用者の発話)。
pub fn plain_marked(
    text: &str,
    width: usize,
    indent: &str,
    overlay: &Overlay,
) -> Vec<Line<'static>> {
    let width = width.max(2);
    text.split('\n')
        .flat_map(|raw| wrap_spans(text, vec![(raw, Style::new())], width, indent, overlay))
        .collect()
}

/// `inner` (`outer` の部分の文字列) の、`outer` の中の始まりのバイト位置。
fn offset_in(outer: &str, inner: &str) -> usize {
    let at = (inner.as_ptr() as usize).wrapping_sub(outer.as_ptr() as usize);
    debug_assert!(at <= outer.len(), "not a part of the text");
    at
}

/// `# 見出し` の本文 (`#` は 1〜6 個、後ろに空白)。
fn heading(line: &str) -> Option<&str> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) {
        line[hashes..].strip_prefix(' ').map(str::trim)
    } else {
        None
    }
}

/// `**太字**` を分ける (どれも `line` の部分の文字列)。閉じていない `**` はそのまま出す。
fn bold_spans(line: &str) -> Vec<(&str, Style)> {
    let parts: Vec<&str> = line.split("**").collect();
    if parts.len() < 3 {
        return vec![(line, Style::new())];
    }
    let mut out = Vec::new();
    let pairs = (parts.len() - 1) / 2 * 2; // 閉じた組の数の分だけ太字にする
    for (i, p) in parts.iter().enumerate() {
        if i > pairs {
            // 閉じていない `**` と、その後ろ。
            let at = offset_in(line, p);
            out.push((&line[at - 2..at + p.len()], Style::new()));
        } else if i % 2 == 1 {
            out.push((*p, Style::new().add_modifier(Modifier::BOLD)));
        } else if !p.is_empty() {
            out.push((*p, Style::new()));
        }
    }
    out
}

/// `at` (本文のバイト位置) に重ねる装飾。
fn overlay_at(overlay: &Overlay, at: usize) -> Option<Style> {
    overlay
        .iter()
        .find(|(r, _)| r.contains(&at))
        .map(|(_, s)| *s)
}

/// 装飾の区切りを保ったまま、幅で折り返す。`spans` は `text` の部分の文字列。
fn wrap_spans(
    text: &str,
    spans: Vec<(&str, Style)>,
    width: usize,
    indent: &str,
    overlay: &Overlay,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut cur: Vec<Span<'static>> = vec![Span::raw(indent.to_owned())];
    let mut col = 0;
    let mut piece = String::new();
    let mut piece_style = Style::new();
    for (part, style) in spans {
        let base = offset_in(text, part);
        for (i, g) in part.grapheme_indices(true) {
            let w = UnicodeWidthStr::width(g);
            let st = overlay_at(overlay, base + i).map_or(style, |o| style.patch(o));
            if col + w > width {
                if !piece.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut piece), piece_style));
                }
                lines.push(Line::from(std::mem::replace(
                    &mut cur,
                    vec![Span::raw(indent.to_owned())],
                )));
                col = 0;
            }
            if st != piece_style && !piece.is_empty() {
                cur.push(Span::styled(std::mem::take(&mut piece), piece_style));
            }
            piece_style = st;
            piece.push_str(g);
            col += w;
        }
    }
    if !piece.is_empty() {
        cur.push(Span::styled(piece, piece_style));
    }
    lines.push(Line::from(cur));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn headings_bold_and_code_blocks() {
        let text = "# 見出し\n本文の **太字** と普通\n```rust\nlet a = **b**;\n```\n終わり";
        let lines = render(text, 40, "  ");
        assert_eq!(
            plain(&lines),
            [
                "  見出し",
                "  本文の 太字 と普通",
                "  ```rust",
                "  let a = **b**;",
                "  ```",
                "  終わり"
            ]
        );
        assert!(
            lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        let bold = lines[1].spans.iter().find(|s| s.content == "太字").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        // コードブロックの中の ** は太字にしない。
        assert!(
            lines[3]
                .spans
                .iter()
                .all(|s| !s.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn an_unclosed_marker_is_kept_and_wide_text_wraps() {
        assert_eq!(plain(&render("a **b", 40, "")), ["a **b"]);
        let lines = render("**あいうえお**", 6, "");
        assert_eq!(plain(&lines), ["あいう", "えお"]);
        assert!(
            lines[1]
                .spans
                .iter()
                .all(|s| s.content.is_empty() || s.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    /// 印の範囲は、装飾を重ねても本文と折り返しを変えない (04 §7.3)。
    #[test]
    fn an_overlay_styles_only_its_range() {
        let text = "私は[伏せ字]に住む。**太字**";
        let at = text.find('[').unwrap();
        let mark = Style::new().add_modifier(Modifier::REVERSED);
        let overlay = [(at..at + "[伏せ字]".len(), mark)];
        let lines = render_marked(text, 80, "  ", &overlay);
        assert_eq!(plain(&lines), ["  私は[伏せ字]に住む。太字"]);
        let marked: Vec<&str> = lines[0]
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, ["[伏せ字]"]);
        let plain_lines = plain_marked("a\n[伏せ字]b", 80, "", &[(2..2 + "[伏せ字]".len(), mark)]);
        assert_eq!(plain(&plain_lines), ["a", "[伏せ字]b"]);
        assert!(
            plain_lines[1].spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn hashes_without_a_space_are_not_headings() {
        assert_eq!(heading("#タグ"), None);
        assert_eq!(heading("## 二"), Some("二"));
        assert_eq!(heading("####### 七"), None);
    }
}
