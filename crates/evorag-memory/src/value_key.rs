//! 値の鍵と再び覚えない印 (04 §5.6・§7.6)。
//!
//! - 正規化 v1 は **凍結する**: NFKC → 完全な case folding (CaseFolding.txt の C と F) → NFKC →
//!   White_Space の連なりを U+0020 の 1 つにまとめて両端を除く。依存の版を上げて結果が変わったら、
//!   試験 (`all_scalars_are_frozen`) が落ちる。
//! - HMAC-SHA256 は sha2 の上に組む (RFC 2104)。結果は秘密の比較に使わないので、定数時間にしない。
//! - HMAC に入れるバイト列は 04 §5.6 のとおりに凍結する (用途の印・長さを前に置く・値の鍵は生の 32 バイト)。
//! - 照合の形 v1 (04 §5.6) も **凍結する** (忘れるの印に入るため): 元の本文から、正規化 v1 に加えて、語を空白で
//!   区切らない文字 (漢字・かな) に接する空白の連なり (Zs だけのもの) を落とす。試験
//!   (`loose_form_is_frozen`) が落ちたら、正規化 v1 と同じく古い結果を保つ。

use std::cell::OnceCell;
use std::fmt;
use std::ops::Range;

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// 値の鍵の正規化の版 (04 §5.6)。記録の `norm` に書く。
pub const NORM_V1: u32 = 1;

/// 値の鍵の入力の用途の印 (04 §5.6)。
const VK_TAG: &[u8] = b"evorag.vk.1\0";
/// 再び覚えない印の入力の用途の印 (04 §5.6)。
const MARK_TAG: &[u8] = b"evorag.mark.1\0";
/// 自由記述の属性の鍵の入力の用途の印 (04 §5.6、v0.5)。
const ASP_TAG: &[u8] = b"evorag.asp.1\0";

/// 値の鍵の正規化 v1 (04 §5.6)。主語の呼び名にも同じものを当てる (04 §5.1)。
pub fn normalize_v1(s: &str) -> String {
    let nfkc: String = s.nfkc().collect();
    let folded = unicase::UniCase::unicode(nfkc.as_str()).to_folded_case();
    let mut out = String::with_capacity(folded.len());
    let mut pending_space = false;
    for c in folded.nfkc() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

/// 照合の形 v1 で落とす、元の本文の書記素 (`clusters` の番号ごとに、落とすか、04 §5.6)。
///
/// 空白 (White_Space) だけの書記素の連なりのうち、次の 2 つを満たすものを落とす。
/// - 連なりの文字がすべて一般カテゴリ Zs (U+0020・U+3000・NBSP など)。Cc (TAB・改行など)・Zl・Zp を含む連なりは
///   落とさない (行の区切りをまたいで値をつながない)。
/// - 連なりのすぐ前か後ろの書記素の最初の文字が、語を空白で区切らない文字 (`is_unspaced_letter`。関門 4 と同じ)。
///   英数字・ハングル・記号どうしの間の空白は残す ("Ann a" を "Anna" に、"5 kg" を "5kg" に当てない)。
pub(crate) fn dropped_spaces(s: &str, clusters: &[Range<usize>]) -> Vec<bool> {
    let mut out = vec![false; clusters.len()];
    let blank = |k: usize| s[clusters[k].clone()].chars().all(char::is_whitespace);
    let first = |k: usize| s[clusters[k].clone()].chars().next();
    let mut i = 0;
    while i < clusters.len() {
        if !blank(i) {
            i += 1;
            continue;
        }
        let start = i;
        while i < clusters.len() && blank(i) {
            i += 1;
        }
        let only_zs = (start..i).all(|k| {
            s[clusters[k].clone()]
                .chars()
                .all(crate::chars::is_space_separator)
        });
        let before = start.checked_sub(1).and_then(first);
        let after = if i < clusters.len() { first(i) } else { None };
        let next_to_unspaced = [before, after]
            .into_iter()
            .flatten()
            .any(crate::chars::is_unspaced_letter);
        if only_zs && next_to_unspaced {
            out[start..i].fill(true);
        }
    }
    out
}

/// 照合の形 v1 (04 §5.6)。**凍結する**。元の本文から [`dropped_spaces`] の空白を除き、正規化 v1 を当てる。
/// 照合 (関門 1・2・4・5、訂正の検証、伏せる範囲の探索) と、忘れるの印の照合の形の鍵 ([`loose_key`]) に使う。
/// 値の鍵 ([`value_key`]) と主語の記録は正規化 v1 のまま。
pub fn normalize_loose(s: &str) -> String {
    let clusters = crate::chars::graphemes(s);
    let dropped = dropped_spaces(s, &clusters);
    let kept: String = clusters
        .iter()
        .zip(&dropped)
        .filter(|(_, d)| !**d)
        .map(|(r, _)| &s[r.clone()])
        .collect();
    normalize_v1(&kept)
}

/// 書記素 1 つを正規化 v1 の手順 (NFKC → case folding → NFKC) で直したもの (空白の連なりはまとめない)。
pub(crate) fn fold_cluster(cluster: &str) -> String {
    let nfkc: String = cluster.nfkc().collect();
    let folded = unicase::UniCase::unicode(nfkc.as_str()).to_folded_case();
    folded.nfkc().collect()
}

/// 正規化 v1 (か照合の形) の結果と、元の本文の位置との対応表 (04 §5.6)。
///
/// 元の本文を書記素ごとに正規化してつなぎ、出力の文字ごとに、どの書記素から出たかを持つ。照合の一致は、
/// 両端が書記素の展開の切れ目に来たものだけを採る (一致の端が 1 つの文字の展開の途中なら一致としない)。照合の
/// 形で落とした空白は出力に何も出さないので、一致の端が落とした空白に来ることはない。
#[derive(Debug, Clone)]
pub(crate) struct Mapped {
    text: String,
    /// 出力の文字ごとの (出力のバイト位置, 元の書記素の番号)。バイト位置の昇順。
    owners: Vec<(usize, usize)>,
    /// 元の本文の書記素の範囲。
    clusters: Vec<Range<usize>>,
}

/// 照合の一致 1 つ。`out` は正規化の後の位置、`source` は元の本文の位置 (書記素の境界)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hit {
    pub(crate) out: Range<usize>,
    pub(crate) source: Range<usize>,
}

/// 一致の端の条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edges {
    /// 部分一致。
    Any,
    /// 英数字で始まる・終わる探す語は、前・後が英数字でない位置だけ (04 §5.2 の関門 2)。
    Word,
}

impl Mapped {
    /// 対応表を作る。書記素ごとに正規化した結果が、本文の全体を正規化した結果 (`normalize_v1`) と
    /// 食い違うとき (書記素をまたぐ合成など) は `None` (位置を戻せないので、正規化の後の照合をしない)。
    pub(crate) fn new(s: &str) -> Option<Self> {
        let clusters = crate::chars::graphemes(s);
        let (text, owners) = Self::build(s, &clusters, None);
        (text == normalize_v1(s)).then_some(Self {
            text,
            owners,
            clusters,
        })
    }

    /// 照合の形 v1 ([`normalize_loose`]) の対応表を作る。書記素ごとの結果が [`normalize_loose`] と食い違うときは
    /// `None`。
    pub(crate) fn loose(s: &str) -> Option<Self> {
        let clusters = crate::chars::graphemes(s);
        let dropped = dropped_spaces(s, &clusters);
        let (text, owners) = Self::build(s, &clusters, Some(&dropped));
        (text == normalize_loose(s)).then_some(Self {
            text,
            owners,
            clusters,
        })
    }

    /// 書記素ごとに正規化してつなぐ。`dropped` の書記素は出力に何も出さない。
    fn build(
        s: &str,
        clusters: &[Range<usize>],
        dropped: Option<&[bool]>,
    ) -> (String, Vec<(usize, usize)>) {
        let mut text = String::with_capacity(s.len());
        let mut owners = Vec::new();
        let mut pending_space: Option<usize> = None;
        for (ci, r) in clusters.iter().enumerate() {
            if dropped.is_some_and(|d| d[ci]) {
                continue;
            }
            for c in fold_cluster(&s[r.clone()]).chars() {
                if c.is_whitespace() {
                    if !text.is_empty() && pending_space.is_none() {
                        pending_space = Some(ci);
                    }
                } else {
                    if let Some(owner) = pending_space.take() {
                        owners.push((text.len(), owner));
                        text.push(' ');
                    }
                    owners.push((text.len(), ci));
                    text.push(c);
                }
            }
        }
        (text, owners)
    }

    /// 正規化の後の本文 (`normalize_v1` と同じ)。
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// `needle` (正規化 v1 の後の文字列) の一致を、重なりを含めて前から順に返す。
    pub(crate) fn find(&self, needle: &str, edges: Edges) -> Vec<Hit> {
        let mut hits = Vec::new();
        let Some(first) = needle.chars().next() else {
            return hits;
        };
        let last = needle.chars().next_back().unwrap_or(first);
        let mut from = 0;
        while let Some(p) = self.text[from..].find(needle) {
            let at = from + p;
            let end = at + needle.len();
            from = at + first.len_utf8();
            if edges == Edges::Word {
                let before = self.text[..at].chars().next_back();
                let after = self.text[end..].chars().next();
                let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
                if (first.is_ascii_alphanumeric() && word(before))
                    || (last.is_ascii_alphanumeric() && word(after))
                {
                    continue;
                }
            }
            if let Some(source) = self.source_of(at, end) {
                hits.push(Hit {
                    out: at..end,
                    source,
                });
            }
        }
        hits
    }

    /// 伏せるための一致 (04 §7.5)。[`Mapped::find`] と同じ端の条件で探し、一致の端が 1 つの文字の展開の途中に
    /// 来たとき (`㈱` の中の `株`、ZWJ の並びの中の 1 文字など) は、捨てずに、その展開を含む書記素まで広げて返す
    /// (伏せる側に倒す。照合の規則は変えない)。元の本文の範囲を返す。
    pub(crate) fn find_widened(&self, needle: &str, edges: Edges) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let Some(first) = needle.chars().next() else {
            return out;
        };
        let last = needle.chars().next_back().unwrap_or(first);
        let mut from = 0;
        while let Some(p) = self.text[from..].find(needle) {
            let at = from + p;
            let end = at + needle.len();
            from = at + first.len_utf8();
            if edges == Edges::Word {
                let before = self.text[..at].chars().next_back();
                let after = self.text[end..].chars().next();
                let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
                if (first.is_ascii_alphanumeric() && word(before))
                    || (last.is_ascii_alphanumeric() && word(after))
                {
                    continue;
                }
            }
            out.push(self.source_of(at, end).unwrap_or_else(|| {
                self.cluster_of(at).start..self.cluster_of(end - last.len_utf8()).end
            }));
        }
        out
    }

    /// 正規化の後の本文のバイト位置 `out` の文字が出た、元の本文の書記素の範囲。
    pub(crate) fn cluster_of(&self, out: usize) -> Range<usize> {
        let i = self
            .owners
            .partition_point(|o| o.0 <= out)
            .saturating_sub(1);
        self.owners
            .get(i)
            .map_or(0..0, |o| self.clusters[o.1].clone())
    }

    /// 元の本文の範囲 `source` (書記素の境界) に当たる、正規化の後の本文の範囲。`source` の両端が、出力に文字を
    /// 出した書記素の端に来なければ `None` (照合の形の一致の位置を、正規化 v1 の位置に直すのに使う)。
    pub(crate) fn out_of_source(&self, source: &Range<usize>) -> Option<Range<usize>> {
        let cluster = |o: &(usize, usize)| &self.clusters[o.1];
        let i = self
            .owners
            .partition_point(|o| cluster(o).start < source.start);
        let j = self
            .owners
            .partition_point(|o| cluster(o).end <= source.end)
            .checked_sub(1)?;
        let (first, last) = (self.owners.get(i)?, self.owners.get(j)?);
        if i > j || cluster(first).start != source.start || cluster(last).end != source.end {
            return None;
        }
        let end = self.owners.get(j + 1).map_or(self.text.len(), |o| o.0);
        Some(first.0..end)
    }

    /// 出力の範囲 `[start, end)` を元の本文の範囲に戻す。端が書記素の展開の途中なら `None`。
    pub(crate) fn source_of(&self, start: usize, end: usize) -> Option<Range<usize>> {
        let i = self.owners.binary_search_by_key(&start, |o| o.0).ok()?;
        let j = self.owners.partition_point(|o| o.0 < end).checked_sub(1)?;
        let (owner_i, owner_j) = (self.owners[i].1, self.owners[j].1);
        let starts_clean = i == 0 || self.owners[i - 1].1 != owner_i;
        let ends_clean = self.owners.get(j + 1).is_none_or(|o| o.1 != owner_j);
        (starts_clean && ends_clean)
            .then(|| self.clusters[owner_i].start..self.clusters[owner_j].end)
    }
}

/// 探す文字列の、正規化 v1 の形と照合の形 (04 §5.6)。どちらも元の文字列から作る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Needle {
    pub(crate) v1: String,
    pub(crate) loose: String,
}

impl Needle {
    pub(crate) fn new(s: &str) -> Self {
        Self {
            v1: normalize_v1(s),
            loose: normalize_loose(s),
        }
    }

    /// 正規化すると空 (探さない)。
    pub(crate) fn is_empty(&self) -> bool {
        self.v1.is_empty() || self.loose.is_empty()
    }
}

/// 探す相手の本文と、その正規化 v1 と照合の形の対応表 (照合の形は要るときだけ作る)。照合は正規化 v1 → 照合の形
/// の順に行い、照合の形は正規化 v1 で 1 つも当たらないときだけ使う (04 §5.6)。
#[derive(Debug)]
pub(crate) struct Haystack<'t> {
    text: &'t str,
    v1: Option<Mapped>,
    loose: OnceCell<Option<Mapped>>,
}

/// 本文の中の一致をすべて探した結果 ([`Haystack::find_all`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Placed {
    /// 一致する元の本文の範囲 (空なら一致なし)。
    Ranges(Vec<Range<usize>>),
    /// 正規化 v1 の一致の範囲は戻せたが、照合の形の対応表を作れず、照合の形の本文には値がある (照合の形の一致の
    /// 位置を戻せない)。正規化 v1 の範囲は捨てずに持つ。呼び手は伏せる側に倒す (本文の全体や根拠の範囲の全体を
    /// 伏せる・伏せ字を中止する)。
    Partial(Vec<Range<usize>>),
    /// 正規化した本文に値はあるが、対応表を作れず位置を戻せない。
    Unplaceable,
}

impl<'t> Haystack<'t> {
    pub(crate) fn new(text: &'t str) -> Self {
        Self {
            text,
            v1: Mapped::new(text),
            loose: OnceCell::new(),
        }
    }

    /// 元の本文。
    pub(crate) fn text(&self) -> &'t str {
        self.text
    }

    /// 正規化 v1 の対応表。
    pub(crate) fn v1(&self) -> Option<&Mapped> {
        self.v1.as_ref()
    }

    /// 照合の形の対応表。
    pub(crate) fn loose(&self) -> Option<&Mapped> {
        self.loose.get_or_init(|| Mapped::loose(self.text)).as_ref()
    }

    /// `needle` の一致 (`Hit::source` は元の本文の位置。`Hit::out` は当たった段の正規化の後の位置)。正規化 v1 で
    /// `keep` を満たす一致が無いときだけ照合の形で探す。
    pub(crate) fn find_where(
        &self,
        needle: &Needle,
        edges: Edges,
        keep: impl Fn(&Hit) -> bool,
    ) -> Vec<Hit> {
        if needle.is_empty() {
            return Vec::new();
        }
        let v1: Vec<Hit> = self.v1().map_or_else(Vec::new, |m| {
            m.find(&needle.v1, edges)
                .into_iter()
                .filter(&keep)
                .collect()
        });
        if !v1.is_empty() {
            return v1;
        }
        self.loose().map_or_else(Vec::new, |m| {
            m.find(&needle.loose, edges)
                .into_iter()
                .filter(&keep)
                .collect()
        })
    }

    /// `needle` の一致 ([`Self::find_where`] のすべて)。
    pub(crate) fn find(&self, needle: &Needle, edges: Edges) -> Vec<Hit> {
        self.find_where(needle, edges, |_| true)
    }

    /// 本文の中の `needle` の一致をすべて探す (伏せ字・`[非表示]` の範囲、04 §5.6 の「照合の順」・§7.3)。正規化
    /// v1 の一致を先に採り、照合の形では、それと重ならない一致だけを足す (元の本文の位置、前から順)。`widen` なら
    /// 端が文字の展開の途中に来た一致も、その書記素まで広げて採る ([`Mapped::find_widened`])。正規化 v1 の対応表を
    /// 作れず、正規化した本文に値が含まれるときは `Unplaceable`。照合の形の対応表だけを作れず、照合の形の本文に
    /// 値が含まれるときは、正規化 v1 の範囲を持つ `Partial`。
    pub(crate) fn find_all(&self, needle: &Needle, edges: Edges, widen: Widen) -> Placed {
        if needle.is_empty() {
            return Placed::Ranges(Vec::new());
        }
        let search = |m: &Mapped, n: &str| -> Vec<Range<usize>> {
            match widen {
                Widen::No => m.find(n, edges).into_iter().map(|h| h.source).collect(),
                Widen::ToGraphemes => m.find_widened(n, edges),
            }
        };
        let mut out = match self.v1() {
            Some(m) => search(m, &needle.v1),
            None if normalize_v1(self.text).contains(&needle.v1) => return Placed::Unplaceable,
            None => Vec::new(),
        };
        match self.loose() {
            Some(m) => {
                let extra: Vec<Range<usize>> = search(m, &needle.loose)
                    .into_iter()
                    .filter(|r| !out.iter().any(|x| x.start < r.end && r.start < x.end))
                    .collect();
                out.extend(extra);
            }
            None if normalize_loose(self.text).contains(&needle.loose) => {
                out.sort_by_key(|r| (r.start, r.end));
                return Placed::Partial(out);
            }
            None => {}
        }
        out.sort_by_key(|r| (r.start, r.end));
        Placed::Ranges(out)
    }
}

/// [`Haystack::find_all`] で、端が文字の展開の途中に来た一致を広げるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Widen {
    /// 広げない (一致としない)。
    No,
    /// その書記素まで広げて採る (伏せる側に倒す)。
    ToGraphemes,
}

/// データ根ごとの HMAC の鍵 (`meta` の `memory.forget_key` を復号した 32 バイト、03 §3.1.1)。
#[derive(Clone, PartialEq, Eq)]
pub struct ForgetKey([u8; 32]);

impl ForgetKey {
    pub fn from_bytes(b: [u8; 32]) -> Self {
        Self(b)
    }

    /// 試験で `const` に置くための形。
    #[cfg(test)]
    pub(crate) const fn from_bytes_const(b: [u8; 32]) -> Self {
        Self(b)
    }

    /// `meta` の値 (ちょうど 64 桁の小文字の 16 進) から読む。形が違えば `None`。
    pub fn from_hex(s: &str) -> Option<Self> {
        decode_hex32(s).map(Self)
    }

    /// `meta` に書く形 (64 桁の小文字の 16 進)。
    pub fn to_hex(&self) -> String {
        encode_hex(&self.0)
    }
}

impl fmt::Debug for ForgetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 秘密なので中身を出さない。
        f.write_str("ForgetKey(..)")
    }
}

/// 値の鍵 (04 §5.6)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueKey(pub [u8; 32]);

/// 再び覚えない印 (04 §7.6)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockMark(pub [u8; 32]);

macro_rules! hex32 {
    ($t:ident) => {
        impl $t {
            /// 記録に書く形 (64 桁の小文字の 16 進)。
            pub fn to_hex(&self) -> String {
                encode_hex(&self.0)
            }

            /// 記録の形から読む。形が違えば `None`。
            pub fn from_hex(s: &str) -> Option<Self> {
                decode_hex32(s).map(Self)
            }
        }

        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({}…)", stringify!($t), &self.to_hex()[..8])
            }
        }
    };
}
hex32!(ValueKey);
hex32!(BlockMark);

/// 値の鍵: HMAC-SHA256(K, "evorag.vk.1" ‖ 0x00 ‖ UTF-8 (正規化 v1 (値)))。
pub fn value_key(key: &ForgetKey, value: &str) -> ValueKey {
    key_of(key, &normalize_v1(value))
}

/// 照合の形の鍵: HMAC-SHA256(K, "evorag.vk.1" ‖ 0x00 ‖ UTF-8 (照合の形 v1 (値)))。値の鍵と同じ形の入力で、
/// 正規化 v1 の代わりに照合の形 ([`normalize_loose`]) を入れる (落とす空白の無い値では、値の鍵と同じになる)。
/// 忘れるの指示の印と `vks` に、値の鍵と並べて入れ、関門 8 と訂正の「忘れた値」の検査で値の鍵と両方を引く
/// (04 §5.6・§7.6)。
pub fn loose_key(key: &ForgetKey, value: &str) -> ValueKey {
    key_of(key, &normalize_loose(value))
}

/// 正規化した値 `n` の鍵。
fn key_of(key: &ForgetKey, n: &str) -> ValueKey {
    let mut msg = Vec::with_capacity(VK_TAG.len() + n.len());
    msg.extend_from_slice(VK_TAG);
    msg.extend_from_slice(n.as_bytes());
    ValueKey(hmac_sha256(&key.0, &msg))
}

/// 自由記述の属性の鍵 (v0.5): HMAC-SHA256(K, "evorag.asp.1" ‖ 0x00 ‖ UTF-8 (正規化 v1 (属性)))。指示の `aspect_mac`
/// に、自由記述の属性の文字列の代わりに 16 進で書く (属性の名前が値に近い内容を含みうるため、04 §5.6)。
#[must_use]
pub fn aspect_key(key: &ForgetKey, aspect: &str) -> ValueKey {
    let n = normalize_v1(aspect);
    let mut msg = Vec::with_capacity(ASP_TAG.len() + n.len());
    msg.extend_from_slice(ASP_TAG);
    msg.extend_from_slice(n.as_bytes());
    ValueKey(hmac_sha256(&key.0, &msg))
}

/// 再び覚えない印: HMAC-SHA256(K, "evorag.mark.1" ‖ 0x00 ‖ u32be(len S) ‖ S ‖ u32be(len A) ‖ A ‖ vk)。
/// S は正規化 v1 の後の主語、A は属性の名前。
pub fn block_mark(key: &ForgetKey, subject: &str, attribute: &str, vk: &ValueKey) -> BlockMark {
    let s = normalize_v1(subject);
    let mut msg = Vec::with_capacity(MARK_TAG.len() + 8 + s.len() + attribute.len() + 32);
    msg.extend_from_slice(MARK_TAG);
    push_with_len(&mut msg, s.as_bytes());
    push_with_len(&mut msg, attribute.as_bytes());
    msg.extend_from_slice(&vk.0);
    BlockMark(hmac_sha256(&key.0, &msg))
}

fn push_with_len(out: &mut Vec<u8>, b: &[u8]) {
    let len = u32::try_from(b.len()).expect("a field is shorter than 4 GiB");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(b);
}

/// 値の鍵 ([`value_key`] と同じ値) を、正規化した後の文字列を少しずつ足しながら作る口 (部分文字列の鍵、
/// `apply::keys`)。鍵の内と外の詰め物と用途の印を 1 回だけ吸わせておき、[`KeyStream::start`] の写しに正規化した
/// 文字列のバイト列を足して [`KeyStream::finish`] で値の鍵にする (前に足した分を吸わせ直さない)。
#[derive(Clone)]
pub(crate) struct KeyStream {
    inner: Sha256,
    outer: Sha256,
}

impl KeyStream {
    pub(crate) fn new(key: &ForgetKey) -> Self {
        let k0 = hmac_block(&key.0);
        let mut inner = Sha256::new();
        inner.update(k0.map(|b| b ^ 0x36));
        inner.update(VK_TAG);
        let mut outer = Sha256::new();
        outer.update(k0.map(|b| b ^ 0x5c));
        Self { inner, outer }
    }

    /// 正規化した文字列を足していく状態 (まだ何も足していない)。
    pub(crate) fn start(&self) -> Sha256 {
        self.inner.clone()
    }

    /// 足し終えた状態 `msg` から値の鍵を作る (`msg` は写しを使う)。
    pub(crate) fn finish(&self, msg: &Sha256) -> ValueKey {
        let inner = msg.clone().finalize();
        let mut outer = self.outer.clone();
        outer.update(inner);
        ValueKey(outer.finalize().into())
    }
}

impl fmt::Debug for KeyStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyStream(..)")
    }
}

/// HMAC の鍵を 1 ブロックにしたもの (RFC 2104)。
fn hmac_block(key: &[u8]) -> [u8; 64] {
    let mut k0 = [0u8; 64];
    if key.len() > 64 {
        k0[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k0[..key.len()].copy_from_slice(key);
    }
    k0
}

/// HMAC-SHA256 (RFC 2104)。
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let k0 = hmac_block(key);
    let mut inner = Sha256::new();
    inner.update(k0.map(|b| b ^ 0x36));
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k0.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

fn encode_hex(b: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(char::from(DIGITS[usize::from(x >> 4)]));
        s.push(char::from(DIGITS[usize::from(x & 0xf)]));
    }
    s
}

fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, pair) in b.as_chunks::<2>().0.iter().enumerate() {
        out[i] = nib(pair[0])? << 4 | nib(pair[1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        encode_hex(b)
    }

    #[test]
    fn the_key_stream_matches_value_keys() {
        let key = ForgetKey::from_bytes([2; 32]);
        let ks = KeyStream::new(&key);
        let mut h = ks.start();
        let mut text = String::new();
        for part in ["霧", "ヶ丘", " の", "朝市"] {
            h.update(part.as_bytes());
            text.push_str(part);
            assert_eq!(ks.finish(&h), value_key(&key, &text), "{text}");
        }
        assert_eq!(ks.finish(&ks.start()), value_key(&key, ""));
    }

    /// verifies: 04 §5.6 (自由記述の属性の鍵: "evorag.asp.1" ‖ 0x00 ‖ 正規化 v1 (属性) の HMAC)
    #[test]
    fn aspect_keys_are_normalized_and_separated_from_value_keys() {
        let key = ForgetKey::from_bytes([1; 32]);
        let mut msg = b"evorag.asp.1\0".to_vec();
        msg.extend_from_slice("よく行く店".as_bytes());
        assert_eq!(
            aspect_key(&key, "よく行く店").0,
            hmac_sha256(&[1; 32], &msg)
        );
        assert_eq!(
            aspect_key(&key, " よく行く店 "),
            aspect_key(&key, "よく行く店")
        );
        assert_ne!(
            aspect_key(&key, "よく行く店"),
            value_key(&key, "よく行く店")
        );
    }

    #[test]
    fn hmac_matches_rfc4231() {
        // RFC 4231 の試験の値 1・2・3・6。
        let cases: [(Vec<u8>, &[u8], &str); 4] = [
            (
                vec![0x0b; 20],
                b"Hi There",
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?",
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 20],
                &[0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
            (
                vec![0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First",
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
        ];
        for (key, msg, want) in cases {
            assert_eq!(hex(&hmac_sha256(&key, msg)), want);
        }
    }

    #[test]
    fn normalization_v1_examples() {
        let cases = [
            ("Tokyo", "tokyo"),
            ("TOKYO", "tokyo"),
            ("Straße", "strasse"),
            ("ΣΊΣΥΦΟΣ", "σίσυφοσ"),
            ("\u{212A}", "k"), // ケルビンの記号
            ("\u{0130}", "i\u{0307}"),
            ("ｶﾞｽ", "ガス"),
            ("㈱", "(株)"),
            ("  東京 \u{3000}タワー\t ", "東京 タワー"),
            ("C++", "c++"),
            ("C#", "c#"),
            ("とうきょう", "とうきょう"),
            ("トウキョウ", "トウキョウ"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_v1(input), want, "{input:?}");
        }
    }

    /// 凍結の試験 (04 §5.6): すべての Unicode のスカラー値を 1 文字ずつ正規化した結果のハッシュ。
    /// 依存 (unicode-normalization・unicase) や toolchain を上げて変わったら、ここが落ちる。
    /// そのときは値を書き換えず、古い結果を例外の表で保つ。
    #[test]
    fn all_scalars_are_frozen_and_idempotent() {
        let mut h = Sha256::new();
        let mut not_idempotent = Vec::new();
        for c in (0..=0x10FFFFu32).filter_map(char::from_u32) {
            let n = normalize_v1(c.encode_utf8(&mut [0; 4]));
            if normalize_v1(&n) != n {
                not_idempotent.push(c);
            }
            h.update(n.as_bytes());
            h.update([0xff]);
        }
        assert_eq!(
            not_idempotent,
            Vec::<char>::new(),
            "normalization v1 must be idempotent"
        );
        assert_eq!(
            hex(&h.finalize()),
            "6f28fb0aa86180219e458061d18ed859764dac233ab3bd82ff74987d482669dd"
        );
    }

    #[test]
    fn keys_and_marks_are_stable_and_separated() {
        let k = ForgetKey::from_bytes([7; 32]);
        let vk = value_key(&k, "Tokyo");
        // 正規化で同じになる表記は同じ鍵になる。
        assert_eq!(vk, value_key(&k, "  TOKYO "));
        assert_ne!(vk, value_key(&k, "Osaka"));
        // 鍵が違えば違う。
        assert_ne!(vk, value_key(&ForgetKey::from_bytes([8; 32]), "Tokyo"));
        // 印は主語と属性で分かれる。
        let m = block_mark(&k, "self", "residence", &vk);
        assert_ne!(m, block_mark(&k, "self", "hometown", &vk));
        assert_ne!(m, block_mark(&k, "妻", "residence", &vk));
        // 長さを前に置くので、主語と属性の境目をずらしても同じにならない。
        assert_ne!(
            block_mark(&k, "ab", "c", &vk),
            block_mark(&k, "a", "bc", &vk)
        );
        // 凍結した値 (バイト列の形を変えたら落ちる)。
        assert_eq!(
            vk.to_hex(),
            "89b30952b8f4e57555096033a90a912328ac61d7c9bb249861c2b5eb188dfa54"
        );
        assert_eq!(
            m.to_hex(),
            "ddabaab93d1cb62796502f31b8b14b744af0f4030ab961f84aa13c506772c77b"
        );
    }

    #[test]
    fn hex_round_trips_and_rejects_bad_input() {
        let k = ForgetKey::from_bytes([0xab; 32]);
        assert_eq!(ForgetKey::from_hex(&k.to_hex()), Some(k.clone()));
        assert_eq!(k.to_hex().len(), 64);
        assert!(ForgetKey::from_hex("AB").is_none());
        assert!(ForgetKey::from_hex(&"AB".repeat(32)).is_none()); // 大文字は受けない
        assert!(ForgetKey::from_hex(&"zz".repeat(32)).is_none());
        assert_eq!(format!("{k:?}"), "ForgetKey(..)");
    }

    fn sources<'t>(text: &'t str, needle: &str, edges: Edges) -> Vec<&'t str> {
        let m = Mapped::new(text).expect("consistent");
        assert_eq!(m.text(), normalize_v1(text));
        m.find(needle, edges)
            .into_iter()
            .map(|h| &text[h.source])
            .collect()
    }

    #[test]
    fn mapped_positions_return_to_the_original_text() {
        assert_eq!(
            sources("ＲＴＸ４０７０を使う", "rtx4070", Edges::Any),
            ["ＲＴＸ４０７０"]
        );
        assert_eq!(sources("ｶﾞｽと ガス", "ガス", Edges::Any), ["ｶﾞｽ", "ガス"]);
        assert_eq!(
            sources("東京 \u{3000} タワー", "東京 タワー", Edges::Any),
            ["東京 \u{3000} タワー"]
        );
        // 一致の端が 1 つの文字の展開の途中なら一致としない (`㈱` → `(株)`)。
        assert!(sources("㈱山田", "株", Edges::Any).is_empty());
        assert_eq!(sources("㈱山田", "(株)", Edges::Any), ["㈱"]);
        // 英数字は語の境界で。記号で終わる語は後ろを見ない。
        assert!(sources("Annabelle", "ann", Edges::Word).is_empty());
        assert_eq!(sources("Ann and Annabelle", "ann", Edges::Word), ["Ann"]);
        assert_eq!(sources("C++er", "c++", Edges::Word), ["C++"]);
        assert_eq!(sources("5歳の娘", "5歳の娘", Edges::Word), ["5歳の娘"]);
        // 重なる一致も返す。
        assert_eq!(sources("aaa", "aa", Edges::Any), ["aa", "aa"]);
    }

    #[test]
    fn loose_form_examples() {
        let cases = [
            // 漢字・かなに接する空白 (Zs) は除く。
            ("7 月 22 日", "7月22日"),
            ("7月22日", "7月22日"),
            ("RTX4070 を使う", "rtx4070を使う"),
            ("田中 さん", "田中さん"),
            ("東京\u{3000}タワー", "東京タワー"),
            ("東京 \u{00A0} タワー", "東京タワー"),
            ("ｶﾞｽ と 水", "ガスと水"),
            ("長音 ー x", "長音ーx"),
            // 英数字・ハングル・記号どうしの間の空白は残す。
            ("Ann a", "ann a"),
            ("5 kg", "5 kg"),
            ("서울 시", "서울 시"),
            ("C++ と C#", "c++とc#"),
            ("a - b", "a - b"),
            // Cc・Zl・Zp を含む空白の連なりは除かない (1 つの空白にまとめる)。
            ("東京\nタワー", "東京 タワー"),
            ("東京 \t タワー", "東京 タワー"),
            ("東京\u{2028}タワー", "東京 タワー"),
            ("東京\r\nタワー", "東京 タワー"),
            // 両端は正規化 v1 と同じく除く。
            ("  東京 ", "東京"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_loose(input), want, "{input:?}");
        }
    }

    /// 凍結の試験 (04 §5.6 の照合の形 v1): 空白を除くかの判定 (語を空白で区切らない文字と Zs) と正規化 v1 を、
    /// すべての Unicode のスカラー値で固定する。依存 (unicode-script・unicode-properties・unicode-segmentation
    /// など) を上げて変わったら、ここが落ちる。そのときは値を書き換えず、古い結果を例外の表で保つ。
    #[test]
    fn loose_form_is_frozen() {
        let mut h = Sha256::new();
        for c in (0..=0x10FFFFu32).filter_map(char::from_u32) {
            // 前後の文字が `c` のときに空白を除くか。
            let n = normalize_loose(&format!("{c} {c}"));
            h.update(n.as_bytes());
            h.update([0xff]);
            // 空白の連なりが `c` だけのときに除くか。
            if c.is_whitespace() {
                let n = normalize_loose(&format!("あ{c}あ"));
                h.update(n.as_bytes());
                h.update([0xfe]);
            }
        }
        assert_eq!(
            hex(&h.finalize()),
            "2590f68c2590a96dccfdb9980bb4090f29d0b954a530a8fabdd1237d9a60d56b"
        );
    }

    #[test]
    fn loose_keys_follow_the_loose_form() {
        let k = ForgetKey::from_bytes([7; 32]);
        // 照合の鍵は、照合の形が同じなら同じ。落とす空白の無い値では値の鍵と同じ。
        assert_eq!(loose_key(&k, "7 月 22 日"), loose_key(&k, "7月22日"));
        assert_eq!(loose_key(&k, "7 月 22 日"), value_key(&k, "7月22日"));
        assert_ne!(value_key(&k, "7 月 22 日"), value_key(&k, "7月22日"));
        assert_ne!(loose_key(&k, "5 kg"), loose_key(&k, "5kg"));
        assert_eq!(loose_key(&k, "Tokyo"), value_key(&k, "Tokyo"));
    }

    fn loose_sources<'t>(text: &'t str, needle: &str, edges: Edges) -> Vec<&'t str> {
        let m = Mapped::loose(text).expect("consistent");
        assert_eq!(m.text(), normalize_loose(text));
        m.find(&normalize_loose(needle), edges)
            .into_iter()
            .map(|h| &text[h.source])
            .collect()
    }

    #[test]
    fn loose_positions_return_to_the_original_text() {
        // 一致の範囲は、最初の文字の始まりから最後の文字の終わりまで (除いた空白で始まり・終わらない)。
        assert_eq!(
            loose_sources("誕生日は 7 月 22 日 です", "7月22日", Edges::Any),
            ["7 月 22 日"]
        );
        assert_eq!(
            loose_sources("誕生日は7月22日です", "7 月 22 日", Edges::Word),
            ["7月22日"]
        );
        // 英数字どうしの空白は残るので、"Ann a" は "Anna" に当たらない。
        assert!(loose_sources("Ann a", "anna", Edges::Word).is_empty());
        assert!(loose_sources("5 kg です", "5kg", Edges::Word).is_empty());
        // 改行をまたいでは当たらない。
        assert!(loose_sources("東京\nタワー", "東京タワー", Edges::Any).is_empty());
        // 語の境界は照合の形の本文で見る。
        assert!(loose_sources("RTX40701 を", "rtx4070", Edges::Word).is_empty());
    }

    #[test]
    fn loose_hits_map_back_to_v1_positions() {
        let text = "誕生日は 7 月 22 日です";
        let v1 = Mapped::new(text).expect("v1");
        let loose = Mapped::loose(text).expect("loose");
        let hit = loose
            .find("7月22日", Edges::Word)
            .pop()
            .expect("a loose hit");
        let out = v1.out_of_source(&hit.source).expect("v1 range");
        assert_eq!(&v1.text()[out], "7 月 22 日");
        // 文字の途中に来る範囲は直せない。
        let start = text.find('月').expect("月");
        assert!(v1.out_of_source(&(start + 1..start + 3)).is_none());
    }

    #[test]
    fn haystacks_try_v1_before_the_loose_form() {
        let text = "7月22日と7 月 22 日";
        let hay = Haystack::new(text);
        // 照合の順 (関門): 正規化 v1 で当たれば照合の形は使わない。
        let hits = hay.find(&Needle::new("7月22日"), Edges::Word);
        let got: Vec<&str> = hits.iter().map(|h| &text[h.source.clone()]).collect();
        assert_eq!(got, ["7月22日"]);
        // すべてを探すとき (伏せる範囲): 正規化 v1 の一致に、重ならない照合の形の一致を足す。
        let Placed::Ranges(all) = hay.find_all(&Needle::new("7月22日"), Edges::Word, Widen::No)
        else {
            panic!("placeable");
        };
        let got: Vec<&str> = all.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(got, ["7月22日", "7 月 22 日"]);
        // 正規化 v1 で当たらなければ照合の形で探す。
        assert_eq!(hay.find(&Needle::new("7 月22日"), Edges::Word).len(), 2);
    }
}
