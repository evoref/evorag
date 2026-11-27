//! 関門 (B3) の純粋な関数 (04 §5.2 の「属性の決め方」と関門 1〜8、08 §2.6 の M5.2 の「関門の移し替え」)。
//!
//! 台帳 (§5.2.1) の単位の候補 1 つ ([`LedgerUnit`]、読むときの型) に、属性の決め方 → 関門 1・2・3・3a・3b・4・5・6・
//! 6a・7・8 の順に当て、通れば記録する形 ([`Passed`]) を、外れれば最初に外れた関門の理由 ([`Reason`]) を返す。
//! I/O・LLM・書き込みを持たず、同じ入力から同じ結果を返す。
//!
//! - [`screen`]: 属性の決め方と関門 1〜7。結果は、候補の要素・根拠の文の位置と `rev`・`[非表示]` の範囲・規則と
//!   辞書の版 ([`cache_key`]) だけで決まるので、導出 (B4) はこの結果を覚えてよい (04 §5.3)。
//! - [`forgotten`]: 関門 8 (忘れるの指示)。当て方の述語は `apply` の [`DirectiveSet::forgotten_by`] に任せる。
//! - [`check()`]: 関門 1〜7 と関門 8 を順に当てる (通れば導出に渡す形)。
//! - [`check_text`]: 関門 6 だけ (検索の鍵の検査、§5.5)。
//!
//! 位置は保存した本文の UTF-8 のバイト位置で、書記素の境界に合わせる (04 §5.1 の `anchors`)。値・主語・`old`・
//! `when` の照合と、括弧・問いの文・手がかりの語・節は、前のふるいの後の本文 (今の規則で除いた範囲を持たない) で
//! 見る。
//!
//! **依存の規則**: このモジュールは、旧エンジンのモジュール (`facts`・`fold`・`correction`・`note`・`extract`・
//! `table`・`forget`・`mask`・`view`・`gates`) に依存しない (旧エンジンは v0.5 の判定の前に外す、08 §2.6)。v0.4 の
//! 関門 (`gates.rs`) の純粋な部分は、このモジュールに写した (写した元を注に書く)。
//!
//! 旧エンジンの型と取り違えないように、クレートの最上位に型を出さず、`b3::…` で呼ぶ。

mod place;
mod rules;
mod text;

#[cfg(test)]
mod cases;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Range;

use evorag_types::Id;

use crate::apply::{DirectiveSet, Probe, ProbeAspect, ResolvedEntity, ValueKeys};
use crate::chars::{grapheme_boundaries, is_separator};
use crate::ledger::{ByteRange, LedgerLayer, LedgerMood, LedgerTense, LedgerUnit, SentenceSpan};
use crate::lexical::{Signals, signals};
use crate::prefilter::Prefiltered;
use crate::unit::{UnitLayer, UnitTense};
use crate::value_key::{Edges, ForgetKey, Haystack, Hit, Needle, aspect_key, normalize_v1};
use place::{
    Cut, Forms, QuoteRegions, clause_at, has_cue, has_negation, in_question_clause, sentences_at,
    to_sent,
};
pub use rules::{
    B3Rules, DictAttribute, Dictionary, DictionaryDecl, DictionaryError, GATES_VERSION,
    LanguageExtra, RuleVersions, Rules, Sensitive, SubjectRules, b3_rules,
};
pub use text::{MAX_SUBJECT_CHARS, check_text};
use text::{SELF, check_subject, has_redaction_marker, is_sensitive};

/// 自由記述の属性の長さの上限 (正規化 v1 の後の文字数、04 §5.1)。
pub const FREE_ASPECT_MAX_CHARS: usize = 40;

/// 自由記述の属性の値の長さの上限 (文字数、04 §5.1。暫定)。
pub const FREE_VALUE_MAX_CHARS: usize = 60;

/// 候補を捨てた理由 (04 §5.2)。並びは当てる順。名前 ([`Reason::as_str`]) は数えるときの固定の符号で、`doctor` と
/// 試験の事例 (`tests/memory-cases/v0.5/`) が使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reason {
    /// 属性の決め方: 自由記述の属性が空・40 文字を超える・関門 6 に通らない。
    Aspect,
    /// 関門 1: 根拠の文が台帳に無い・ターンが無いか改訂の番号が合わない・今の前のふるいの後の本文の外にある・
    /// すべてが `[非表示]` の範囲にある。
    Evidence,
    /// 関門 2: 値が根拠の文に (`[非表示]` の範囲の外で) 一致しない。位置を保存した本文に戻せないときも。
    ValueNotInEvidence,
    /// 関門 3: 叙法が問い・仮定、辞書の属性の `world` 以外の否定。値の位置が問いの文の最後の節にあるときも。
    Mood,
    /// 関門 3a: `cue = true` の属性で、値の位置が掛かる文に手がかりの語が無い。
    NoCue,
    /// 関門 3b: 値の位置が掛かる節の否定の印と、叙法 (`fact`・`negated`) が合わない。
    Polarity,
    /// 関門 4: 値の一致が、どれも括弧・引用符の内側に掛かる。
    Quoted,
    /// 関門 5: 主語が許されない (属性が `self` 以外を許さない・根拠の文に無い・長すぎる・プロファイルで `self` を
    /// 表す語か応答する側を表す語と同じ)。呼び名が関門 6 に外れたときは、関門 6 の理由にする。
    Subject,
    /// 関門 6: 禁じた一般カテゴリの文字と、表示されない文字。
    ForbiddenCategory,
    /// 関門 6: 行・段落の区切り、表の区切り。
    ForbiddenChar,
    /// 関門 6: URL・スキームの印。
    Url,
    /// 関門 6: 役割の詐称・区切りの模倣。
    RoleMarker,
    /// 関門 6: 1 つの語に許していない組の文字体系を混ぜた並び。
    MixedScript,
    /// 関門 6: 命令形の文・依頼の印。
    Imperative,
    /// 関門 6: ほかの指示への言及。
    InstructionRef,
    /// 関門 6: 伏せ字の印を含む、または根拠の文の中で値の一致が印に接する。
    RedactionMarker,
    /// 関門 6a: 機微な情報のカテゴリ。
    Sensitive,
    /// 関門 7: 値が属性の上限を超える。
    TooLong,
    /// 関門 8: 効いている忘れるの指示に当たる。
    Forgotten,
}

impl Reason {
    /// すべての理由 (当てる順)。
    pub const ALL: [Self; 19] = [
        Self::Aspect,
        Self::Evidence,
        Self::ValueNotInEvidence,
        Self::Mood,
        Self::NoCue,
        Self::Polarity,
        Self::Quoted,
        Self::Subject,
        Self::ForbiddenCategory,
        Self::ForbiddenChar,
        Self::Url,
        Self::RoleMarker,
        Self::MixedScript,
        Self::Imperative,
        Self::InstructionRef,
        Self::RedactionMarker,
        Self::Sensitive,
        Self::TooLong,
        Self::Forgotten,
    ];

    /// 数えるときの名前 (04 §5.2 の括弧の中の名前)。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aspect => "aspect",
            Self::Evidence => "evidence",
            Self::ValueNotInEvidence => "value_not_in_evidence",
            Self::Mood => "mood",
            Self::NoCue => "no_cue",
            Self::Polarity => "polarity",
            Self::Quoted => "quoted",
            Self::Subject => "subject",
            Self::ForbiddenCategory => "forbidden_category",
            Self::ForbiddenChar => "forbidden_char",
            Self::Url => "url",
            Self::RoleMarker => "role_marker",
            Self::MixedScript => "mixed_script",
            Self::Imperative => "imperative",
            Self::InstructionRef => "instruction_ref",
            Self::RedactionMarker => "redaction_marker",
            Self::Sensitive => "sensitive",
            Self::TooLong => "too_long",
            Self::Forgotten => "forgotten",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 通った候補について、別に数える理由 (受け付けたが数える、04 §5.2)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Note {
    /// 値が正規化 v1 で一致した (生の本文では一致しない)。
    ValueNormalized,
    /// 値が照合の形で一致した。
    ValueLoose,
    /// `old` が根拠の文に無く、空として扱った。
    OldIgnored,
    /// `when` が根拠の文に無く、空として扱った。
    WhenIgnored,
    /// 辞書の属性で、`l` を辞書の層に直した。
    LayerFixed,
}

impl Note {
    /// 数えるときの名前。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ValueNormalized => "value_normalized",
            Self::ValueLoose => "value_loose",
            Self::OldIgnored => "old_ignored",
            Self::WhenIgnored => "when_ignored",
            Self::LayerFixed => "layer_fixed",
        }
    }
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 関門を通った候補の叙法 (問い・仮定は関門 3 で捨てる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mood {
    /// 言い切り。
    Fact,
    /// 否定。辞書の属性の `world` だけ (導出の「否定」の規則にだけ使う、04 §5.3)。
    Negated,
}

/// 自由記述の属性 (正規化 v1 の後の文字列と、その鍵、04 §5.6)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FreeAspect {
    /// 正規化 v1 の後の属性。
    pub text: String,
    /// 自由記述の属性の鍵の 16 進 (`aspect_key`。指示には文字列ではなくこの鍵を書く、04 §7.5)。
    pub mac: String,
}

impl FreeAspect {
    fn new(key: &ForgetKey, normalized: &str) -> Self {
        Self {
            text: normalized.to_owned(),
            mac: aspect_key(key, normalized).to_hex(),
        }
    }
}

/// 属性の決め方 (04 §5.2) の結果。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Aspect {
    /// 辞書の属性。
    Dict {
        /// 辞書の属性の名前。
        name: String,
        /// 別名で写したときの、写す前の自由記述の属性 (`end`・採る・戻すは写す前の鍵とも比べる、04 §7.5)。
        from_free: Option<FreeAspect>,
    },
    /// 自由記述の属性。
    Free(FreeAspect),
}

impl Aspect {
    /// 単位の `aspect` (辞書の属性の名前か、正規化した自由記述の属性)。
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Dict { name, .. } => name,
            Self::Free(f) => &f.text,
        }
    }

    /// 自由記述の属性か (単位の `free`)。
    #[must_use]
    pub fn is_free(&self) -> bool {
        matches!(self, Self::Free(_))
    }

    /// 指示を当てるときの属性 (`apply`)。
    #[must_use]
    pub fn probe(&self) -> ProbeAspect<'_> {
        match self {
            Self::Dict { name, from_free } => ProbeAspect::Dict {
                name,
                was_free: from_free.as_ref().map(|f| f.mac.as_str()),
            },
            Self::Free(f) => ProbeAspect::Free { mac: &f.mac },
        }
    }
}

/// 記録する文字列 (保存した本文の部分文字列、04 §5.2 の「記録する文字列」) と、その範囲と鍵。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// 保存した本文の部分文字列。
    pub text: String,
    /// 保存した本文の範囲。
    pub range: ByteRange,
    /// 値の鍵と照合の鍵 (04 §5.6)。
    pub keys: ValueKeys,
}

/// 根拠の文の中で見つけた `when` の表現 (解くのは導出、04 §5.1)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhenText {
    /// 保存した本文の部分文字列。
    pub text: String,
    /// 保存した本文の範囲。
    pub range: ByteRange,
}

/// 関門を通った候補 (導出 (B4) に渡す形)。範囲は保存した本文 (改訂の番号 `rev`) の位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passed {
    /// 根拠のターン。
    pub turn: Id,
    /// 根拠のターンの改訂の番号。
    pub rev: u32,
    /// 根拠の文の範囲 (`anchors` の `s_start`・`s_end`)。
    pub sentence: ByteRange,
    /// 層 (辞書の属性は辞書の層)。
    pub layer: UnitLayer,
    /// 主語 (`self` か、根拠の文の中の呼び名に正規化 v1 を当てたもの)。
    pub entity: String,
    pub aspect: Aspect,
    /// 値 (`anchors` の `start`・`end` は `value.range`)。
    pub value: Recorded,
    pub mood: Mood,
    pub tense: UnitTense,
    /// 置き換える古い値 (根拠の文に無いか、正規化 v1 で違う一致が混じるときは `None`)。
    pub old: Option<Recorded>,
    /// 日付の表現 (根拠の文に無ければ `None`)。
    pub when: Option<WhenText>,
    /// 根拠の文に読み取りの規則を当てた印 (04 §5.3 の置き換えの条件と否定)。
    pub signals: Signals,
    notes: BTreeSet<Note>,
}

impl Passed {
    /// 別に数える理由 (並びは [`Note`] の順)。
    pub fn notes(&self) -> impl Iterator<Item = Note> + '_ {
        self.notes.iter().copied()
    }

    /// 指示を当てる相手 (関門 8・private・採る・戻す、04 §7.5)。
    #[must_use]
    pub fn probe(&self) -> Probe<'_> {
        Probe::new(
            ResolvedEntity::new(&self.entity, str::to_owned),
            self.aspect.probe(),
            self.value.keys,
            self.turn,
        )
    }
}

/// 関門を当てる利用者の発話 1 ターン (今の本文と、今の規則の前のふるいの後の本文、`[非表示]` の範囲)。
#[derive(Debug, Clone)]
pub struct Utterance<'a> {
    turn: Id,
    rev: u32,
    text: &'a str,
    filtered: Prefiltered,
    /// `[非表示]` の範囲 (保存した本文の位置。始まりの順)。
    hidden: Vec<Range<usize>>,
}

impl<'a> Utterance<'a> {
    /// ターン `turn` の今の本文 `text` (改訂の番号 `rev`) と、その `[非表示]` の範囲 `hidden` (今の本文の位置、04
    /// §7.5)。前のふるいは今の規則で当てる。
    #[must_use]
    pub fn new(turn: Id, rev: u32, text: &'a str, mut hidden: Vec<Range<usize>>) -> Self {
        hidden.sort_by_key(|r| (r.start, r.end));
        Self {
            turn,
            rev,
            text,
            filtered: Prefiltered::from_stored(text),
            hidden,
        }
    }

    /// [`Utterance::new`] の `[非表示]` の範囲を、指示の集合から求める。指示と今の本文が合わないターン (04 §7.2)
    /// は `None` (呼び手はそのターンを渡さない。そのターンの候補は関門 1 で落ちる)。
    #[must_use]
    pub fn with_directives(
        turn: Id,
        rev: u32,
        text: &'a str,
        directives: &DirectiveSet,
    ) -> Option<Self> {
        directives
            .redactions()
            .hidden_ranges(turn, text, rev)
            .map(|h| Self::new(turn, rev, text, h))
    }

    /// 前のふるいの後の本文の範囲 `sent` を保存した本文に戻し、`[非表示]` の範囲に掛からなければ返す。
    fn usable(&self, sent: &Range<usize>) -> Option<Range<usize>> {
        let stored = self.filtered.to_stored(sent.clone())?;
        (!self
            .hidden
            .iter()
            .any(|h| h.start < stored.end && stored.start < h.end))
        .then_some(stored)
    }

    /// [`Self::usable`] の範囲と、その保存した本文の部分文字列。
    fn stored_text(&self, sent: &Range<usize>) -> Option<(&'a str, Range<usize>)> {
        let s = self.usable(sent)?;
        Some((self.text.get(s.clone())?, s))
    }

    /// 保存した本文の範囲 `r` が、`[非表示]` の範囲にすっかり覆われるか。
    fn fully_hidden(&self, r: &Range<usize>) -> bool {
        let mut at = r.start;
        for h in &self.hidden {
            if h.start <= at && at < h.end {
                at = h.end;
            }
            if at >= r.end {
                return true;
            }
        }
        false
    }
}

/// 台帳 1 つの根拠: 文番号の表 (台帳の `sentences`) と、その文のターンの今の発話。
#[derive(Debug, Clone)]
pub struct Evidence<'a> {
    sentences: BTreeMap<u32, &'a SentenceSpan>,
    turns: BTreeMap<Id, Utterance<'a>>,
}

impl<'a> Evidence<'a> {
    /// 台帳の `sentences` と、利用者の発話のターン (応答のターンは渡さない。応答の文は根拠にならない、関門 1)。
    /// 同じ文番号が 2 つあれば、前のものを使う。
    #[must_use]
    pub fn new(
        sentences: &'a [SentenceSpan],
        turns: impl IntoIterator<Item = Utterance<'a>>,
    ) -> Self {
        let mut by_number = BTreeMap::new();
        for s in sentences {
            by_number.entry(s.s).or_insert(s);
        }
        Self {
            sentences: by_number,
            turns: turns.into_iter().map(|u| (u.turn, u)).collect(),
        }
    }
}

/// 関門の環境。
#[derive(Debug, Clone, Copy)]
pub struct Env<'a> {
    pub dictionary: &'a Dictionary,
    pub rules: &'a Rules<'a>,
    /// 値の鍵・照合の鍵・自由記述の属性の鍵の鍵 (03 §3.1.1)。
    pub key: &'a ForgetKey,
}

/// 関門 1〜7 の結果を覚えるときの鍵 (04 §5.3: 要素・根拠の `rev`・`[非表示]` の範囲・規則の版)。同じ鍵なら
/// [`screen`] の結果は同じ。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// 文番号が指す根拠の文 (ターン, 改訂の番号, 始まり, 終わり)。台帳に無い文番号は `None`。
    sentence: Option<(Id, u32, u32, u32)>,
    layer: LedgerLayer,
    entity: String,
    aspect: String,
    value: String,
    mood: LedgerMood,
    tense: LedgerTense,
    old: String,
    when: String,
    /// 根拠のターンの `[非表示]` の範囲。
    hidden: Vec<(usize, usize)>,
    rules: RuleVersions,
    dictionary: u32,
}

/// [`screen`] の結果を覚えるときの鍵。
///
/// 前のふるいの規則は、今の規則で当てる (コードの版は [`GATES_VERSION`] に含める)。
#[must_use]
pub fn cache_key(env: &Env<'_>, evidence: &Evidence<'_>, unit: &LedgerUnit) -> CacheKey {
    let span = evidence.sentences.get(&unit.sentence);
    let hidden = span
        .and_then(|s| evidence.turns.get(&s.turn))
        .map_or_else(Vec::new, |u| {
            u.hidden.iter().map(|r| (r.start, r.end)).collect()
        });
    CacheKey {
        sentence: span.map(|s| (s.turn, s.rev, s.start, s.end)),
        layer: unit.layer,
        entity: unit.entity.clone(),
        aspect: unit.aspect.clone(),
        value: unit.value.clone(),
        mood: unit.mood,
        tense: unit.tense,
        old: unit.old.clone(),
        when: unit.when.clone(),
        hidden,
        rules: env.rules.versions(),
        dictionary: env.dictionary.version(),
    }
}

/// 関門 8 (`forgotten`、04 §5.2): 通った候補に当たる忘れるの指示 (ID の最も小さいもの)。当て方は
/// [`DirectiveSet::forgotten_by`] (記録する値の値の鍵と照合の鍵、entity の同値類、辞書の属性なら属性)。
#[must_use]
pub fn forgotten(directives: &DirectiveSet, passed: &Passed) -> Option<Id> {
    directives.forgotten_by(&passed.probe())
}

/// 属性の決め方と関門 1〜8 を順に当てる (04 §5.2)。
///
/// # Errors
///
/// 最初に外れた関門の理由を返す。
pub fn check(
    env: &Env<'_>,
    evidence: &Evidence<'_>,
    directives: &DirectiveSet,
    unit: &LedgerUnit,
) -> Result<Passed, Reason> {
    let passed = screen(env, evidence, unit)?;
    if forgotten(directives, &passed).is_some() {
        return Err(Reason::Forgotten);
    }
    Ok(passed)
}

/// 値の照合の段 (04 §5.2 の関門 2: 生の本文 → 正規化 v1 → 照合の形)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Raw,
    Normalized,
    Loose,
}

/// `needle` の一致のうち `keep` を満たすものを、正規化 v1 → 照合の形の順に探す (前の段で 1 つでも当たれば次の段
/// に進まない、04 §5.6 の「照合の順」)。生の本文の一致は正規化 v1 の一致に含まれるので、正規化 v1 の一致の元の
/// 文字列が写しと同じなら生の段とする。
fn find_staged(
    hay: &Haystack<'_>,
    raw: &str,
    needle: &Needle,
    keep: impl Fn(&Hit) -> bool,
) -> (Vec<Hit>, Stage) {
    if needle.is_empty() {
        return (Vec::new(), Stage::Raw);
    }
    let v1: Vec<Hit> = hay.v1().map_or_else(Vec::new, |m| {
        m.find(&needle.v1, Edges::Word)
            .into_iter()
            .filter(&keep)
            .collect()
    });
    if !v1.is_empty() {
        let stage = if v1
            .iter()
            .any(|h| hay.text().get(h.source.clone()) == Some(raw))
        {
            Stage::Raw
        } else {
            Stage::Normalized
        };
        return (v1, stage);
    }
    let loose = hay.loose().map_or_else(Vec::new, |m| {
        m.find(&needle.loose, Edges::Word)
            .into_iter()
            .filter(&keep)
            .collect()
    });
    (loose, Stage::Loose)
}

/// 保存した本文の範囲を、記録の形にする (`u32` に収まらなければ `None`)。
fn byte_range(r: &Range<usize>) -> Option<ByteRange> {
    Some(ByteRange {
        start: u32::try_from(r.start).ok()?,
        end: u32::try_from(r.end).ok()?,
    })
}

fn layer_of(l: LedgerLayer) -> UnitLayer {
    match l {
        LedgerLayer::World => UnitLayer::World,
        LedgerLayer::Episode => UnitLayer::Episode,
    }
}

fn tense_of(t: LedgerTense) -> UnitTense {
    match t {
        LedgerTense::Current => UnitTense::Current,
        LedgerTense::Past => UnitTense::Past,
        LedgerTense::Planned => UnitTense::Planned,
    }
}

/// 根拠の文の中で、値の一致 `positions` のどれかが伏せ字の印に接するか (間に空白と記号しか無い・重なる。04 §5.2
/// の関門 6)。`sent` は前のふるいの後の本文、`sentence` は根拠の文の範囲。
fn touches_marker(sent: &str, sentence: &Range<usize>, positions: &[Range<usize>]) -> bool {
    let hay = Haystack::new(&sent[sentence.clone()]);
    let markers: Vec<Range<usize>> = evorag_store::forget::REDACTION_MARKERS
        .iter()
        .flat_map(|m| hay.find(&Needle::new(m), Edges::Any))
        .map(|h| h.source.start + sentence.start..h.source.end + sentence.start)
        .collect();
    let gap = |r: Range<usize>| sent.get(r).is_some_and(|g| g.chars().all(is_separator));
    positions.iter().any(|p| {
        markers.iter().any(|m| {
            (m.start < p.end && p.start < m.end)
                || (m.end <= p.start && gap(m.end..p.start))
                || (p.end <= m.start && gap(p.end..m.start))
        })
    })
}

/// 属性の決め方と関門 1〜7 を順に当てる (04 §5.2)。結果は [`cache_key`] の鍵で覚えてよい。
///
/// # Errors
///
/// 最初に外れた関門の理由を返す。
pub fn screen(env: &Env<'_>, evidence: &Evidence<'_>, unit: &LedgerUnit) -> Result<Passed, Reason> {
    let rules = env.rules;
    let mut notes = BTreeSet::new();

    // 属性の決め方: 正規化 v1 の後に、辞書の属性の名前か別名なら辞書の属性。ほかは自由記述の属性で、空・40 文字
    // を超える・関門 6 に通らないものを捨てる。写した後の属性の宣言で、以下の関門を当てる。
    let normalized = normalize_v1(&unit.aspect);
    let (aspect, dict) = if let Some((d, via_alias)) = env.dictionary.resolve(&normalized) {
        let from_free = via_alias.then(|| FreeAspect::new(env.key, &normalized));
        let aspect = Aspect::Dict {
            name: d.name().to_owned(),
            from_free,
        };
        (aspect, Some(d))
    } else {
        if normalized.is_empty()
            || normalized.chars().count() > FREE_ASPECT_MAX_CHARS
            || check_text(rules, &unit.aspect).is_err()
            || check_text(rules, &normalized).is_err()
        {
            return Err(Reason::Aspect);
        }
        (Aspect::Free(FreeAspect::new(env.key, &normalized)), None)
    };
    let mut layer = layer_of(unit.layer);
    if let Some(d) = dict
        && d.layer() != layer
    {
        layer = d.layer();
        notes.insert(Note::LayerFixed);
    }
    let attribute_name = dict.map(DictAttribute::name);

    // 関門 1: 文番号が台帳の `sentences` にあり、そのターンが今の利用者の発話で改訂の番号が合い、文が今の前のふるい
    // の後の本文の中にあり (除いた行に掛からない)、`[非表示]` の範囲の外の部分を持つ。
    let span = evidence
        .sentences
        .get(&unit.sentence)
        .ok_or(Reason::Evidence)?;
    let u = evidence.turns.get(&span.turn).ok_or(Reason::Evidence)?;
    let stored_sentence = usize::try_from(span.start).map_err(|_| Reason::Evidence)?
        ..usize::try_from(span.end).map_err(|_| Reason::Evidence)?;
    let bounds = grapheme_boundaries(u.text);
    if span.rev != u.rev
        || stored_sentence.is_empty()
        || bounds.binary_search(&stored_sentence.start).is_err()
        || bounds.binary_search(&stored_sentence.end).is_err()
        || u.fully_hidden(&stored_sentence)
    {
        return Err(Reason::Evidence);
    }
    let sentence = to_sent(&u.filtered, &stored_sentence).ok_or(Reason::Evidence)?;
    let sent = u.filtered.text();

    // 関門 2: 値が根拠の文の中に一致する。語の境界は根拠の文の端ではなく、前のふるいの後の本文の全体で見る。
    // `[非表示]` の範囲に掛かる一致と、保存した本文に戻せない一致は一致としない。
    let whole = Haystack::new(sent);
    let in_sentence = |h: &Hit| {
        sentence.start <= h.source.start
            && h.source.end <= sentence.end
            && u.usable(&h.source).is_some()
    };
    let stored_of = |r: &Range<usize>| u.stored_text(r);
    let (hits, stage) = find_staged(&whole, &unit.value, &Needle::new(&unit.value), in_sentence);
    if hits.is_empty() {
        return Err(Reason::ValueNotInEvidence);
    }
    match stage {
        Stage::Raw => {}
        Stage::Normalized => {
            notes.insert(Note::ValueNormalized);
        }
        Stage::Loose => {
            notes.insert(Note::ValueLoose);
        }
    }
    let positions: Vec<Range<usize>> = hits.into_iter().map(|h| h.source).collect();
    // `old`: 根拠の文の中の一致 (括弧の内側かは問わない) が、すべて正規化 v1 で同じ文字列になるときだけ、最初の
    // 一致を採る。一致が無ければ空とし `old_ignored` として数える。
    let old = if unit.old.trim().is_empty() {
        None
    } else {
        let (hits, _) = find_staged(&whole, &unit.old, &Needle::new(&unit.old), in_sentence);
        let found: Vec<(&str, Range<usize>)> =
            hits.iter().filter_map(|h| stored_of(&h.source)).collect();
        match found.first() {
            None => {
                notes.insert(Note::OldIgnored);
                None
            }
            Some((first, range)) => {
                let n = normalize_v1(first);
                found
                    .iter()
                    .all(|(t, _)| normalize_v1(t) == n)
                    .then(|| (*first, range.clone()))
            }
        }
    };
    // `when`: 根拠の文の中の最初の一致。無ければ空とし `when_ignored` として数える (ほかの文の日付を組み合わせて
    // 写したものを解かないため)。
    let when = if unit.when.trim().is_empty() {
        None
    } else {
        let (hits, _) = find_staged(&whole, &unit.when, &Needle::new(&unit.when), in_sentence);
        let found = hits.first().and_then(|h| stored_of(&h.source));
        if found.is_none() {
            notes.insert(Note::WhenIgnored);
        }
        found
    };

    // 関門 3: 叙法。`negated` は辞書の属性の `world` だけ。
    let mood = match unit.mood {
        LedgerMood::Fact => Mood::Fact,
        LedgerMood::Negated if dict.is_some() && layer == UnitLayer::World => Mood::Negated,
        LedgerMood::Negated | LedgerMood::Question | LedgerMood::Hypothetical => {
            return Err(Reason::Mood);
        }
    };
    // 値の位置: 括弧・引用符の内側に 1 文字も掛からない最初の一致 (関門 4 と記録する値)。無ければ最初の一致。
    let quotes = QuoteRegions::new(rules.languages.iter().flat_map(|r| &r.safety.quote), sent);
    let unquoted = positions
        .iter()
        .find(|p| !quotes.overlaps(attribute_name, p));
    let place = unquoted
        .or_else(|| positions.first())
        .ok_or(Reason::ValueNotInEvidence)?;
    // 関門 3 の補い: 値の位置が、問いの印で終わる文の最後の節に掛かれば、問いとして捨てる。問いの文は、略語の後ろの
    // `.` では切らない (切ると問いの文の前半の値を通すため)。
    let languages = &rules.languages;
    if sentences_at(sent, place, Cut::KeepAbbreviations)
        .iter()
        .any(|s| in_question_clause(sent, s, place, languages))
    {
        return Err(Reason::Mood);
    }
    let sentences = sentences_at(sent, place, Cut::Delimiters);
    // 関門 3a: `cue = true` の属性は、値の位置が掛かる文のどれかに、その属性の手がかりの語を求める。
    if let Some(d) = dict
        && d.attribute().cue
        && !sentences
            .iter()
            .any(|s| has_cue(&Forms::new(&sent[s.clone()]), d.name(), languages))
    {
        return Err(Reason::NoCue);
    }
    // 関門 3b: 値の位置が掛かる節の否定の印と叙法が合う (値の意味が否定を含む属性には当てない)。
    if !dict.is_some_and(DictAttribute::negative) {
        let extras = rules.extras();
        let negation = sentences.iter().any(|s| {
            let clause = clause_at(sent, s, place, languages);
            has_negation(&sent[clause], &extras)
        });
        if negation != (mood == Mood::Negated) {
            return Err(Reason::Polarity);
        }
    }

    // 関門 4: 括弧・引用符の内側に掛からない一致がある。記録する値は、その最初の一致の保存した本文。
    let at = unquoted.ok_or(Reason::Quoted)?;
    let (value_text, value_range) = stored_of(at).ok_or(Reason::ValueNotInEvidence)?;

    // 関門 5: 主語。`self` 以外の呼び名は根拠の文に一致し、記録する主語はその最初の一致の保存した本文から作る。
    let others = dict.is_none_or(|d| d.attribute().other_subjects);
    let mut entity = check_subject(rules, others, &unit.entity)?;
    if entity != SELF {
        let (hits, _) = find_staged(
            &whole,
            &unit.entity,
            &Needle::new(&unit.entity),
            in_sentence,
        );
        let written = hits
            .first()
            .and_then(|h| stored_of(&h.source))
            .ok_or(Reason::Subject)?;
        entity = check_subject(rules, others, written.0)?;
    }

    // 関門 6: 記録する値・LLM の写しの値・記録する `old`。根拠の文の中で値の一致が伏せ字の印に接するものも捨てる。
    check_text(rules, value_text)?;
    check_text(rules, &unit.value)?;
    if has_redaction_marker(value_text) || touches_marker(sent, &sentence, &positions) {
        return Err(Reason::RedactionMarker);
    }
    if let Some((o, _)) = &old {
        check_text(rules, o)?;
    }
    // 関門 6a: 機微な情報 (値・`old`・自由記述の属性)。
    let free_text = match &aspect {
        Aspect::Free(f) => Some(f.text.as_str()),
        Aspect::Dict { .. } => None,
    };
    if is_sensitive(rules, value_text)
        || old.as_ref().is_some_and(|(o, _)| is_sensitive(rules, o))
        || free_text.is_some_and(|a| is_sensitive(rules, a))
    {
        return Err(Reason::Sensitive);
    }
    // 関門 7: 値の長さ。
    let max = dict.map_or(FREE_VALUE_MAX_CHARS, |d| {
        usize::from(d.attribute().max_chars)
    });
    if value_text.chars().count() > max {
        return Err(Reason::TooLong);
    }

    let recorded = |text: &str, range: &Range<usize>| -> Result<Recorded, Reason> {
        Ok(Recorded {
            text: text.to_owned(),
            range: byte_range(range).ok_or(Reason::Evidence)?,
            keys: ValueKeys::of(env.key, text),
        })
    };
    let value = recorded(value_text, &value_range)?;
    let old = old.map(|(t, r)| recorded(t, &r)).transpose()?;
    let when = match when {
        Some((t, r)) => Some(WhenText {
            text: t.to_owned(),
            range: byte_range(&r).ok_or(Reason::Evidence)?,
        }),
        None => None,
    };
    Ok(Passed {
        turn: u.turn,
        rev: u.rev,
        sentence: byte_range(&stored_sentence).ok_or(Reason::Evidence)?,
        layer,
        // 読み取りの規則は、前のふるいの後の根拠の文に当てる (除いた行の文字を読まない)。
        signals: signals(&sent[sentence.clone()], &entity),
        entity,
        aspect,
        value,
        mood,
        tense: tense_of(unit.tense),
        old,
        when,
        notes,
    })
}
