//! 訂正の候補を、区切りの利用者の発話から字句の規則で見つける (04 §6 の「候補」「候補を見つける時機」、v0.5)。
//! I/O と LLM を使わない。
//!
//! - [`find_candidate`] は区切りの発話 1 つ ([`Text`]) から、B2 の要求の組み立て ([`crate::b2::Layout`]) にそのまま
//!   渡せる候補 ([`Candidate`]) を 1 つまで作る。穴の範囲は保存した本文の UTF-8 のバイト位置。
//! - [`find_holes`] は同じ照合で、当たった型と穴の範囲だけを返す。
//! - [`CORRECTION_V1`] は台帳の `inputs` の `correction` に書く版 (04 §5.2.1)。
//!
//! 照合の手順 (04 §6): チャットのセッションの発話の、前のふるいの後の本文 (コードブロックと引用の行を除いたもの)
//! に、本文の文字種から決めた言語 (04 §5.2 の「読み取りの規則」) の一次のふるい (`triggers`) を当て、どれかが
//! 当たれば訂正の型 (`patterns`) を固定の部分の長いものから試す。最初に当たった型の、本文の前からの最初の一致
//! だけを候補にする。型と一次のふるいは、同梱の規則 (`rules/<言語>.toml` の `[correction]`) を、旧エンジンと
//! 共有する規則の読み手 (`rules.rs`) で読む。
//!
//! **依存の規則**: 旧エンジンのモジュール (`facts`・`fold`・`correction`・`note`・`extract`・`table`・`forget`・
//! `mask`・`view`・`gates`) に依存しない (旧エンジンは v0.5 の判定の前に外す、08 §2.6)。v0.4 の `correction.rs` の
//! 照合の部分 (`match_correction`・`find_correction` と、その下の型) を写した (写した元は各関数の注)。
//!
//! 旧エンジンと名前を取り違えないように、クレートの最上位に型を出さず、`corrections::…` で呼ぶ。

mod matcher;

#[cfg(test)]
mod tests;

use std::ops::Range;

use crate::b2::{Candidate, Text};
use crate::lexical::{evidence_language, find_all, word_bounded};
use crate::prefilter::{MODE_CHAT, Prefiltered};
use crate::rules::{fold, language_rules};
use matcher::{Side, View, compile, first_match};

/// 訂正の字句の型の版。台帳の `inputs` の `correction` に書く (04 §5.2.1。版の合わない台帳の `corrections` は
/// 使わない、§5.2.1 の「版の不一致」)。
///
/// 照合の手順 (このモジュール) か、同梱の規則の `[correction]` (全言語の `triggers`・`patterns`) を変えたら上げる。
/// 規則の中身は試験 (`correction_rules_are_pinned`) で固定する。
pub const CORRECTION_V1: u32 = 1;

/// これより長い (前のふるいの後の本文の文字数。正規化の後の、空白と読点を除いた文字数にも当てる) 発話には型を
/// 当てない (暫定、04 §6。長い貼り付けで照合の費用が膨らむため)。v0.4 の `correction::MATCH_MAX_CHARS` と同じ値。
pub const MATCH_MAX_CHARS: usize = 4_000;

/// 当たった訂正の型と穴の範囲 (04 §6 の台帳の `corr` の `pattern`・`new`・`old`)。
///
/// 範囲は、値を比べる範囲 (末尾の穴が文末で終わるときの終わりの「よ」「です」を 1 回だけ除いたもの、04 §6 の
/// 「文末の「よ」「です」」) で、保存した本文の UTF-8 のバイト位置 (書記素の境界)。長さで切らない (古い値の
/// 200 文字の上限は関門 4 で当てる)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holes {
    /// 当たった訂正の型 (同梱の規則の TOML のとおりの文字列)。
    pub pattern: &'static str,
    /// `{new}` の穴の範囲。
    pub new: Range<usize>,
    /// `{old}` の穴の範囲。古い値の穴の無い型 (`I meant {new}`・「正しくは{new}」など) は `None`。
    pub old: Option<Range<usize>>,
}

/// 本文 (前のふるいの後) が型を当てる長さか (正規化の後の長さは [`View::new`] が見る)。
///
/// 写した元: v0.4 の `correction::matchable`。
fn matchable(text: &str) -> bool {
    text.chars().nth(MATCH_MAX_CHARS).is_none()
}

/// 前のふるいの後の本文 `text` に、一次のふるいと訂正の型を当てる。範囲は `text` のバイト位置。
///
/// 写した元: v0.4 の `correction::match_correction` (穴の本文の代わりに範囲を返す。範囲を元の本文に戻せない一致
/// は候補にしない)。
fn match_text(text: &str) -> Option<Holes> {
    if !matchable(text) {
        return None;
    }
    let lang = evidence_language(text);
    let rules = &language_rules(lang)?.correction;
    let folded = fold(text);
    let bounded = word_bounded(lang);
    if !rules
        .triggers
        .iter()
        .any(|t| !find_all(&folded, t, bounded).is_empty())
    {
        return None;
    }
    let view = View::new(text)?;
    for pattern in rules.patterns_longest_first() {
        let Some(compiled) = compile(pattern) else {
            continue;
        };
        let Some(m) = first_match(&compiled, &view) else {
            continue;
        };
        // 最初に当たった型の最初の一致だけを候補にする (04 §6)。穴を元の本文に戻せなければ、ほかの型を試さずに
        // 候補にしない (後の型の一致を、最初に当たった型の代わりに送らないため)。
        let new = m.hole(Side::New)?.source(&view)?;
        let old = match m.hole(Side::Old) {
            Some(h) => Some(h.source(&view)?),
            None => None,
        };
        return Some(Holes { pattern, new, old });
    }
    None
}

/// 保存した発話 `stored` から、訂正の型と穴の範囲 (保存した本文の位置) を見つける (04 §6 の「候補」)。
///
/// チャットのセッション (`mode` が [`MODE_CHAT`]) でなければ探さない (コーディングのセッションと知らないモード)。
/// 前のふるいの後の本文に当てるので、引用の行とコードブロックの中の訂正は候補にならない。前のふるいで番号を
/// 振らない発話 (短い・挨拶・相槌) にも、本文が残れば当てる。1 つの発話に型が 2 つ以上当たっても、最初に当たった
/// 1 つ (固定の部分の長い型から。同じ型なら本文の前から) だけを返す。
///
/// 写した元: v0.4 の `correction::find_correction`。
#[must_use]
pub fn find_holes(mode: &str, stored: &str) -> Option<Holes> {
    if mode != MODE_CHAT {
        return None;
    }
    let filtered = Prefiltered::from_stored(stored);
    if filtered.text().trim().is_empty() {
        return None;
    }
    let holes = match_text(filtered.text())?;
    // 穴は 1 つの文の中 (改行の手前まで) なので、前のふるいで除いた行をまたがない。またぐなら候補にしない。
    let new = filtered.to_stored(holes.new)?;
    let old = match holes.old {
        Some(o) => Some(filtered.to_stored(o)?),
        None => None,
    };
    Some(Holes {
        pattern: holes.pattern,
        new,
        old,
    })
}

/// 区切りの利用者の発話 `utterance` から、訂正の候補を 1 つまで作る (04 §6 の「候補を見つける時機」)。
///
/// `previous` は候補の発話の直前の応答 (同じセッションの、候補の発話より前の最後の応答のターン。無ければ
/// `None`) で、候補にそのまま添える (B2 が `[参照のみ]` の枠に入れる)。`mode` はセッションのモード。
///
/// 忘れるの `[非表示]` の範囲を持つ発話 (`utterance.hidden` が空でない) は候補にしない (04 §6: 忘れるの指示の
/// `hidden` に入るターンの発話は候補にしない。§7.5 の「忘れると訂正」)。忘れるの指示の `turns`・`redactions` に
/// 入るターンも候補にしないので、呼び手はそのターンを渡さない (このモジュールは指示を読まない)。
///
/// 言語は引数に取らない: 型と一次のふるいは、データ根のプロンプトの言語ではなく、本文の文字種から決めた言語の
/// ものを当てる (04 §5.2 の「読み取りの規則」)。
#[must_use]
pub fn find_candidate<'a>(
    mode: &str,
    utterance: Text<'a>,
    previous: Option<Text<'a>>,
) -> Option<Candidate<'a>> {
    if !utterance.hidden.is_empty() {
        return None;
    }
    let holes = find_holes(mode, utterance.text)?;
    Some(Candidate {
        turn: utterance.turn,
        pattern: holes.pattern,
        new: holes.new,
        old: holes.old,
        previous,
    })
}
