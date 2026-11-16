//! ターンをチャンクに分ける (05 §2.4)。
//!
//! - 段落の区切り (空行) で分け、1 つのチャンクを 400 文字 (目安) までにする。短いターンは 1 つ。
//! - 長い段落は文の区切り (。！？.!? と改行) で分け、前のチャンクと 50 文字を重ねる。
//! - コードブロック (```` ``` ```` で囲んだ範囲) は途中で切らない。400 文字を超えるときだけ、行の区切りで分ける。
//! - 位置は文字 (Unicode のスカラー値) の数で数える。本文は写さない (ターンの本文から作り直せる)。

/// 区切り方の版。規則を変えたら上げる (チャンクを作り直す)。
pub const CHUNK_RULES_VERSION: u32 = 1;
/// 1 つのチャンクの文字数の目安。
pub const MAX_CHARS: usize = 400;
/// 長い段落を分けるときに、前のチャンクと重ねる文字数。
pub const OVERLAP: usize = 50;

/// 1 つの範囲 (文字の位置、`start..end`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// ブロック: 段落かコードブロック (文字の位置の範囲)。
struct Block {
    start: usize,
    end: usize,
    code: bool,
}

fn blocks(chars: &[char]) -> Vec<Block> {
    // 行に分ける (行末の改行を含めた範囲)。
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut s = 0;
    for (i, c) in chars.iter().enumerate() {
        if *c == '\n' {
            lines.push((s, i + 1));
            s = i + 1;
        }
    }
    if s < chars.len() {
        lines.push((s, chars.len()));
    }
    let text = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
    let mut out: Vec<Block> = Vec::new();
    let mut cur: Option<(usize, usize)> = None;
    let mut in_code = false;
    let mut code_start = 0;
    for (a, b) in lines {
        let line = text(a, b);
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if in_code {
                in_code = false;
                out.push(Block {
                    start: code_start,
                    end: b,
                    code: true,
                });
            } else {
                if let Some((s, e)) = cur.take() {
                    out.push(Block {
                        start: s,
                        end: e,
                        code: false,
                    });
                }
                in_code = true;
                code_start = a;
            }
            continue;
        }
        if in_code {
            continue;
        }
        if trimmed.is_empty() {
            if let Some((s, e)) = cur.take() {
                out.push(Block {
                    start: s,
                    end: e,
                    code: false,
                });
            }
            continue;
        }
        cur = Some(match cur {
            Some((s, _)) => (s, b),
            None => (a, b),
        });
    }
    if in_code {
        // 閉じていないコードブロックは、最後までを 1 つにする。
        out.push(Block {
            start: code_start,
            end: chars.len(),
            code: true,
        });
    } else if let Some((s, e)) = cur {
        out.push(Block {
            start: s,
            end: e,
            code: false,
        });
    }
    out
}

fn is_sentence_end(c: char) -> bool {
    matches!(c, '。' | '！' | '？' | '.' | '!' | '?' | '\n')
}

/// 長い段落を、文の区切りで `MAX_CHARS` までに分け、前と `OVERLAP` 文字を重ねる。
fn split_long(chars: &[char], start: usize, end: usize, code: bool) -> Vec<Span> {
    let mut out = Vec::new();
    let mut s = start;
    while end - s > MAX_CHARS {
        let limit = s + MAX_CHARS;
        // 範囲の中で最後の区切り (コードは改行だけ)。無ければ上限で切る。
        let cut = (s + 1..=limit)
            .rev()
            .find(|&i| {
                let c = chars[i - 1];
                if code { c == '\n' } else { is_sentence_end(c) }
            })
            .filter(|&i| i > s + OVERLAP)
            .unwrap_or(limit);
        out.push(Span { start: s, end: cut });
        // コードは重ねない (行の途中から始めないため)。
        s = if code {
            cut
        } else {
            cut.saturating_sub(OVERLAP).max(s + 1)
        };
    }
    if s < end {
        out.push(Span { start: s, end });
    }
    out
}

/// 本文をチャンクに分ける。空白だけの本文はチャンクを作らない。
pub fn split(text: &str) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    if text.trim().is_empty() {
        return Vec::new();
    }
    let bs = blocks(&chars);
    let mut out: Vec<Span> = Vec::new();
    // 短い段落は、続く段落と合わせて目安までまとめる (コードブロックは単独)。
    let mut pending: Option<Span> = None;
    for b in bs {
        let len = b.end - b.start;
        if b.code || len > MAX_CHARS {
            if let Some(p) = pending.take() {
                out.push(p);
            }
            if len > MAX_CHARS {
                out.extend(split_long(&chars, b.start, b.end, b.code));
            } else {
                out.push(Span {
                    start: b.start,
                    end: b.end,
                });
            }
            continue;
        }
        pending = Some(match pending {
            Some(p) if b.end - p.start <= MAX_CHARS => Span {
                start: p.start,
                end: b.end,
            },
            Some(p) => {
                out.push(p);
                Span {
                    start: b.start,
                    end: b.end,
                }
            }
            None => Span {
                start: b.start,
                end: b.end,
            },
        });
    }
    if let Some(p) = pending {
        out.push(p);
    }
    out
}

/// 範囲の本文。
pub fn slice(text: &str, span: Span) -> String {
    text.chars()
        .skip(span.start)
        .take(span.end - span.start)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(t: &str) -> Vec<String> {
        split(t).into_iter().map(|s| slice(t, s)).collect()
    }

    #[test]
    fn a_short_turn_is_one_chunk() {
        assert_eq!(texts("こんにちは。元気？"), ["こんにちは。元気？"]);
        assert!(split("  \n ").is_empty());
    }

    #[test]
    fn paragraphs_are_merged_up_to_the_limit_and_split_beyond_it() {
        let p = "あ".repeat(150);
        let t = format!("{p}\n\n{p}\n\n{p}");
        let c = texts(&t);
        assert_eq!(c.len(), 2, "{c:?}");
        assert!(c[0].chars().count() <= MAX_CHARS);
        assert!(c[1].starts_with('あ'));
    }

    #[test]
    fn a_long_paragraph_is_split_at_sentences_with_overlap() {
        let sentence = format!("{}。", "い".repeat(99));
        let t = sentence.repeat(10); // 1,000 文字
        let spans = split(&t);
        assert!(spans.len() >= 3, "{spans:?}");
        for w in spans.windows(2) {
            assert!(w[0].end - w[0].start <= MAX_CHARS);
            assert_eq!(w[0].end - w[1].start, OVERLAP, "{w:?}");
        }
        assert_eq!(spans.last().unwrap().end, 1000);
        // 文の区切りで切る。
        assert!(slice(&t, spans[0]).ends_with('。'));
    }

    #[test]
    fn code_blocks_are_not_cut_unless_too_long() {
        let code = "```rust\nfn main() {\n    println!(\"x\");\n}\n```\n";
        let t = format!("前の段落。\n{code}後の段落。");
        let c = texts(&t);
        assert_eq!(c.len(), 3, "{c:?}");
        assert!(c[1].starts_with("```rust") && c[1].trim_end().ends_with("```"));
        let long = format!("```\n{}```\n", "let x = 1;\n".repeat(60));
        let spans = split(&long);
        assert!(spans.len() >= 2);
        for s in &spans {
            assert!(s.end - s.start <= MAX_CHARS);
            // 行の区切りで切る。
            assert!(slice(&long, *s).ends_with('\n') || s.end == long.chars().count());
        }
    }

    #[test]
    fn the_chunks_cover_every_character() {
        let t = format!(
            "{}\n\n```\ncode\n```\n{}",
            "う".repeat(500),
            "え".repeat(30)
        );
        let spans = split(&t);
        let mut covered = vec![false; t.chars().count()];
        for s in &spans {
            for c in &mut covered[s.start..s.end] {
                *c = true;
            }
        }
        // 空行の区切り以外は、すべてどれかのチャンクに入る。
        for (i, ch) in t.chars().enumerate() {
            if ch != '\n' {
                assert!(covered[i], "char {i} {ch:?} not covered");
            }
        }
    }
}
