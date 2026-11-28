//! 訂正の字句の型の照合 (04 §6 の「訂正の型の使い方」「穴の範囲」「読点だけの固定の部分」「古い値は穴の全体」
//! 「文末の「よ」「です」」「区切りの文字の判定」「照合の費用」)。
//!
//! v0.4 の旧エンジンの `correction.rs` の照合の部分 (`Side`・`Compiled`・`compile`・`variants`・`compact`・`View`・
//! `HoleAt`・`Matched`・`Matcher`・`first_match`・`delimits`・`is_line_break`) を写した (旧エンジンは M5.5 で外す、
//! 08 §2.6)。照合の結果は同じにし、写した元で検証にだけ使っていた項目 (一致の全体・錨の位置・範囲の引き直し) は
//! 除いた。穴は本文の文字列ではなく、照合した本文 (前のふるいの後の本文) の範囲で返す。
//!
//! TODO(M5.5): `delimits`・`is_line_break` は関門 (B3) の `b3/place.rs` にも写してある。旧エンジンを外すときに
//! 1 か所 (`chars.rs` など) にまとめる。

use std::ops::Range;

use crate::value_key::{Mapped, normalize_v1};

use super::MATCH_MAX_CHARS;

/// 型の穴。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Old,
    New,
}

/// 照合の形にした訂正の型。固定の部分 (錨) は、照合の形 ([`compact`]) にした変種 (文末の「よ」「です」の有無)
/// の並びで持つ。穴は、最初の錨の前 (`lead`) と、各錨の後 (次の錨の前か、末尾) にまとめる。1 か所に穴が 2 つ
/// 並ぶのは、間の固定の部分が読点・空白だけの型 (「{new}、{old}ではなく」) で、穴は本文の読点・空白で分ける。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Compiled {
    lead: Vec<Side>,
    anchors: Vec<(Vec<Vec<char>>, Vec<Side>)>,
}

/// 照合で区別しない文字 (空白と読点)。値の鍵の正規化 v1 の後に見る (`，`・`､` は NFKC で `,`・`、` になる)。
fn ignorable(c: char) -> bool {
    c.is_whitespace() || matches!(c, '、' | ',')
}

/// 読点 (`{old}` の穴を切る文字、04 §6 の「古い値は穴の全体」)。元の本文の文字で見る。
fn is_comma(c: char) -> bool {
    matches!(c, '、' | '，' | ',' | '､')
}

/// 照合の前にそろえる文字 (向きの違うアポストロフィ。"it’s" を "it's" に当てる)。
fn canon(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{02BC}' => '\'',
        _ => c,
    }
}

/// 改行 (04 §5.2 の行の区切り)。
fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}')
}

/// 元の本文の書記素 `cluster` が文の区切りか (04 §6 の「区切りの文字の判定」)。全角の `。．！？` は常に、半角の
/// `.!?` は後ろが空白・改行・本文の終わりのときだけ区切り (小数・URL・略語で文を切らない)。文番号の文の分け方
/// v1 (04 §2) には寄せない。
fn delimits(text: &str, cluster: &Range<usize>) -> bool {
    match &text[cluster.clone()] {
        "。" | "．" | "！" | "？" | "｡" => true,
        "." | "!" | "?" => text[cluster.end..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace),
        _ => false,
    }
}

/// 照合の形: 値の鍵の正規化 v1 の後に、区別しない文字を除き、アポストロフィをそろえる。
fn compact(s: &str) -> Vec<char> {
    normalize_v1(s)
        .chars()
        .filter(|&c| !ignorable(c))
        .map(canon)
        .collect()
}

/// 固定の部分の変種: 文末 (後ろが句読点・空白・部分の終わり) の「よ」「です」を、それぞれ有りと無しにする。
pub(super) fn variants(seg: &str) -> Vec<Vec<char>> {
    /// 有りと無しを作る「よ」「です」の数の上限 (変種は 2 の累乗で増えるため)。
    const MAX_OPTIONAL: usize = 8;
    let ends = |rest: &str| {
        rest.chars().next().is_none_or(|c| {
            c.is_whitespace()
                || matches!(c, '。' | '、' | ',' | '，' | '.' | '!' | '！' | '?' | '？')
        })
    };
    let mut optional: Vec<Range<usize>> = Vec::new();
    for word in ["です", "よ"] {
        for (i, _) in seg.match_indices(word) {
            if ends(&seg[i + word.len()..]) {
                optional.push(i..i + word.len());
            }
        }
    }
    optional.sort_by_key(|r| r.start);
    optional.truncate(MAX_OPTIONAL);
    let mut out: Vec<Vec<char>> = Vec::new();
    for mask in 0..(1u32 << optional.len()) {
        let mut s = String::with_capacity(seg.len());
        let mut at = 0;
        for (k, r) in optional.iter().enumerate() {
            if mask & (1 << k) != 0 {
                s.push_str(&seg[at..r.start]);
                at = r.end;
            }
        }
        s.push_str(&seg[at..]);
        let v = compact(&s);
        if !out.contains(&v) {
            out.push(v);
        }
    }
    // 同じ位置なら長い変種を先に試す (固定の部分を広く取る)。
    out.sort_by_key(|v| std::cmp::Reverse(v.len()));
    out
}

/// 訂正の型を照合の形にする。`{new}` がちょうど 1 つ、`{old}` が 1 つまで、固定の部分が空でない型だけ。並んだ
/// 2 つの穴の間は、読点・空白だけの固定の部分でなければならない。
pub(super) fn compile(pattern: &str) -> Option<Compiled> {
    enum Raw {
        Hole(Side),
        Fixed(Vec<Vec<char>>),
        Sep,
    }
    let mut raw = Vec::new();
    let (mut olds, mut news) = (0, 0);
    let mut rest = pattern;
    let push_fixed = |raw: &mut Vec<Raw>, seg: &str| {
        if seg.is_empty() {
            return;
        }
        let v: Vec<Vec<char>> = variants(seg)
            .into_iter()
            .filter(|x| !x.is_empty())
            .collect();
        raw.push(if v.is_empty() {
            Raw::Sep
        } else {
            Raw::Fixed(v)
        });
    };
    loop {
        let next = [("{old}", Side::Old), ("{new}", Side::New)]
            .into_iter()
            .filter_map(|(m, side)| rest.find(m).map(|i| (i, m, side)))
            .min_by_key(|x| x.0);
        let Some((i, marker, side)) = next else {
            push_fixed(&mut raw, rest);
            break;
        };
        push_fixed(&mut raw, &rest[..i]);
        raw.push(Raw::Hole(side));
        match side {
            Side::Old => olds += 1,
            Side::New => news += 1,
        }
        rest = &rest[i + marker.len()..];
    }
    if news != 1 || olds > 1 {
        return None;
    }
    let mut c = Compiled {
        lead: Vec::new(),
        anchors: Vec::new(),
    };
    let mut sep_open = false;
    let mut after_hole = false;
    for r in raw {
        let group = match c.anchors.last_mut() {
            Some(a) => &mut a.1,
            None => &mut c.lead,
        };
        match r {
            Raw::Hole(side) => {
                // 区切りの無い穴の並び (「{old}{new}」) は分けられない。
                if after_hole && !sep_open {
                    return None;
                }
                group.push(side);
                after_hole = true;
                sep_open = false;
            }
            Raw::Sep => {
                // 読点・空白だけの固定の部分は、2 つの穴の間にだけ置ける。
                if !after_hole || sep_open {
                    return None;
                }
                sep_open = true;
            }
            Raw::Fixed(v) => {
                if sep_open {
                    return None;
                }
                c.anchors.push((v, Vec::new()));
                after_hole = false;
            }
        }
    }
    (!sep_open && !c.anchors.is_empty()).then_some(c)
}

/// 照合する本文 (正規化 v1 の対応表と、区別しない文字を除いた文字の並び)。文の区切り・読点・空白の位置は、作る
/// ときに表にしておき、照合の中では表を引くだけにする (照合を本文の長さにおおむね線形で終えるため)。
pub(super) struct View {
    mapped: Mapped,
    chars: Vec<char>,
    /// `chars` の文字ごとの、正規化の後の本文の範囲。
    out: Vec<Range<usize>>,
    /// `chars[i]` と `chars[i - 1]` の間に、改行があった。
    newline: Vec<bool>,
    /// `chars[i]` の文の番号 (区切りの文字の後と改行で増える)。
    sid: Vec<usize>,
    /// 文ごとの (始まり, 終わり)。終わりは文の区切りの文字を含まない。
    sentences: Vec<(usize, usize)>,
    /// `[0, i)` の文の区切りの文字の数。
    term_before: Vec<usize>,
    /// `[0, i)` の位置の改行 (`newline[j]`) の数。
    newline_before: Vec<usize>,
    /// `i` 以前で最後の、`gap`・読点のある位置 (無ければ `usize::MAX`)。
    prev_gap: Vec<usize>,
    prev_comma: Vec<usize>,
    /// `i` 以降で最初の、`gap`・読点のある位置 (無ければ `chars.len()`)。
    next_gap: Vec<usize>,
    next_comma: Vec<usize>,
}

impl View {
    /// 表を作る。正規化の後の文字が [`MATCH_MAX_CHARS`] を超えれば `None` (型を当てない)。正規化 v1 の位置を元の
    /// 本文に戻せない本文 (書記素をまたぐ合成) も `None`。
    pub(super) fn new(text: &str) -> Option<Self> {
        let mapped = Mapped::new(text)?;
        let mut chars = Vec::new();
        let mut out = Vec::new();
        let mut gap = Vec::new();
        let mut comma = Vec::new();
        let mut newline = Vec::new();
        let mut term = Vec::new();
        let mut skipped = false;
        let mut prev_cluster: Option<Range<usize>> = None;
        for (i, c) in mapped.text().char_indices() {
            if ignorable(c) {
                skipped = true;
                continue;
            }
            let cluster = mapped.cluster_of(i);
            let between = prev_cluster
                .as_ref()
                .filter(|p| p.end <= cluster.start)
                .map_or("", |p| &text[p.end..cluster.start]);
            newline.push(between.chars().any(is_line_break));
            comma.push(between.chars().any(is_comma));
            gap.push(skipped || !between.is_empty());
            term.push(delimits(text, &cluster));
            skipped = false;
            chars.push(canon(c));
            out.push(i..i + c.len_utf8());
            prev_cluster = Some(cluster);
            if chars.len() > MATCH_MAX_CHARS {
                return None;
            }
        }
        let n = chars.len();
        let mut sid = Vec::with_capacity(n);
        let mut sentences: Vec<(usize, usize)> = Vec::new();
        for i in 0..n {
            let new_sentence = i == 0 || term[i - 1] || newline[i];
            if new_sentence {
                sentences.push((i, i));
            }
            sid.push(sentences.len() - 1);
            if !term[i]
                && let Some(s) = sentences.last_mut()
            {
                s.1 = i + 1;
            }
        }
        let prefix = |flags: &[bool]| -> Vec<usize> {
            let mut v = Vec::with_capacity(n + 1);
            v.push(0);
            for &f in flags {
                v.push(v.last().copied().unwrap_or(0) + usize::from(f));
            }
            v
        };
        let prev = |flags: &[bool]| -> Vec<usize> {
            let mut last = usize::MAX;
            flags
                .iter()
                .enumerate()
                .map(|(i, &f)| {
                    if f {
                        last = i;
                    }
                    last
                })
                .collect()
        };
        let next = |flags: &[bool]| -> Vec<usize> {
            let mut v = vec![n; n + 1];
            for i in (0..n).rev() {
                v[i] = if flags[i] { i } else { v[i + 1] };
            }
            v
        };
        Some(Self {
            term_before: prefix(&term),
            newline_before: prefix(&newline),
            prev_gap: prev(&gap),
            prev_comma: prev(&comma),
            next_gap: next(&gap),
            next_comma: next(&comma),
            mapped,
            chars,
            out,
            newline,
            sid,
            sentences,
        })
    }

    pub(super) fn len(&self) -> usize {
        self.chars.len()
    }

    /// 錨の始まり `a` の前の穴の始まり (同じ文の頭)。
    fn sentence_start(&self, a: usize) -> usize {
        self.sentences[self.sid[a]].0
    }

    /// 錨の終わり `b` の後の穴の終わり (同じ文の終わり。直後が改行なら空)。
    fn sentence_end(&self, b: usize) -> usize {
        if b >= self.len() || self.newline[b] {
            return b;
        }
        self.sentences[self.sid[b]].1.max(b)
    }

    /// `[b, a)` が同じ文の中か (文の区切りを含まず、`b` から `a` までに改行が無い)。
    fn within_sentence(&self, b: usize, a: usize) -> bool {
        let nl_end = (a + 1).min(self.len());
        self.term_before[a] == self.term_before[b]
            && self.newline_before[nl_end] == self.newline_before[b.min(nl_end)]
    }

    /// `(s, e)` の中の、最後か最初の区切り (`prev`・`next` は `prev_*`・`next_*` の表)。
    fn mark_in(
        &self,
        s: usize,
        e: usize,
        last: bool,
        prev: &[usize],
        next: &[usize],
    ) -> Option<usize> {
        if e <= s + 1 {
            return None;
        }
        let j = if last { prev[e - 1] } else { next[s + 1] };
        (j != usize::MAX && j > s && j < e).then_some(j)
    }

    /// `chars` の範囲 (空でない) を、正規化の後の本文の範囲にする。
    fn out_range(&self, r: &Range<usize>) -> Range<usize> {
        self.out[r.start].start..self.out[r.end - 1].end
    }

    /// `chars` の範囲を、元の本文の範囲 (書記素の境界) にする。書記素の展開の途中に来たら `None`。
    fn source(&self, r: &Range<usize>) -> Option<Range<usize>> {
        let o = self.out_range(r);
        self.mapped.source_of(o.start, o.end)
    }

    /// 固定の部分の一致 `[a, b)` が、英数字の語の途中に始まり・終わらないか (英語の型を語の境界で当てる)。
    fn bounded(&self, a: usize, b: usize) -> bool {
        let text = self.mapped.text();
        let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
        let before = text[..self.out[a].start].chars().next_back();
        let after = text[self.out[b - 1].end..].chars().next();
        !(self.chars[a].is_ascii_alphanumeric() && word(before))
            && !(self.chars[b - 1].is_ascii_alphanumeric() && word(after))
    }

    /// 錨の変種の一致 (`region` の中、始まりの順。同じ始まりなら長い変種が先)。
    fn occurrences(&self, variants: &[Vec<char>], region: &Range<usize>) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for f in variants {
            let len = f.len();
            if len == 0 || region.end < region.start + len {
                continue;
            }
            for a in region.start..=region.end - len {
                if self.chars[a] == f[0]
                    && self.chars[a..a + len] == f[..]
                    && self.bounded(a, a + len)
                {
                    out.push((a, len));
                }
            }
        }
        out.sort_by_key(|&(a, len)| (a, std::cmp::Reverse(len)));
        out
    }
}

/// 型の一致の穴 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HoleAt {
    pub(super) side: Side,
    /// `View::chars` の範囲 (空でない)。
    range: Range<usize>,
    /// 後ろに固定の部分が無い (末尾の穴)。値の始まりを穴の始まりに合わせる。
    trailing: bool,
    /// 末尾の穴で、文末で終わる (終わりの「よ」「です」を 1 回だけ除く)。
    at_end: bool,
}

impl HoleAt {
    /// 値を比べる範囲 (04 §6 の「文末の「よ」「です」」)。文末で終わる末尾の穴の、終わりの「です」か「よ」を
    /// 1 回だけ除く (1 文字は残す)。ほかの穴は除かない。
    fn trimmed(&self, v: &View) -> Range<usize> {
        let r = self.range.clone();
        if !(self.trailing && self.at_end) {
            return r;
        }
        let tail = &v.chars[r.clone()];
        if tail.len() > 2 && tail.ends_with(&['で', 'す']) {
            r.start..r.end - 2
        } else if tail.len() > 1 && tail.ends_with(&['よ']) {
            r.start..r.end - 1
        } else {
            r
        }
    }

    /// 値を比べる範囲 ([`HoleAt::trimmed`]) の、照合した本文 (元の本文) のバイト範囲。書記素の展開の途中に来たら
    /// `None`。
    pub(super) fn source(&self, v: &View) -> Option<Range<usize>> {
        v.source(&self.trimmed(v))
    }
}

/// 型の最初の一致 (04 §6 の「訂正の型の使い方」)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Matched {
    holes: Vec<HoleAt>,
}

impl Matched {
    pub(super) fn hole(&self, side: Side) -> Option<&HoleAt> {
        self.holes.iter().find(|h| h.side == side)
    }
}

/// 照合の状態 (型・本文と、錨ごとの一致の表)。
struct Matcher<'a> {
    c: &'a Compiled,
    v: &'a View,
    occ: Vec<Vec<(usize, usize)>>,
}

impl Matcher<'_> {
    /// 穴の並び `sides` を `[s, e)` に置く。2 つなら読点・空白で分ける (`last_gap` なら最後の区切り)。`{old}`
    /// の穴は読点でも切る (後ろに固定の部分がある穴は最後の読点の後から、末尾の穴は最初の読点の前まで)。
    fn place(
        &self,
        sides: &[Side],
        (s, e): (usize, usize),
        last_gap: bool,
        trailing: bool,
        at_end: bool,
        out: &mut Vec<HoleAt>,
    ) -> Option<()> {
        let v = self.v;
        let mut one =
            |side: Side, s: usize, e: usize, trailing: bool, at_end: bool| -> Option<()> {
                let (mut s, mut e, mut at_end) = (s, e, at_end);
                if side == Side::Old
                    && let Some(j) = v.mark_in(s, e, !trailing, &v.prev_comma, &v.next_comma)
                {
                    if trailing {
                        e = j;
                        at_end = false;
                    } else {
                        s = j;
                    }
                }
                (s < e).then_some(())?;
                out.push(HoleAt {
                    side,
                    range: s..e,
                    trailing,
                    at_end,
                });
                Some(())
            };
        match sides {
            [] => Some(()),
            [side] => one(*side, s, e, trailing, at_end),
            [a, b] => {
                let m = v.mark_in(s, e, last_gap, &v.prev_gap, &v.next_gap)?;
                one(*a, s, m, false, false)?;
                one(*b, m, e, trailing, at_end)
            }
            _ => None,
        }
    }

    /// 最初の錨の前の穴が置けるか (置けなければ、その錨の位置からは当たらない)。穴の端は文で決める。
    fn lead(&self, a0: usize, out: &mut Vec<HoleAt>) -> Option<()> {
        let s = self.v.sentence_start(a0);
        self.place(&self.c.lead, (s, a0), true, false, false, out)
    }

    /// 錨の位置 (始まり, 長さ) から穴を置く。穴のどれかが空なら `None`。
    fn assemble(&self, placed: &[(usize, usize)]) -> Option<Matched> {
        let mut holes = Vec::new();
        let (a0, _) = placed[0];
        self.lead(a0, &mut holes)?;
        for (k, (_, sides)) in self.c.anchors.iter().enumerate() {
            let (a, len) = placed[k];
            let b = a + len;
            match placed.get(k + 1) {
                Some(&(next, _)) => self.place(sides, (b, next), true, false, false, &mut holes)?,
                None => {
                    let e = self.v.sentence_end(b).max(b);
                    self.place(sides, (b, e), false, true, true, &mut holes)?;
                }
            }
        }
        Some(Matched { holes })
    }

    /// 錨 `k` から先を、本文の前から当てる。最初に穴まで置けた一致を返す (すべての組を数え上げない)。錨の間の
    /// 穴は同じ文の中だけなので、次の錨は、表の中で今の位置より後の一致を、文の区切りに当たるまで試す。
    fn search(&self, k: usize, placed: &mut Vec<(usize, usize)>) -> Option<Matched> {
        if k == self.c.anchors.len() {
            return self.assemble(placed);
        }
        let occ = &self.occ[k];
        match k.checked_sub(1) {
            None => {
                for &(a, len) in occ {
                    if self.lead(a, &mut Vec::new()).is_none() {
                        continue;
                    }
                    placed.push((a, len));
                    if let Some(m) = self.search(1, placed) {
                        return Some(m);
                    }
                    placed.pop();
                }
            }
            Some(p) => {
                let (pa, plen) = placed[p];
                let b = pa + plen;
                let holes = !self.c.anchors[p].1.is_empty();
                let from = b + usize::from(holes);
                let i = occ.partition_point(|x| x.0 < from);
                for &(a, len) in &occ[i..] {
                    if (!holes && a != b) || (holes && !self.v.within_sentence(b, a)) {
                        break;
                    }
                    placed.push((a, len));
                    if let Some(m) = self.search(k + 1, placed) {
                        return Some(m);
                    }
                    placed.pop();
                }
            }
        }
        None
    }
}

/// 型の最初の一致 (本文の前から)。穴は、先頭の穴は文の頭から、末尾の穴は文の終わりまで取る (04 §6 の「穴の
/// 範囲」)。
pub(super) fn first_match(c: &Compiled, v: &View) -> Option<Matched> {
    let region = 0..v.len();
    let occ = c
        .anchors
        .iter()
        .map(|(variants, _)| v.occurrences(variants, &region))
        .collect();
    let m = Matcher { c, v, occ };
    m.search(0, &mut Vec::new())
}
