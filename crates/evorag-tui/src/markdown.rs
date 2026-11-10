//! 応答の Markdown の最低限の装飾 (FR-109、07 §2.1): 見出し・太字・コードブロックだけ。
//!
//! 色に頼らない (太字・下線・薄い表示を中心にする)。コードブロックの中は装飾せず、そのまま出す。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// 幅 `width` で折り返した、装飾つきの行 (頭に `indent` を付ける)。
pub fn render(text: &str, width: usize, indent: &str) -> Vec<Line<'static>> {
    let width = width.max(2);
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            out.extend(wrap_spans(
                vec![(raw.to_owned(), Style::new().add_modifier(Modifier::DIM))],
                width,
                indent,
            ));
            continue;
        }
        let spans = if in_code {
            vec![(
                raw.to_owned(),
                Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC),
            )]
        } else if let Some(h) = heading(trimmed) {
            vec![(
                h.to_owned(),
                Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )]
        } else {
            bold_spans(raw)
        };
        out.extend(wrap_spans(spans, width, indent));
    }
    out
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

/// `**太字**` を分ける。閉じていない `**` はそのまま出す。
fn bold_spans(line: &str) -> Vec<(String, Style)> {
    let parts: Vec<&str> = line.split("**").collect();
    if parts.len() < 3 {
        return vec![(line.to_owned(), Style::new())];
    }
    let mut out = Vec::new();
    let pairs = (parts.len() - 1) / 2 * 2; // 閉じた組の数の分だけ太字にする
    for (i, p) in parts.iter().enumerate() {
        if i > pairs {
            out.push((format!("**{p}"), Style::new()));
        } else if i % 2 == 1 {
            out.push(((*p).to_owned(), Style::new().add_modifier(Modifier::BOLD)));
        } else if !p.is_empty() {
            out.push(((*p).to_owned(), Style::new()));
        }
    }
    out
}

/// 装飾の区切りを保ったまま、幅で折り返す。
fn wrap_spans(spans: Vec<(String, Style)>, width: usize, indent: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut cur: Vec<Span<'static>> = vec![Span::raw(indent.to_owned())];
    let mut col = 0;
    for (text, style) in spans {
        let mut piece = String::new();
        for g in text.graphemes(true) {
            let w = UnicodeWidthStr::width(g);
            if col + w > width {
                if !piece.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut piece), style));
                }
                lines.push(Line::from(std::mem::replace(
                    &mut cur,
                    vec![Span::raw(indent.to_owned())],
                )));
                col = 0;
            }
            piece.push_str(g);
            col += w;
        }
        if !piece.is_empty() {
            cur.push(Span::styled(piece, style));
        }
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

    #[test]
    fn hashes_without_a_space_are_not_headings() {
        assert_eq!(heading("#タグ"), None);
        assert_eq!(heading("## 二"), Some("二"));
        assert_eq!(heading("####### 七"), None);
    }
}
