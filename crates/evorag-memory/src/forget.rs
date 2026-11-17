//! 忘れる (04 §7.1〜§7.5): 計画と当て直し。
//!
//! - 計画 ([`plan_forget`]): 対象の行と選択から、伏せ字にするターンと範囲・取り消す事実・消す派生の記録・指示の
//!   記録と、書き手に渡す操作の列を、記録を変えずに作る (04 §7.1 の「見本と計画」)。見本と計画のハッシュも返す。
//! - 当て直し ([`replay_forgets`]): 忘れるの指示だけから、DB の層 (ターンの本文・取り消し・span・消す派生の記録)
//!   を当て直す (04 §7.2 の「当て直し」)。何度当てても同じ。
//! - ジャーナルの層 ([`journal_cuts`]): 書き直しの対象 (04 §7.2 の「ジャーナルの層の見つけ方」)。
//! - ターンごとの指示 ([`TurnForgets`]): 伏せ字と `[非表示]` の範囲。画面の印と想起の伏せに使う。
//! - 忘れた範囲 ([`ForgetScope`]): 忘れたターンと値の鍵 (訂正の候補と検証の結果を止める、04 §6)。
//!
//! 値の鍵の検査は操作のときだけ行い、当て直しでは行わない (ハッシュで確かめる、04 §7.2 の純粋な核)。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Range;

use evorag_store::forget::{
    HiddenRange, Mark, MarkKind, Redaction, TurnDirective, TurnOutcome, apply_to_turn,
    display_marks, merge_ranges, turn_patch, widen_to_graphemes,
};
use evorag_store::{Envelope, JournalCut, KIND_QUARANTINE, Read, ReadTxn, RevSpan, WriteOp};
use evorag_types::api::FactKey;
use evorag_types::{Id, IdGen, Timestamp};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::correction::{CorrectionBody, KIND_CORRECTION, REL_CORRECTION_OF};
use crate::extract::turn_rev_checked;
use crate::facts::{
    FACT_DIRECTIVE_V, FactDirectiveBody, HiddenBody, KIND_FACT_DIRECTIVE, OP_FORGET, ORIGIN_USER,
    RedactionBody, derive_directive, diff_ops, for_each_directive_body, load_directives,
    load_facts,
};
use crate::fold::{
    Directive, DirectiveKind, Fact, FactSet, FactState, Source, refresh_private, retract,
};
use crate::kinds::{
    ChunkBody, EmbeddingBody, JOB_FORGET_BACKUPS, JOB_PURGE_INDEXES, JobBody, KIND_CHUNK,
    KIND_EMBEDDING, KIND_JOB, KIND_NOTE, KIND_TURN, NoteBody, REL_CHUNK_OF, REL_EMBEDDING_OF,
    REL_NOTE_OF, REL_REPLY_TO,
};
use crate::mask::apply_ranges;
use crate::note::note_matches;
use crate::read::MemoryError;
use crate::rules::Attributes;
use crate::value_key::{
    BlockMark, Edges, ForgetKey, Mapped, NORM_V1, ValueKey, block_mark, normalize_v1, value_key,
};
use crate::view::{find_row, row_key};

/// この版が読むターンの `v` (03 §3.3)。これより新しい `v` のターンは、伏せ字にできない (03 §4.2 の規則 6)。
const TURN_V: u32 = 1;

/// 忘れるの選択 (04 §7.1 の確認の画面)。既定は [`ForgetOptions::default`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgetOptions {
    /// 出典の発話を伏せ字にする (既定)。偽なら事実だけを消し、選んだ範囲を想起での `[非表示]` にする。
    pub redact: bool,
    /// 同じ (主語, 属性) の置き換えの履歴 (`Superseded` の値。時制を問わない) もまとめて忘れる。`Contested` は
    /// 含めない。
    pub history: bool,
    /// 同じ往復の応答 (出典の発話を `reply_to` に持つすべての応答) の中で値の鍵が一致する範囲も伏せる (既定)。
    pub echo: bool,
    /// ほかのターン (すべてのセッションの利用者の発話と応答) で値の鍵が一致する範囲も伏せる。
    pub others: bool,
}

impl Default for ForgetOptions {
    fn default() -> Self {
        Self {
            redact: true,
            history: false,
            echo: true,
            others: false,
        }
    }
}

/// 計画と当て直しの環境。
pub struct ForgetEnv<'a> {
    pub key: &'a ForgetKey,
    pub attributes: &'a Attributes,
    /// 伏せ字の印 (プロンプトの言語で選ぶ、04 §7.3。[`crate::redaction_marker`])。
    pub marker: &'a str,
    /// `[非表示]` の印 (見本の、伏せ字にしないときの後の本文に使う。[`crate::hidden_marker`])。
    pub hidden_marker: &'a str,
    /// 指示と job の ID。ストアと同じ、単調に増える ID の作り手を渡す (指示の ID はターン・ノートの ID と同じ列で
    /// 比べる: 当て直しの「指示より前に作ったか」、ノートの `forgets_seen`、04 §5.3 の入力の順)。
    pub ids: &'a dyn IdGen,
    pub now: Timestamp,
}

impl fmt::Debug for ForgetEnv<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgetEnv")
            .field("marker", &self.marker)
            .field("hidden_marker", &self.hidden_marker)
            .field("now", &self.now)
            .finish_non_exhaustive()
    }
}

/// 忘れるを中止した理由 (04 §7.1 の「中止」、§7.3)。本文も事実も変えない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetAbort {
    /// 指した行が無い (統合で置き換わった・消えた)。「一覧が更新されました」と出して新しい一覧を出す。
    RowChanged,
    /// 指した行が値の行 (今の値・`Contested` の値) でない。
    NotAValue,
    /// 伏せ字にするターンが読めない (読めない記録・`v` が新しい記録・改訂の番号を読めない)。
    UnreadableTurn(Id),
    /// 取り消す事実の出典の改訂の番号が、今のターンと合わない。
    StaleSource(Id),
    /// 伏せ字にする範囲の本文の値の鍵が忘れる値と合わない (本文が記録と食い違う)。
    ValueMismatch(Id),
    /// 伏せ字にするターンが、前の忘れるの指示と合わない (当て直しが済んでいない・指示が欠けている、04 §7.2 の
    /// 「合わないターン」)。
    MismatchedTurn(Id),
}

impl fmt::Display for ForgetAbort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RowChanged => f.write_str("the fact list has changed"),
            Self::NotAValue => f.write_str("the row is not a value row"),
            Self::UnreadableTurn(t) => write!(f, "turn {t} cannot be read"),
            Self::StaleSource(t) => write!(f, "the source revision of turn {t} is stale"),
            Self::ValueMismatch(t) => write!(f, "the text of turn {t} does not hold the value"),
            Self::MismatchedTurn(t) => {
                write!(f, "turn {t} does not match its earlier forget directives")
            }
        }
    }
}

impl std::error::Error for ForgetAbort {}

/// 計画のハッシュ (04 §7.1 の「見本と計画」)。実行で変わるものだけを覆う。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlanHash([u8; 32]);

impl fmt::Display for PlanHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for PlanHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PlanHash({self})")
    }
}

/// 見本の 1 行 (伏せ字か `[非表示]` にするターンの、前と後の本文)。`Debug` は本文を出さない (ログに忘れる値を
/// 残さないため)。
#[derive(Clone, PartialEq, Eq)]
pub struct ForgetSample {
    pub turn: Id,
    pub before: String,
    pub after: String,
}

impl fmt::Debug for ForgetSample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgetSample")
            .field("turn", &self.turn)
            .field("before_len", &self.before.len())
            .field("after_len", &self.after.len())
            .finish()
    }
}

/// 見本 (04 §7.1 の「見本と計画」)。値そのものは本文の中にだけある (画面に出す前に、利用者が自分で選んだ値)。
/// `Debug` は本文を出さない。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ForgetPreview {
    /// 伏せ字 (か `[非表示]`) にするターンの前と後の本文 (ターンの ID の順)。
    pub samples: Vec<ForgetSample>,
    /// ほかのターンで値の鍵が一致したターンの数 (選んでいなくても数える)。
    pub other_turns: usize,
    /// ほかのターンの一致の範囲の数。
    pub other_matches: usize,
    /// まとめて忘れられる履歴 (`Superseded`) の値の数 (選んでいなくても数える)。
    pub history: usize,
    /// 伏せ字の範囲に span が重なる、ほかの事実の行 (自動では取り消さない、04 §7.1 の「影響するほかの事実」)。
    pub affected: Vec<FactKey>,
    /// 根拠の範囲に値の一致が無いので伏せ字にしない出典のターン。
    pub unredacted_sources: Vec<Id>,
    /// 出典の発話の、根拠の範囲の外で値の鍵が一致した範囲の数 (同じ往復の応答と同じ選択で伏せる)。
    pub source_outside: usize,
    /// 位置を戻せないので全体を `[非表示]` にするターンの数 (伏せ字にしないとき)。
    pub whole_turns: usize,
    /// 前の忘れるの指示と合わないので飛ばしたターン (伏せ字にしないとき。伏せ字にするときは中止する)。
    pub mismatched_turns: Vec<Id>,
    /// 探さずに飛ばした読めないターン (読めない記録・`v` が新しい記録) の数。
    pub skipped_unreadable: usize,
}

impl fmt::Debug for ForgetPreview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgetPreview")
            .field("samples", &self.samples)
            .field("other_turns", &self.other_turns)
            .field("other_matches", &self.other_matches)
            .field("history", &self.history)
            .field("affected", &self.affected.len())
            .field("unredacted_sources", &self.unredacted_sources)
            .field("source_outside", &self.source_outside)
            .field("whole_turns", &self.whole_turns)
            .field("mismatched_turns", &self.mismatched_turns)
            .field("skipped_unreadable", &self.skipped_unreadable)
            .finish()
    }
}

/// 忘れるの計画 ([`plan_forget`])。`ops` を 1 つの依頼で書き手に渡す。`Debug` は本文 (操作の封筒) を出さない。
#[derive(Clone)]
pub struct ForgetPlan {
    /// 足す忘れるの指示の ID。
    pub directive: Id,
    /// 書き込みの操作。前提 (伏せ字にするターンの改訂の番号) → 指示の `Put` → ターンの `Patch` → 事実 → 消す派生の
    /// 記録 → `purge_indexes`・`forget_backups` の job → `compact.pending` → ジャーナルの書き直しの順。
    pub ops: Vec<WriteOp>,
    pub preview: ForgetPreview,
    pub hash: PlanHash,
    /// `Retracted` にした事実 (ID の順)。事実の検索の鍵の ID も同じ (04 §5.5)。
    pub retracted: Vec<Id>,
    /// 取り消した今の値に代わって今の値にした矛盾の値。
    pub promoted: Vec<Id>,
    /// 消す派生の記録 (チャンク・ノート・埋め込み・訂正・隔離の記録) の ID。
    pub derived: Vec<Id>,
    /// 伏せ字にするターン (開いているセッションにあれば、画面に読み直させる)。
    pub redacted_turns: Vec<Id>,
    /// `[非表示]` の範囲を足すターン。
    pub hidden_turns: Vec<Id>,
    /// コミットの後のジャーナルの書き直し (`ops` にも同じものを入れてある)。
    pub journal: BTreeMap<Id, JournalCut>,
}

impl fmt::Debug for ForgetPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgetPlan")
            .field("directive", &self.directive)
            .field("ops", &self.ops.len())
            .field("preview", &self.preview)
            .field("hash", &self.hash)
            .field("retracted", &self.retracted)
            .field("promoted", &self.promoted)
            .field("derived", &self.derived)
            .field("redacted_turns", &self.redacted_turns)
            .field("hidden_turns", &self.hidden_turns)
            .field("journal", &self.journal)
            .finish()
    }
}

/// 読んだターンの要るところ。`Debug` は本文を出さない。
#[derive(Clone)]
struct TurnSnap {
    role: String,
    text: String,
    rev: u32,
    reply_to: Option<Id>,
}

impl fmt::Debug for TurnSnap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnSnap")
            .field("role", &self.role)
            .field("text_len", &self.text.len())
            .field("rev", &self.rev)
            .field("reply_to", &self.reply_to)
            .finish()
    }
}

#[derive(Debug, Clone)]
enum TurnRead {
    Missing,
    Unreadable,
    Ok(TurnSnap),
}

#[derive(Deserialize)]
struct TurnFields {
    role: String,
    text: String,
    #[serde(default)]
    reply_to: Option<Id>,
}

fn read_turn(tx: &ReadTxn<'_>, id: Id) -> Result<TurnRead, MemoryError> {
    Ok(match tx.get(KIND_TURN, id)? {
        None => TurnRead::Missing,
        Some(Read::Ok(e)) => snap(&e).map_or(TurnRead::Unreadable, TurnRead::Ok),
        Some(_) => TurnRead::Unreadable,
    })
}

/// 読めるターン (`v` がこの版のもので、本文・役割・改訂の番号が読めるもの)。
fn snap(e: &Envelope) -> Option<TurnSnap> {
    if e.v > TURN_V {
        return None;
    }
    let b = e.body_as::<TurnFields>().ok()?;
    Some(TurnSnap {
        role: b.role,
        text: b.text,
        rev: turn_rev_checked(e)?,
        reply_to: b.reply_to,
    })
}

/// 本文の対応表 `mapped` の中で、正規化した値 `needle` に一致する範囲 (広げる前の、元の本文の書記素の境界の範囲。
/// 英数字は語の境界で、04 §5.2 の関門 2 と同じ照合)。位置の対応表を作れなければ空。
fn value_matches(mapped: Option<&Mapped>, needle: &str) -> Vec<Range<usize>> {
    match mapped {
        Some(m) if !needle.is_empty() => m
            .find(needle, Edges::Word)
            .into_iter()
            .map(|h| h.source)
            .collect(),
        _ => Vec::new(),
    }
}

/// 本文の全体で、忘れる値を探した結果 ([`whole_matches`])。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Found {
    /// 一致する範囲 (書記素の境界に広げた後。空なら一致なし)。
    Ranges(Ranges),
    /// 一致の値の鍵が合わない (本文が記録と食い違う)。
    Mismatch,
    /// 正規化した本文に値はあるが、位置の対応表を作れず範囲を戻せない。
    Unplaceable,
}

impl Found {
    /// 値があるか (位置を戻せたかに関わらず)。
    fn hit(&self) -> bool {
        !matches!(self, Found::Ranges(r) if r.is_empty())
    }
}

/// 本文の全体で、忘れる値のどれかに一致する範囲 (04 §7.1 の「同じ往復の応答」「ほかのターン」)。
fn whole_matches(key: &ForgetKey, text: &str, needles: &[Needle]) -> Found {
    let Some(mapped) = Mapped::new(text) else {
        let norm = normalize_v1(text);
        return if needles
            .iter()
            .any(|n| !n.norm.is_empty() && norm.contains(&n.norm))
        {
            Found::Unplaceable
        } else {
            Found::Ranges(Vec::new())
        };
    };
    let mut out = Vec::new();
    for n in needles {
        for h in value_matches(Some(&mapped), &n.norm) {
            if !checked(key, text, &h, &n.vk) {
                return Found::Mismatch;
            }
            out.push(widen_to_graphemes(text, h));
        }
    }
    Found::Ranges(out)
}

/// 本文に、正規化した値が (位置を戻せなくても) 含まれるか。
fn holds_value(text: &str, needle: &str) -> bool {
    !needle.is_empty()
        && Mapped::new(text).map_or_else(
            || normalize_v1(text).contains(needle),
            |m| !m.find_widened(needle, Edges::Word).is_empty(),
        )
}

/// 本文の範囲の並び。
type Ranges = Vec<Range<usize>>;

/// 忘れる値 1 つ (値の鍵と、照合する正規化した値)。
#[derive(Debug, Clone)]
struct Needle {
    vk: ValueKey,
    norm: String,
}

/// 伏せるターン 1 つの範囲 (広げた後、今の本文の位置)。
#[derive(Debug, Clone)]
struct Target {
    snap: TurnSnap,
    ranges: Vec<Range<usize>>,
}

/// 範囲が値の鍵の検査を通るか (04 §7.3。広げる前の範囲で確かめる)。
fn checked(key: &ForgetKey, text: &str, r: &Range<usize>, vk: &ValueKey) -> bool {
    text.get(r.clone())
        .is_some_and(|s| value_key(key, s) == *vk)
}

/// 忘れる事実 (04 §7.1): 指した事実と同じ (主語, 属性) で、値の鍵が `vks` のどれかと同じ、`Retracted` でない
/// 事実。`vks` は指した値の鍵と、履歴もまとめるときは `Superseded` の値の鍵 (`Contested` は含めない)。
/// 当て直しの取り消し (再び覚えない印に当たる事実) と同じ集合になる。
fn forgotten_facts(
    set: &FactSet,
    target: &Fact,
    history: bool,
) -> (BTreeSet<Id>, BTreeSet<ValueKey>, usize) {
    let same = |f: &&Fact| f.subject == target.subject && f.attribute == target.attribute;
    let target_vk = target.vk.expect("value rows have a value key");
    let superseded: BTreeSet<ValueKey> = set
        .facts
        .iter()
        .filter(same)
        .filter(|f| matches!(f.state, FactState::Superseded { .. }))
        .filter_map(|f| f.vk)
        .filter(|v| *v != target_vk)
        .collect();
    let history_count = superseded.len();
    let mut vks = BTreeSet::from([target_vk]);
    if history {
        vks.extend(superseded);
    }
    let ids = set
        .facts
        .iter()
        .filter(same)
        .filter(|f| f.state != FactState::Retracted && f.vk.is_some_and(|v| vks.contains(&v)))
        .map(|f| f.id)
        .collect();
    (ids, vks, history_count)
}

/// 純粋な核の側で忘れる (回帰ケースの鏡): 事実を取り消し、印を持つ指示を足して private を計算し直す。
#[cfg(test)]
pub(crate) fn forget_in_core(
    set: &mut FactSet,
    dirs: &mut crate::fold::Directives,
    key: &ForgetKey,
    attributes: &Attributes,
    target: Id,
    history: bool,
    directive: Id,
) {
    let fact = set
        .facts
        .iter()
        .find(|f| f.id == target)
        .expect("the target fact")
        .clone();
    let (ids, vks, _) = forgotten_facts(set, &fact, history);
    retract(set, &ids, attributes);
    let marks = vks
        .iter()
        .map(|vk| block_mark(key, &fact.subject, &fact.attribute, vk))
        .collect();
    dirs.push(Directive {
        id: directive,
        kind: DirectiveKind::Forget { marks },
        subject: fact.subject.clone(),
        attribute: fact.attribute.clone(),
        vk: fact.vk,
    });
    refresh_private(set, dirs);
}

/// 消す派生の記録を集める。
#[derive(Debug, Default)]
struct Purge {
    /// (種類, ID)。ID の順に 1 回だけ消す。
    records: BTreeMap<Id, &'static str>,
}

impl Purge {
    fn add(&mut self, kind: &'static str, id: Id) {
        self.records.entry(id).or_insert(kind);
    }

    /// 埋め込みも含めて、チャンクかノートを消す。
    fn with_embeddings(
        &mut self,
        tx: &ReadTxn<'_>,
        kind: &'static str,
        id: Id,
    ) -> Result<(), MemoryError> {
        self.add(kind, id);
        for (emb, _) in tx.links_to(id, REL_EMBEDDING_OF)? {
            self.add(KIND_EMBEDDING, emb);
        }
        Ok(())
    }

    fn ids(&self) -> Vec<Id> {
        self.records.keys().copied().collect()
    }

    fn ops(&self) -> Vec<WriteOp> {
        let mut ops = Vec::new();
        for (id, kind) in &self.records {
            ops.push(WriteOp::Delete {
                kind: (*kind).into(),
                id: *id,
            });
            if *kind == KIND_EMBEDDING {
                ops.push(WriteOp::DeleteBlobs(*id));
            }
            ops.push(WriteOp::DeleteLinksOf(*id));
        }
        ops
    }
}

/// 往復のノートを引くターン (応答は、その発話)。
fn note_keys(turn: Id, snap: Option<&TurnSnap>) -> Vec<Id> {
    let mut keys = vec![turn];
    if let Some(u) = snap
        .filter(|s| s.role == "assistant")
        .and_then(|s| s.reply_to)
    {
        keys.push(u);
    }
    keys
}

/// ターンの隔離の記録 (本体の `id` がそのターン) の ID。
fn quarantined_of(tx: &ReadTxn<'_>, turns: &BTreeSet<Id>) -> Result<Vec<Id>, MemoryError> {
    #[derive(Deserialize)]
    struct Orig {
        id: String,
    }
    let mut out = Vec::new();
    if turns.is_empty() {
        return Ok(out);
    }
    tx.for_each(KIND_QUARANTINE, |r| {
        if let Read::Ok(e) = r
            && let Ok(o) = e.body_as::<Orig>()
            && o.id.parse::<Id>().is_ok_and(|id| turns.contains(&id))
        {
            out.push(e.id);
        }
        true
    })?;
    Ok(out)
}

/// 検証済みの訂正のうち、古い値か新しい値の値の鍵が `vks` のどれかと同じもの (04 §7.5 の「忘れると訂正」)。
fn corrections_with_values(
    tx: &ReadTxn<'_>,
    vks: &BTreeSet<ValueKey>,
) -> Result<Vec<Id>, MemoryError> {
    #[derive(Deserialize)]
    struct Vks {
        #[serde(default)]
        old_vk: Option<String>,
        #[serde(default)]
        new_vk: Option<String>,
    }
    let mut out = Vec::new();
    if vks.is_empty() {
        return Ok(out);
    }
    tx.for_each(KIND_CORRECTION, |r| {
        if let Read::Ok(e) = r
            && let Ok(b) = e.body_as::<Vks>()
            && [b.old_vk, b.new_vk]
                .iter()
                .flatten()
                .filter_map(|h| ValueKey::from_hex(h))
                .any(|v| vks.contains(&v))
        {
            out.push(e.id);
        }
        true
    })?;
    Ok(out)
}

fn job(
    ids: &dyn IdGen,
    now: Timestamp,
    task: &str,
    targets: Vec<Id>,
    directive: Option<Id>,
) -> Result<WriteOp, MemoryError> {
    let mut e = Envelope::new(ids.next_id(), KIND_JOB, 1, now, "");
    e.set_body(&JobBody {
        task: task.into(),
        ids: targets,
        directive,
    })?;
    Ok(WriteOp::Put(e))
}

/// 忘れるの退避の写し直しの job (03 §7.3)。`directive` は忘れるの指示、`ids` は退避から除く ID。
///
/// # Errors
/// 本体を書けないとき (起きない)。
pub fn forget_backups_job(
    ids: &dyn IdGen,
    now: Timestamp,
    directive: Id,
    targets: Vec<Id>,
) -> Result<WriteOp, MemoryError> {
    job(ids, now, JOB_FORGET_BACKUPS, targets, Some(directive))
}

/// 位置を記録の形 (`u32`) にする。
fn pos32(p: usize, turn: Id) -> Result<u32, ForgetAbort> {
    u32::try_from(p).map_err(|_| ForgetAbort::ValueMismatch(turn))
}

/// 計画のハッシュに入れる行を書く。
fn hash_line(h: &mut Sha256, line: &str) {
    h.update(line.as_bytes());
    h.update(b"\n");
}

/// 忘れるの計画を作る (04 §7.1〜§7.3)。記録は変えない。
///
/// 呼ぶ側は、索引を書き換える経路の直列化と事実を書く経路のロックを、この順に取ってから `tx` を開き、書き手の
/// コミットと索引から除くのを終えるまで放さない (04 §7.2 の「ロックの順」)。見本のときも同じ関数で計画を作り
/// (記録を変えない)、実行のときにハッシュが見本と違えば実行せずに新しい見本を出す。
///
/// - 伏せ字にするときは、出典のターンが読めない・`v` が新しい、出典の改訂の番号が今と合わない、範囲の値の鍵が
///   合わない、根拠の範囲に値が無い (本文が記録と食い違う) ときに中止する。見本では、中止の理由を示して伏せ字に
///   しない忘れるを勧める (`redact = false` で作り直す)。
/// - 伏せ字にしないときは中止しない。範囲を求められないターン (読めない・`v` が新しい・番号の合わない出典、前の
///   忘れるの指示と合わないターン) には `[非表示]` の範囲を足さない。根拠の範囲に値の一致が無い出典は、根拠の
///   範囲の全体を、位置を戻せないほかのターンは本文の全体を `[非表示]` にする (private と同じく伏せる側に倒す)。
/// - 出典の発話の、根拠の範囲の外の一致は、同じ往復の応答と同じ選択 (`echo`) で伏せる。
///
/// 書き手の前提は、伏せるターンの改訂の番号だけを確かめる (本文のハッシュは確かめない)。応答の生成中のターンの
/// 本文が改訂の番号を変えずに書き換わらないよう、呼ぶ側は、どれかのセッションで応答を生成している間は忘れるを
/// 受け付けない (04 §7.1 の「受け付けないとき」)。
///
/// # Errors
/// 記録を読めない・本体を書けないとき。
pub fn plan_forget(
    tx: &ReadTxn<'_>,
    env: &ForgetEnv<'_>,
    target: &FactKey,
    options: ForgetOptions,
) -> Result<Result<ForgetPlan, ForgetAbort>, MemoryError> {
    let before = load_facts(tx)?;
    let mut dirs = load_directives(tx)?;
    let Some(fact) = find_row(&before.set, target) else {
        return Ok(Err(ForgetAbort::RowChanged));
    };
    if !matches!(fact.state, FactState::Live | FactState::Contested { .. }) || fact.vk.is_none() {
        return Ok(Err(ForgetAbort::NotAValue));
    }
    let fact = fact.clone();
    let (forgotten, vks, history_count) = forgotten_facts(&before.set, &fact, options.history);
    let needles: Vec<Needle> = {
        let mut by_vk: BTreeMap<ValueKey, String> = BTreeMap::new();
        for f in before
            .set
            .facts
            .iter()
            .filter(|f| forgotten.contains(&f.id))
        {
            if let (Some(vk), Some(v)) = (f.vk, f.value.as_deref()) {
                by_vk.entry(vk).or_insert_with(|| normalize_v1(v));
            }
        }
        by_vk
            .into_iter()
            .map(|(vk, norm)| Needle { vk, norm })
            .collect()
    };
    let mut preview = ForgetPreview {
        history: history_count,
        ..ForgetPreview::default()
    };

    // 1. 出典の発話の、根拠の範囲の中の値の一致。
    // ターン → (出典, その事実の値の鍵)。同じ根拠から 2 つの値 (「大阪から東京に引っ越した」) がありうる。
    let mut sources: BTreeMap<Id, Vec<(Source, ValueKey)>> = BTreeMap::new();
    for f in before
        .set
        .facts
        .iter()
        .filter(|f| forgotten.contains(&f.id))
    {
        let Some(vk) = f.vk else { continue };
        for s in &f.sources {
            sources.entry(s.turn).or_default().push((*s, vk));
        }
    }
    let mut targets: BTreeMap<Id, Target> = BTreeMap::new();
    for (&turn, spans) in &sources {
        let snap = match read_turn(tx, turn)? {
            // 古い版で消したターン (出典の無い事実の掃除が外す)。伏せるものが無い。
            TurnRead::Missing => continue,
            TurnRead::Unreadable if options.redact => {
                return Ok(Err(ForgetAbort::UnreadableTurn(turn)));
            }
            TurnRead::Unreadable => {
                preview.skipped_unreadable += 1;
                continue;
            }
            TurnRead::Ok(s) => s,
        };
        let mut ranges = Vec::new();
        let mut usable = true;
        let mapped = Mapped::new(&snap.text);
        for (s, vk) in spans {
            let vk = *vk;
            let needle = needles
                .iter()
                .find(|n| n.vk == vk)
                .map_or("", |n| n.norm.as_str());
            let span = s.start as usize..s.end as usize;
            if s.rev != snap.rev || snap.text.get(span.clone()).is_none() {
                if options.redact {
                    return Ok(Err(if s.rev == snap.rev {
                        ForgetAbort::ValueMismatch(turn)
                    } else {
                        ForgetAbort::StaleSource(turn)
                    }));
                }
                usable = false;
                break;
            }
            let hits: Vec<Range<usize>> = value_matches(mapped.as_ref(), needle)
                .into_iter()
                .filter(|h| span.start <= h.start && h.end <= span.end)
                .collect();
            if hits.is_empty() {
                if !options.redact {
                    // 伏せ字にしないときは、根拠の範囲の全体を `[非表示]` にする (private と同じ、04 §7.5)。
                    ranges.push(widen_to_graphemes(&snap.text, span));
                    continue;
                }
                let within = &snap.text[span];
                if holds_value(within, needle)
                    || crate::mask::REDACTION_MARKERS
                        .iter()
                        .any(|m| within.contains(m))
                {
                    // 値は根拠の範囲にあるが書記素の境界で切り出せない (言い換えて写した根拠など)、または値の
                    // 一部が前の忘れるで伏せ字になっている (影響するほかの事実を後から忘れるとき)。伏せ字にせず、
                    // 見本に示す (04 §7.1)。
                    preview.unredacted_sources.push(turn);
                    continue;
                }
                if options.redact {
                    return Ok(Err(ForgetAbort::ValueMismatch(turn)));
                }
                continue;
            }
            for h in hits {
                if !checked(env.key, &snap.text, &h, &vk) {
                    if options.redact {
                        return Ok(Err(ForgetAbort::ValueMismatch(turn)));
                    }
                    continue;
                }
                ranges.push(widen_to_graphemes(&snap.text, h));
            }
        }
        if !usable {
            continue;
        }
        // 根拠の範囲の外の一致 (同じ往復の応答と同じ選択で伏せる)。
        match whole_matches(env.key, &snap.text, &needles) {
            Found::Ranges(all) => {
                let outside: Vec<Range<usize>> = all
                    .into_iter()
                    .filter(|r| !ranges.iter().any(|x| x.start < r.end && r.start < x.end))
                    .collect();
                preview.source_outside += outside.len();
                if options.echo {
                    ranges.extend(outside);
                }
            }
            Found::Mismatch if options.echo && options.redact => {
                return Ok(Err(ForgetAbort::ValueMismatch(turn)));
            }
            Found::Mismatch | Found::Unplaceable => {}
        }
        if !ranges.is_empty() {
            targets.insert(turn, Target { snap, ranges });
        }
    }
    preview.unredacted_sources.sort_unstable();
    preview.unredacted_sources.dedup();

    // 2. 同じ往復の応答 (出典の発話を `reply_to` に持つすべての応答。中断・失敗を含む)。
    let mut echo_turns: BTreeSet<Id> = BTreeSet::new();
    for &turn in sources.keys() {
        for (answer, _) in tx.links_to(turn, REL_REPLY_TO)? {
            echo_turns.insert(answer);
        }
    }
    echo_turns.retain(|t| !sources.contains_key(t));
    for &answer in &echo_turns {
        let snap = match read_turn(tx, answer)? {
            TurnRead::Missing => continue,
            TurnRead::Unreadable => {
                preview.skipped_unreadable += 1;
                continue;
            }
            TurnRead::Ok(s) => s,
        };
        if !options.echo {
            continue;
        }
        let ranges = match whole_matches(env.key, &snap.text, &needles) {
            Found::Ranges(r) => r,
            Found::Mismatch | Found::Unplaceable if options.redact => {
                return Ok(Err(ForgetAbort::ValueMismatch(answer)));
            }
            Found::Mismatch | Found::Unplaceable => {
                preview.whole_turns += 1;
                std::iter::once(0..snap.text.len()).collect()
            }
        };
        if !ranges.is_empty() {
            targets.insert(answer, Target { snap, ranges });
        }
    }

    // 3. ほかのターン (すべてのセッションの利用者の発話と応答のうち、上の 2 つに入らないもの)。
    // 一致のあるターンだけを覚える (すべてのターンの本文を持たない)。
    let mut others: Vec<(Id, TurnSnap, Found)> = Vec::new();
    tx.for_each(KIND_TURN, |r| {
        match r {
            Read::Ok(e) => {
                if sources.contains_key(&e.id) || echo_turns.contains(&e.id) {
                    return true;
                }
                match snap(&e) {
                    Some(s) if s.role == "user" || s.role == "assistant" => {
                        let found = whole_matches(env.key, &s.text, &needles);
                        if found.hit() {
                            others.push((e.id, s, found));
                        }
                    }
                    Some(_) => {}
                    None => preview.skipped_unreadable += 1,
                }
            }
            _ => preview.skipped_unreadable += 1,
        }
        true
    })?;
    for (turn, snap, found) in others {
        // 位置を戻せない・値の鍵が合わないターンも、ほかのターンの一致として数える。
        preview.other_turns += 1;
        let ranges = match found {
            Found::Ranges(r) => {
                preview.other_matches += r.len();
                r
            }
            Found::Mismatch | Found::Unplaceable if options.redact && options.others => {
                return Ok(Err(ForgetAbort::ValueMismatch(turn)));
            }
            Found::Mismatch | Found::Unplaceable => {
                if options.others {
                    preview.whole_turns += 1;
                }
                std::iter::once(0..snap.text.len()).collect()
            }
        };
        if options.others {
            targets.insert(turn, Target { snap, ranges });
        }
    }

    // 前の忘れるの指示と合わないターン (04 §7.2 の「合わないターン」) は伏せない。伏せ字にするときは中止する。
    let forgets = TurnForgets::load(tx)?;
    let mut skipped = Vec::new();
    for (&turn, t) in &targets {
        if forgets
            .marks(turn, &t.snap.text, t.snap.rev, env.marker)
            .is_none()
        {
            if options.redact {
                return Ok(Err(ForgetAbort::MismatchedTurn(turn)));
            }
            skipped.push(turn);
        }
    }
    for turn in &skipped {
        targets.remove(turn);
    }
    preview.mismatched_turns = skipped;

    // 4. ターンごとの伏せ字 (または `[非表示]` の範囲)。
    let directive = env.ids.next_id();
    let mut redactions: BTreeMap<Id, (Redaction, String)> = BTreeMap::new();
    let mut hidden: Vec<HiddenBody> = Vec::new();
    for (&turn, t) in &targets {
        if options.redact {
            let Ok(planned) = Redaction::plan(
                &t.snap.text,
                t.snap.rev,
                t.ranges.iter().cloned(),
                env.marker,
            ) else {
                return Ok(Err(ForgetAbort::ValueMismatch(turn)));
            };
            redactions.insert(turn, planned);
        } else {
            for r in merge_ranges(t.ranges.clone()) {
                hidden.push(HiddenBody {
                    turn,
                    rev: t.snap.rev,
                    start: match pos32(r.start, turn) {
                        Ok(p) => p,
                        Err(e) => return Ok(Err(e)),
                    },
                    end: match pos32(r.end, turn) {
                        Ok(p) => p,
                        Err(e) => return Ok(Err(e)),
                    },
                });
            }
        }
    }

    // 5. 事実: 取り消し (04 §7.4 の繰り上げ)、印、伏せ字にするターンのほかの事実の span の写し直し。
    let mut set = before.set.clone();
    let retraction = retract(&mut set, &forgotten, env.attributes);
    let marks: Vec<BlockMark> = vks
        .iter()
        .map(|vk| block_mark(env.key, &fact.subject, &fact.attribute, vk))
        .collect();
    dirs.push(Directive {
        id: directive,
        kind: DirectiveKind::Forget {
            marks: marks.clone(),
        },
        subject: fact.subject.clone(),
        attribute: fact.attribute.clone(),
        vk: fact.vk,
    });
    let mut affected: BTreeSet<Id> = BTreeSet::new();
    for (&turn, (r, _)) in &redactions {
        for f in &before.set.facts {
            if forgotten.contains(&f.id) {
                continue;
            }
            let overlaps = f.sources.iter().any(|s| {
                s.turn == turn
                    && s.rev == r.rev_before()
                    && r.ranges()
                        .iter()
                        .any(|x| (s.start as usize) < x.end && x.start < (s.end as usize))
            });
            if overlaps {
                affected.insert(f.id);
            }
        }
        remap_sources(&mut set, turn, r, env.marker);
    }
    refresh_private(&mut set, &dirs);
    preview.affected = before
        .set
        .facts
        .iter()
        .filter(|f| affected.contains(&f.id))
        .filter_map(row_key)
        .collect();

    // 6. 消す派生の記録 (04 §7.1 の表、§7.5 の「忘れると訂正」)。
    let mut purge = Purge::default();
    let note_turns: BTreeSet<Id> = targets.keys().copied().collect();
    let correction_turns: BTreeSet<Id> = if options.redact {
        redactions.keys().copied().collect()
    } else {
        sources
            .keys()
            .copied()
            .chain(hidden.iter().map(|h| h.turn))
            .collect()
    };
    if options.redact {
        for &turn in redactions.keys() {
            for (c, _) in tx.links_to(turn, REL_CHUNK_OF)? {
                purge.with_embeddings(tx, KIND_CHUNK, c)?;
            }
        }
    }
    for &turn in &note_turns {
        for key in note_keys(turn, targets.get(&turn).map(|t| &t.snap)) {
            for (n, _) in tx.links_to(key, REL_NOTE_OF)? {
                purge.with_embeddings(tx, KIND_NOTE, n)?;
            }
        }
    }
    for &turn in &correction_turns {
        for (c, _) in tx.links_to(turn, REL_CORRECTION_OF)? {
            purge.add(KIND_CORRECTION, c);
        }
    }
    for c in corrections_with_values(tx, &vks)? {
        purge.add(KIND_CORRECTION, c);
    }
    // 隔離の記録: 伏せ字にするターン・消す派生の記録・取り消す事実のもの (隔離した原文に値が残るため、04 §7.2)。
    let quarantine_of: BTreeSet<Id> = redactions
        .keys()
        .copied()
        .chain(purge.ids())
        .chain(forgotten.iter().copied())
        .collect();
    for q in quarantined_of(tx, &quarantine_of)? {
        purge.add(KIND_QUARANTINE, q);
    }
    let derived = purge.ids();

    // 7. 計画のハッシュ (実行で変わるものだけ。消す派生の記録と、選んでいないほかのターンの数は覆わない)。
    let hash = {
        let mut h = Sha256::new();
        hash_line(&mut h, "evorag.forget-plan.1");
        hash_line(&mut h, &format!("redact {}", options.redact));
        for (turn, (r, _)) in &redactions {
            let ranges: Vec<String> = r
                .ranges()
                .iter()
                .map(|x| format!("{}-{}", x.start, x.end))
                .collect();
            hash_line(
                &mut h,
                &format!("r {turn} {} {}", r.rev_before(), ranges.join(",")),
            );
        }
        for x in &hidden {
            hash_line(
                &mut h,
                &format!("h {} {} {}-{}", x.turn, x.rev, x.start, x.end),
            );
        }
        for id in &forgotten {
            hash_line(&mut h, &format!("x {id}"));
        }
        for id in &affected {
            hash_line(&mut h, &format!("a {id}"));
        }
        PlanHash(h.finalize().into())
    };

    // 8. 見本。
    for (&turn, t) in &targets {
        let after = match redactions.get(&turn) {
            Some((_, text)) => text.clone(),
            None => apply_ranges(&t.snap.text, &t.ranges, env.hidden_marker),
        };
        preview.samples.push(ForgetSample {
            turn,
            before: t.snap.text.clone(),
            after,
        });
    }

    // 9. 指示の記録と操作の列。
    let mut redaction_bodies = Vec::new();
    for (&turn, (r, _)) in &redactions {
        for x in r.ranges() {
            redaction_bodies.push(RedactionBody {
                turn,
                rev_before: r.rev_before(),
                rev_after: r.rev_after(),
                start: match pos32(x.start, turn) {
                    Ok(p) => p,
                    Err(e) => return Ok(Err(e)),
                },
                end: match pos32(x.end, turn) {
                    Ok(p) => p,
                    Err(e) => return Ok(Err(e)),
                },
                hash: r.hash().to_string(),
            });
        }
    }
    let body = FactDirectiveBody {
        op: OP_FORGET.into(),
        subject: fact.subject.clone(),
        attribute: fact.attribute.clone(),
        vk: fact.vk.map(|v| v.to_hex()),
        norm: Some(NORM_V1),
        fact: Some(fact.id),
        target: None,
        scope: None,
        marks: marks.iter().map(BlockMark::to_hex).collect(),
        vks: vks.iter().map(ValueKey::to_hex).collect(),
        turns: sources.keys().copied().collect(),
        redactions: redaction_bodies,
        hidden: hidden.clone(),
        history: options.history,
        redact: options.redact,
        origin: ORIGIN_USER.into(),
    };
    let mut denv = Envelope::new(
        directive,
        KIND_FACT_DIRECTIVE,
        FACT_DIRECTIVE_V,
        env.now,
        "",
    );
    denv.set_body(&body)?;

    let mut ops = Vec::new();
    // 前提: 伏せ字にするターンと `[非表示]` の範囲を足すターンが、読んだときの改訂の番号のまま。
    let hidden_turns: BTreeSet<Id> = hidden.iter().map(|h| h.turn).collect();
    for (&turn, t) in &targets {
        ops.push(crate::write::expect_turn(turn, t.snap.rev));
    }
    ops.push(WriteOp::Put(denv.clone()));
    ops.extend(derive_directive(&denv));
    for (&turn, (r, text)) in &redactions {
        ops.push(WriteOp::Patch {
            kind: KIND_TURN.into(),
            id: turn,
            fields: turn_patch(text, r.rev_after())?,
            updated_at: None,
        });
    }
    ops.extend(diff_ops(&before, &set, env.now)?);
    ops.extend(purge.ops());
    let mut index_ids: Vec<Id> = derived.clone();
    index_ids.extend(retraction.retracted.iter().copied());
    index_ids.sort_unstable();
    index_ids.dedup();
    ops.push(job(
        env.ids,
        env.now,
        JOB_PURGE_INDEXES,
        index_ids.clone(),
        None,
    )?);
    ops.push(forget_backups_job(env.ids, env.now, directive, index_ids)?);
    ops.push(WriteOp::SetCompactPending);
    let journal: BTreeMap<Id, JournalCut> = redactions
        .iter()
        .map(|(turn, (r, _))| (*turn, JournalCut::Below(u64::from(r.rev_after()))))
        .collect();
    if !journal.is_empty() {
        ops.push(WriteOp::RewriteJournal(journal.clone()));
    }

    Ok(Ok(ForgetPlan {
        directive,
        ops,
        preview,
        hash,
        retracted: retraction.retracted,
        promoted: retraction.promoted,
        derived,
        redacted_turns: redactions.keys().copied().collect(),
        hidden_turns: hidden_turns.into_iter().collect(),
        journal,
    }))
}

/// ターン `turn` を出典とし、出典の改訂の番号が伏せ字の前の番号と同じ事実の span を写し直し、番号を伏せ字の後の
/// 番号にする (04 §7.3。すでに後の番号の事実には触れない)。写し直した事実の数を返す。
fn remap_sources(set: &mut FactSet, turn: Id, r: &Redaction, marker: &str) -> usize {
    let mut n = 0;
    for f in &mut set.facts {
        if !f
            .sources
            .iter()
            .any(|s| s.turn == turn && s.rev == r.rev_before())
        {
            continue;
        }
        f.sources = f
            .sources
            .iter()
            .map(|s| {
                if s.turn != turn || s.rev != r.rev_before() {
                    return *s;
                }
                let span = r.remap_range(s.start as usize..s.end as usize, marker);
                Source {
                    turn,
                    start: u32::try_from(span.start).unwrap_or(u32::MAX),
                    end: u32::try_from(span.end).unwrap_or(u32::MAX),
                    rev: r.rev_after(),
                }
            })
            .collect();
        n += 1;
    }
    n
}

// ---------------------------------------------------------------------------------------------------------------
// ターンごとの忘れるの指示
// ---------------------------------------------------------------------------------------------------------------

/// 1 つの忘れるの指示が 1 つのターンに持つもの。
#[derive(Debug, Clone)]
struct Step {
    directive: Id,
    redaction: Option<Redaction>,
    hidden: Vec<HiddenRange>,
}

/// ターンごとの忘れるの指示 (伏せ字と `[非表示]` の範囲、指示の ID の順)。忘れるの指示は書き換えないので、読む
/// ときに後の伏せ字で写す (04 §7.3 の「位置の写し直し」)。
#[derive(Debug, Clone, Default)]
pub struct TurnForgets {
    turns: BTreeMap<Id, Vec<Step>>,
    /// 記録した伏せ字が形として正しくないターン (合わないターンとして扱う)。
    invalid_redaction: BTreeSet<Id>,
    /// 記録した `[非表示]` の範囲が形として正しくないターン。
    invalid_hidden: BTreeSet<Id>,
}

impl TurnForgets {
    /// 読める忘れるの指示から作る。
    ///
    /// # Errors
    /// 記録を読めないとき。
    pub fn load(tx: &ReadTxn<'_>) -> Result<Self, MemoryError> {
        let mut bodies = Vec::new();
        for_each_directive_body(tx, |id, b| {
            if b.op == OP_FORGET {
                bodies.push((id, b));
            }
        })?;
        Ok(Self::from_bodies(bodies))
    }

    /// 事実への指示の本体 (ID と本体) から作る。忘れるでない指示は無視する。DB の読み取りの口
    /// ([`ReadTxn`]) を持たない呼び手 (書き出しの確かめと `doctor`) のため。
    #[must_use]
    pub fn from_directives(bodies: Vec<(Id, FactDirectiveBody)>) -> Self {
        Self::from_bodies(
            bodies
                .into_iter()
                .filter(|(_, b)| b.op == OP_FORGET)
                .collect(),
        )
    }

    fn from_bodies(mut bodies: Vec<(Id, FactDirectiveBody)>) -> Self {
        bodies.sort_by_key(|(id, _)| *id);
        let mut out = Self::default();
        for (directive, b) in bodies {
            let mut per_turn: BTreeMap<Id, (Vec<&RedactionBody>, Vec<&HiddenBody>)> =
                BTreeMap::new();
            for r in &b.redactions {
                per_turn.entry(r.turn).or_default().0.push(r);
            }
            for h in &b.hidden {
                per_turn.entry(h.turn).or_default().1.push(h);
            }
            for (turn, (rs, hs)) in per_turn {
                let redaction = match rs.first() {
                    None => None,
                    Some(first) => {
                        let same = rs.iter().all(|r| {
                            (r.rev_before, r.rev_after, &r.hash)
                                == (first.rev_before, first.rev_after, &first.hash)
                        });
                        match Redaction::from_stored(
                            first.rev_before,
                            first.rev_after,
                            rs.iter().map(|r| (u64::from(r.start), u64::from(r.end))),
                            &first.hash,
                        ) {
                            Ok(r) if same => Some(r),
                            _ => {
                                out.invalid_redaction.insert(turn);
                                None
                            }
                        }
                    }
                };
                let mut hidden = Vec::new();
                for h in hs {
                    match HiddenRange::from_stored(h.rev, u64::from(h.start), u64::from(h.end)) {
                        Some(x) => hidden.push(x),
                        None => {
                            out.invalid_hidden.insert(turn);
                        }
                    }
                }
                out.turns.entry(turn).or_default().push(Step {
                    directive,
                    redaction,
                    hidden,
                });
            }
        }
        out
    }

    /// `[非表示]` の範囲だけから作る (試験と、指示の ID の順が要らない呼び手のため)。範囲はそのまま (写さずに)
    /// 使う。
    #[must_use]
    pub fn from_hidden(list: Vec<HiddenBody>) -> Self {
        let mut out = Self::default();
        for h in list {
            match HiddenRange::from_stored(h.rev, u64::from(h.start), u64::from(h.end)) {
                Some(x) => out.turns.entry(h.turn).or_default().push(Step {
                    directive: Id::from_bytes([0; 16]),
                    redaction: None,
                    hidden: vec![x],
                }),
                None => {
                    out.invalid_hidden.insert(h.turn);
                }
            }
        }
        out
    }

    /// 指示が 1 つも無い。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// 忘れるの指示が掛かるターン (ID の順)。
    pub fn turns(&self) -> impl Iterator<Item = Id> + '_ {
        self.turns.keys().copied()
    }

    /// ターン `turn` に伏せ字の指示があるか (形の正しくないものを含む)。
    #[must_use]
    pub fn has_redactions(&self, turn: Id) -> bool {
        self.invalid_redaction.contains(&turn)
            || self
                .turns
                .get(&turn)
                .is_some_and(|steps| steps.iter().any(|s| s.redaction.is_some()))
    }

    /// ターン `turn` の伏せ字 (指示の ID の順)。記録が形として正しくなければ `None`。
    #[must_use]
    pub fn redactions(&self, turn: Id) -> Option<Vec<Redaction>> {
        if self.invalid_redaction.contains(&turn) {
            return None;
        }
        Some(
            self.turns
                .get(&turn)
                .map(|steps| steps.iter().filter_map(|s| s.redaction.clone()).collect())
                .unwrap_or_default(),
        )
    }

    /// ターン `turn` (今の本文 `text`、改訂の番号 `rev`) の印の範囲 (04 §7.3 の「画面への印の範囲」)。指示の無い
    /// ターンは空。指示の伏せ字と今の本文が合わない (当て直しが済んでいない・指示が欠けている・記録が壊れている)
    /// ときは `None` (呼び手は、そのターンを「合わないターン」として扱う、04 §7.2)。
    #[must_use]
    pub fn marks(&self, turn: Id, text: &str, rev: u32, marker: &str) -> Option<Vec<Mark>> {
        if self.invalid_redaction.contains(&turn) || self.invalid_hidden.contains(&turn) {
            return None;
        }
        let Some(steps) = self.turns.get(&turn) else {
            return Some(Vec::new());
        };
        let dirs: Vec<TurnDirective<'_>> = steps
            .iter()
            .map(|s| TurnDirective {
                redaction: s.redaction.as_ref(),
                hidden: &s.hidden,
            })
            .collect();
        display_marks(text, rev, &dirs, marker)
    }

    /// ターンの `[非表示]` の範囲 (今の本文の位置)。印の範囲を求められない (合わないターン) ときは `None`: 呼び手は
    /// そのターンを除く (想起・候補の帯・裏の LLM の呼び出し・書き出し、04 §7.2)。
    #[must_use]
    pub fn hidden_ranges(
        &self,
        turn: Id,
        text: &str,
        rev: u32,
        marker: &str,
    ) -> Option<Vec<Range<usize>>> {
        self.marks(turn, text, rev, marker).map(|marks| {
            marks
                .into_iter()
                .filter(|m| m.kind == MarkKind::Hidden)
                .map(|m| m.range)
                .collect()
        })
    }

    /// ターン `turn` に掛かる指示の ID (指示の ID の順)。
    fn directives_of(&self, turn: Id) -> Vec<Id> {
        self.turns
            .get(&turn)
            .map(|steps| steps.iter().map(|s| s.directive).collect())
            .unwrap_or_default()
    }
}

/// 最も新しい忘れるの指示の ID (読める指示のうち)。ノートの `forgets_seen` に、鍵を作る入力を読んだのと同じ
/// 読み取りで書く。
///
/// # Errors
/// 記録を読めないとき。
pub fn latest_forget(tx: &ReadTxn<'_>) -> Result<Option<Id>, MemoryError> {
    let mut last = None;
    for_each_directive_body(tx, |id, b| {
        if b.op == OP_FORGET {
            last = last.max(Some(id));
        }
    })?;
    Ok(last)
}

/// 忘れたターンと値の鍵 (04 §6 の「候補を見つける時機」「検証の時機」、§7.5 の「忘れると訂正」)。印を解いた指示も
/// 含める (消した訂正を作り直さないため)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForgetScope {
    /// 忘れるの指示の `turns`・`redactions`・`hidden` に入るターン。
    pub turns: BTreeSet<Id>,
    /// 忘れるの指示の `vks`。
    pub vks: BTreeSet<ValueKey>,
}

impl ForgetScope {
    /// 読める忘れるの指示から作る。
    ///
    /// # Errors
    /// 記録を読めないとき。
    pub fn load(tx: &ReadTxn<'_>) -> Result<Self, MemoryError> {
        let mut out = Self::default();
        for_each_directive_body(tx, |_, b| {
            if b.op != OP_FORGET {
                return;
            }
            out.turns.extend(b.turns.iter().copied());
            out.turns.extend(b.redactions.iter().map(|r| r.turn));
            out.turns.extend(b.hidden.iter().map(|h| h.turn));
            out.vks
                .extend(b.vks.iter().filter_map(|h| ValueKey::from_hex(h)));
        })?;
        Ok(out)
    }

    /// 訂正の古い値か新しい値が、忘れた値のどれかか。
    #[must_use]
    pub fn forgets_value(&self, old: &ValueKey, new: &ValueKey) -> bool {
        self.vks.contains(old) || self.vks.contains(new)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 当て直し
// ---------------------------------------------------------------------------------------------------------------

/// 忘れるの当て直しの結果 ([`replay_forgets`])。
#[derive(Debug, Clone, Default)]
pub struct Replay {
    /// 書き込みの操作。DB を変えるときは、`purge_indexes`・`forget_backups` の job と `compact.pending` を同じ
    /// 依頼に入れてある。伏せ字にするターンの `Patch` は、書き手がジャーナルに伏せ字の後の `put` の行を書いてから
    /// コミットする (04 §7.2 の「伏せ字の行を先に書く」)。
    pub ops: Vec<WriteOp>,
    /// 当て直しで伏せ字にしたターン。
    pub patched: Vec<Id>,
    /// 合わないターン (本文が記録と合わない・先の指示が無い・読めない)。本文は変えない。呼び手は、ジャーナルに
    /// 伏せ字の後の番号でハッシュが合う `put` の行があればそれを当て、無ければ想起・候補の帯・裏の LLM の
    /// 呼び出し・書き出しから除いて、状態の行と `doctor` に示す (04 §7.2 の DB の層)。
    pub mismatched: Vec<Id>,
    /// 取り消した事実。
    pub retracted: Vec<Id>,
    /// 消した派生の記録。
    pub deleted: Vec<Id>,
    /// DB の記録を変えたもの (job を書いた忘れるの指示)。
    pub directives: Vec<Id>,
}

impl Replay {
    /// DB を変える操作があるか。
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.ops.is_empty()
    }
}

/// 忘れるの指示だけから DB の層を当て直す (04 §7.2 の「当て直し」・`apply_forget` の DB の層)。何度当てても同じ。
///
/// 1. ターンの本文: そのターンのすべての忘れるの指示を、指示の ID の順に純粋な核で当てる。
/// 2. 取り消し: 印を解く指示の相手になっていない忘れるの指示の印に当たる事実を取り消す (04 §7.4 の繰り上げ)。
/// 3. span: 出典の改訂の番号が伏せ字の前の番号と同じ事実だけを写し直す。
/// 4. 消す派生の記録: 伏せ字にしたターンを出典とし、出典の改訂の番号が今と合わないチャンク・ノート・埋め込み・
///    訂正、`vks` に当たる訂正、伏せ字にしないときの `[非表示]` の範囲を持つ往復の、指示より前に作ったノートと
///    訂正、伏せ字にしたターンの隔離の記録。
///
/// 2〜4 はターンの本文の結果に関わらず当てる (ジャーナルの書き直しの済み具合では飛ばさない)。DB を変えるときは、
/// 同じ操作の列に job と `compact.pending` を足す。呼ぶ側は、事実を書く経路のロックを `tx` を開く前から書き終える
/// まで持つ。
///
/// # Errors
/// 記録を読めない・本体を書けないとき。
pub fn replay_forgets(tx: &ReadTxn<'_>, env: &ForgetEnv<'_>) -> Result<Replay, MemoryError> {
    let mut bodies: Vec<(Id, FactDirectiveBody)> = Vec::new();
    for_each_directive_body(tx, |id, b| {
        if b.op == OP_FORGET {
            bodies.push((id, b));
        }
    })?;
    bodies.sort_by_key(|(id, _)| *id);
    let forgets = TurnForgets::from_bodies(bodies.clone());
    let mut out = Replay::default();
    let mut touched: BTreeSet<Id> = BTreeSet::new();
    let mut ops = Vec::new();

    // 1. ターンの本文。今の改訂の番号 (当てた後) を覚える。
    let mut now_rev: BTreeMap<Id, u32> = BTreeMap::new();
    let mut snaps: BTreeMap<Id, TurnSnap> = BTreeMap::new();
    for turn in forgets.turns() {
        // 合わないターンにするのは、伏せ字の指示のあるターンだけ (`[非表示]` だけのターンは本文を変えない)。
        let redacted = forgets.has_redactions(turn);
        let e = match tx.get(KIND_TURN, turn)? {
            None => continue,
            Some(Read::Ok(e)) => e,
            Some(_) => {
                if redacted {
                    out.mismatched.push(turn);
                }
                continue;
            }
        };
        let Some(s) = snap(&e) else {
            if redacted {
                out.mismatched.push(turn);
            }
            continue;
        };
        if !redacted {
            now_rev.insert(turn, s.rev);
            snaps.insert(turn, s);
            continue;
        }
        let Some(steps) = forgets.redactions(turn) else {
            out.mismatched.push(turn);
            now_rev.insert(turn, s.rev);
            snaps.insert(turn, s);
            continue;
        };
        match apply_to_turn(&e, &steps, env.marker) {
            TurnOutcome::Unchanged => {
                now_rev.insert(turn, s.rev);
            }
            TurnOutcome::Changed { text, rev } => {
                ops.push(crate::write::expect_turn(turn, s.rev));
                ops.push(WriteOp::Patch {
                    kind: KIND_TURN.into(),
                    id: turn,
                    fields: turn_patch(&text, rev)?,
                    updated_at: None,
                });
                out.patched.push(turn);
                touched.extend(forgets.directives_of(turn));
                now_rev.insert(turn, rev);
            }
            TurnOutcome::NeedsEarlier | TurnOutcome::Mismatch => {
                out.mismatched.push(turn);
                now_rev.insert(turn, s.rev);
            }
        }
        snaps.insert(turn, s);
    }

    // 2・3. 事実。
    let before = load_facts(tx)?;
    let dirs = load_directives(tx)?;
    let mut set = before.set.clone();
    let active: BTreeMap<BlockMark, Id> = dirs
        .iter()
        .filter(|d| !dirs.is_unblocked(d.id))
        .filter_map(|d| match &d.kind {
            DirectiveKind::Forget { marks } => Some(marks.iter().map(move |m| (*m, d.id))),
            _ => None,
        })
        .flatten()
        .collect();
    let mut hit: BTreeSet<Id> = BTreeSet::new();
    for f in &set.facts {
        if f.state == FactState::Retracted {
            continue;
        }
        let Some(vk) = f.vk else { continue };
        if let Some(d) = active.get(&block_mark(env.key, &f.subject, &f.attribute, &vk)) {
            hit.insert(f.id);
            touched.insert(*d);
        }
    }
    let retraction = retract(&mut set, &hit, env.attributes);
    for (directive, b) in &bodies {
        for (turn, r) in redactions_of(b) {
            if remap_sources(&mut set, turn, &r, env.marker) > 0 {
                touched.insert(*directive);
            }
        }
    }
    refresh_private(&mut set, &dirs);
    ops.extend(diff_ops(&before, &set, env.now)?);
    out.retracted = retraction.retracted;

    // 4. 消す派生の記録。ターンの改訂の番号は当て直しの後のもので読む。
    let rev_of = |t: Id| -> Option<u32> {
        if let Some(r) = now_rev.get(&t) {
            return Some(*r);
        }
        match tx.get(KIND_TURN, t) {
            Ok(Some(Read::Ok(e))) => turn_rev_checked(&e),
            _ => None,
        }
    };
    let answers_of = |t: Id| -> Vec<Id> {
        tx.links_to(t, REL_REPLY_TO)
            .map(|l| l.into_iter().map(|(a, _)| a).collect())
            .unwrap_or_default()
    };
    let mut purge = Purge::default();
    let redacted_turns: BTreeSet<Id> = bodies
        .iter()
        .flat_map(|(_, b)| b.redactions.iter().map(|r| r.turn))
        .collect();
    for &turn in &redacted_turns {
        let Some(&rev) = now_rev.get(&turn) else {
            continue;
        };
        let ds = forgets.directives_of(turn);
        let n = purge.records.len();
        for (c, _) in tx.links_to(turn, REL_CHUNK_OF)? {
            // 読めない (新しい `v` の) チャンクは本体を解釈せずに残す。
            let Some(Read::Ok(e)) = tx.get(KIND_CHUNK, c)? else {
                continue;
            };
            let Ok(b) = e.body_as::<ChunkBody>() else {
                continue;
            };
            if b.src_rev() != rev {
                purge.with_embeddings(tx, KIND_CHUNK, c)?;
                continue;
            }
            for (emb, _) in tx.links_to(c, REL_EMBEDDING_OF)? {
                if let Some(Read::Ok(x)) = tx.get(KIND_EMBEDDING, emb)?
                    && let Ok(eb) = x.body_as::<EmbeddingBody>()
                    && eb.src_rev.is_some_and(|r| r != rev)
                {
                    purge.add(KIND_EMBEDDING, emb);
                }
            }
        }
        // ノートは、読み出しと同じ判定 (`note_matches`) で、当て直しの後の改訂の番号と合わないものを消す。
        for key in note_keys(turn, snaps.get(&turn)) {
            for (n, _) in tx.links_to(key, REL_NOTE_OF)? {
                if let Some(Read::Ok(e)) = tx.get(KIND_NOTE, n)?
                    && let Ok(b) = e.body_as::<NoteBody>()
                    && !note_matches(&b, rev_of, answers_of)
                {
                    purge.with_embeddings(tx, KIND_NOTE, n)?;
                }
            }
        }
        for (c, _) in tx.links_to(turn, REL_CORRECTION_OF)? {
            if let Some(Read::Ok(e)) = tx.get(KIND_CORRECTION, c)?
                && let Ok(b) = e.body_as::<CorrectionBody>()
                && b.rev != rev
            {
                purge.add(KIND_CORRECTION, c);
            }
        }
        attribute_to(&purge, n, &ds, &mut touched);
    }
    // 伏せ字にしないときの `[非表示]` の範囲を持つ往復のノート (その指示を見ずに作ったもの: `forgets_seen` が無いか
    // 指示より前) と、対象のターンの訂正 (訂正には読んだ指示の印が無いので、指示より前に作ったもの)。
    for (directive, b) in bodies.iter().filter(|(_, b)| !b.redact) {
        let turns: BTreeSet<Id> = b
            .turns
            .iter()
            .copied()
            .chain(b.hidden.iter().map(|h| h.turn))
            .collect();
        let n = purge.records.len();
        for &turn in &b.hidden.iter().map(|h| h.turn).collect::<BTreeSet<Id>>() {
            let s = match snaps.get(&turn) {
                Some(s) => Some(s.clone()),
                None => match read_turn(tx, turn)? {
                    TurnRead::Ok(s) => Some(s),
                    _ => None,
                },
            };
            for key in note_keys(turn, s.as_ref()) {
                for (note, _) in tx.links_to(key, REL_NOTE_OF)? {
                    let seen = match tx.get(KIND_NOTE, note)? {
                        Some(Read::Ok(e)) => {
                            e.body_as::<NoteBody>().ok().and_then(|b| b.forgets_seen)
                        }
                        // 読めない (新しい `v` の) ノートは本体を解釈せずに残す。
                        _ => continue,
                    };
                    if seen.is_none_or(|s| s < *directive) {
                        purge.with_embeddings(tx, KIND_NOTE, note)?;
                    }
                }
            }
        }
        for &turn in &turns {
            for (c, _) in tx.links_to(turn, REL_CORRECTION_OF)? {
                if c < *directive {
                    purge.add(KIND_CORRECTION, c);
                }
            }
        }
        attribute_to(&purge, n, &[*directive], &mut touched);
    }
    // 忘れた値を持つ検証済みの訂正 (どのターンのものも)。
    for (directive, b) in &bodies {
        let vks: BTreeSet<ValueKey> = b.vks.iter().filter_map(|h| ValueKey::from_hex(h)).collect();
        let n = purge.records.len();
        for c in corrections_with_values(tx, &vks)? {
            purge.add(KIND_CORRECTION, c);
        }
        attribute_to(&purge, n, &[*directive], &mut touched);
    }
    // 隔離の記録: 伏せ字の指示のあるターン・消す派生の記録・取り消す事実のもの。
    let quarantine_of: BTreeSet<Id> = redacted_turns
        .iter()
        .copied()
        .chain(purge.ids())
        .chain(out.retracted.iter().copied())
        .collect();
    for q in quarantined_of(tx, &quarantine_of)? {
        purge.add(KIND_QUARANTINE, q);
    }
    out.deleted = purge.ids();
    ops.extend(purge.ops());

    if !ops.is_empty() {
        let mut ids: Vec<Id> = out.deleted.clone();
        ids.extend(out.retracted.iter().copied());
        ids.sort_unstable();
        ids.dedup();
        ops.push(job(env.ids, env.now, JOB_PURGE_INDEXES, ids.clone(), None)?);
        // 変えたものを特定できない (事実の差だけ) ときも、退避は写し直す。
        if touched.is_empty() {
            touched.extend(bodies.iter().map(|(id, _)| *id));
        }
        for d in &touched {
            ops.push(forget_backups_job(env.ids, env.now, *d, ids.clone())?);
        }
        ops.push(WriteOp::SetCompactPending);
        out.directives = touched.into_iter().collect();
    }
    out.mismatched.sort_unstable();
    out.mismatched.dedup();
    out.ops = ops;
    Ok(out)
}

/// `before` 件より後に消す記録が増えていれば、その指示 `ds` を DB を変えたものとして覚える。
fn attribute_to(purge: &Purge, before: usize, ds: &[Id], touched: &mut BTreeSet<Id>) {
    if purge.records.len() > before {
        touched.extend(ds.iter().copied());
    }
}

/// 指示の伏せ字を、ターンごとの記述にする (形の正しくないものは飛ばす。DB の層は核で「合わない」として扱う)。
fn redactions_of(b: &FactDirectiveBody) -> Vec<(Id, Redaction)> {
    let mut per_turn: BTreeMap<Id, Vec<&RedactionBody>> = BTreeMap::new();
    for r in &b.redactions {
        per_turn.entry(r.turn).or_default().push(r);
    }
    per_turn
        .into_iter()
        .filter_map(|(turn, rs)| {
            let first = rs.first()?;
            if rs.iter().any(|r| {
                (r.rev_before, r.rev_after, &r.hash)
                    != (first.rev_before, first.rev_after, &first.hash)
            }) {
                return None;
            }
            Redaction::from_stored(
                first.rev_before,
                first.rev_after,
                rs.iter().map(|r| (u64::from(r.start), u64::from(r.end))),
                &first.hash,
            )
            .ok()
            .map(|r| (turn, r))
        })
        .collect()
}

/// ジャーナルの層の書き直しの対象 (04 §7.2 の「ジャーナルの層の見つけ方」、03 §6)。忘れるの指示の伏せ字の
/// ターンのうち、ジャーナルに集めた `put` の行の改訂の番号の範囲 (`rev_span`、[`evorag_store::Store::journal_rev_span`])
/// が `min < 伏せ字の後の番号 <= max` のものを、その最も大きい伏せ字の後の番号より小さい行を除く書き直しにする
/// (伏せ字の後の行が無ければ、何も除かない)。DB の層は、この結果に関わらず毎回当て直す。
///
/// # Errors
/// 記録を読めないとき。
pub fn journal_cuts(
    tx: &ReadTxn<'_>,
    rev_span: &dyn Fn(Id) -> Option<RevSpan>,
) -> Result<BTreeMap<Id, JournalCut>, MemoryError> {
    let mut best: BTreeMap<Id, u64> = BTreeMap::new();
    for_each_directive_body(tx, |_, b| {
        if b.op != OP_FORGET {
            return;
        }
        for r in &b.redactions {
            let after = u64::from(r.rev_after);
            if let Some(span) = rev_span(r.turn)
                && span.min < after
                && after <= span.max
            {
                let e = best.entry(r.turn).or_insert(after);
                *e = (*e).max(after);
            }
        }
    })?;
    Ok(best
        .into_iter()
        .map(|(id, after)| (id, JournalCut::Below(after)))
        .collect())
}

/// 規則 12 の (1) (03 §4.2): 古い版 (`last_writer` が 0.3.0 より前) がジャーナルから関係を作らずに取り込んだ
/// 事実への指示の、関係と副索引 (`sa`・`vk`・再び覚えない印の `mark`、解く相手への `cancels`) を本体から作り直す
/// 操作。同じ項目を書き直すだけなので、何度当てても同じ。
///
/// # Errors
/// 記録を読めないとき。
pub fn rebuild_directive_indexes(tx: &ReadTxn<'_>) -> Result<Vec<WriteOp>, MemoryError> {
    let mut ops = Vec::new();
    tx.for_each(KIND_FACT_DIRECTIVE, |r| {
        if let Read::Ok(e) = r {
            ops.extend(derive_directive(&e));
        }
        true
    })?;
    Ok(ops)
}

/// すべての忘れるの指示の `forget_backups` の job を作り直す操作 (03 §4.2 の規則 12。restore の後と、
/// `last_writer` が 0.3.0 より前のデータ根を開いたとき)。`compact.pending` も書く。指示が無ければ空。
///
/// # Errors
/// 記録を読めない・本体を書けないとき。
pub fn forget_backups_for_all(
    tx: &ReadTxn<'_>,
    ids: &dyn IdGen,
    now: Timestamp,
) -> Result<Vec<WriteOp>, MemoryError> {
    let mut directives = Vec::new();
    for_each_directive_body(tx, |id, b| {
        if b.op == OP_FORGET {
            directives.push(id);
        }
    })?;
    directives.sort_unstable();
    let mut ops = Vec::new();
    for d in directives {
        ops.push(forget_backups_job(ids, now, d, Vec::new())?);
    }
    if !ops.is_empty() {
        ops.push(WriteOp::SetCompactPending);
    }
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use evorag_store::testutil::TempDir;
    use evorag_store::{
        BodyPatch, LookupRange, OpenOptions, Store, StoreError, WriteError, WriteRequest, Writer,
        WriterHandle,
    };
    use evorag_types::{ManualClock, UuidV7Gen};

    use super::*;
    use crate::correction::{
        PatternMatch, Target as CorrectionTarget, Unverifiable, Verdict, candidate_ops, checked_op,
    };
    use crate::facts::{FactCtx, consolidate};
    use crate::fold::{Candidate, Event, Mood, Tense, VerifiedCorrection};
    use crate::gates::Safety;
    use crate::kinds::{KIND_SESSION, SrcRev};
    use crate::lexical::Signals;
    use crate::mask::{Masker, TurnText};
    use crate::rules::attributes;
    use crate::write::Writes;

    const NOW: Timestamp = Timestamp::from_millis(1_762_650_000_000);
    const M: &str = "[伏せ字]";

    struct Env {
        _dir: TempDir,
        store: Arc<Store>,
        handle: WriterHandle,
        thread: Option<std::thread::JoinHandle<()>>,
        clock: Arc<ManualClock>,
        ids: UuidV7Gen,
        key: ForgetKey,
        session: Id,
    }

    impl Env {
        fn new(name: &str) -> Self {
            let dir = TempDir::new(name);
            let clock = Arc::new(ManualClock::new(NOW));
            let (store, _) = Store::open(OpenOptions {
                data_root: dir.path().to_path_buf(),
                evorag_version: "0.3.0".into(),
                kinds: crate::kinds(),
                clock: clock.clone(),
                ids: Arc::new(UuidV7Gen::with_seed(clock.clone(), 1)),
                config_file: None,
            })
            .expect("open");
            let store = Arc::new(store);
            let (writer, handle) = Writer::new(store.clone()).expect("writer");
            let thread = std::thread::spawn(move || writer.run());
            let ids = UuidV7Gen::with_seed(clock.clone(), 2);
            let session = ids.next_id();
            let mut s = Envelope::new(session, KIND_SESSION, 1, NOW, "");
            s.set_body(&serde_json::json!({
                "title": "t", "mode": "chat", "state": "active", "memory": null,
            }))
            .expect("session");
            let env = Self {
                _dir: dir,
                store,
                handle,
                thread: Some(thread),
                clock,
                ids,
                key: ForgetKey::from_bytes([3; 32]),
                session,
            };
            env.write(vec![WriteOp::Put(s)]).expect("session");
            env
        }

        fn write(&self, ops: Vec<WriteOp>) -> Result<(), WriteError> {
            self.handle
                .submit_wait(WriteRequest::conversation(ops))
                .map(|_| ())
        }

        /// ターンを書く。`rev` は改訂の番号の値 (JSON の値をそのまま。読めない値も書ける)。
        fn turn(&self, role: &str, text: &str, reply_to: Option<Id>, rev: serde_json::Value) -> Id {
            self.clock.advance(1_000);
            let id = self.ids.next_id();
            let mut e = Envelope::new(id, KIND_TURN, 1, NOW, "");
            let mut body = serde_json::json!({
                "session": self.session, "role": role, "state": "confirmed", "text": text,
                "model_key": null, "generation": null, "attached": [], "reply_to": reply_to,
                "error": null,
            });
            if !rev.is_null() {
                body["rev"] = rev;
            }
            e.set_body(&body).expect("turn");
            let mut ops = vec![WriteOp::Put(e.clone())];
            ops.push(WriteOp::PutLink {
                from: id,
                rel: crate::kinds::REL_IN_SESSION.into(),
                to: self.session,
                attrs: Vec::new(),
            });
            if let Some(u) = reply_to {
                ops.push(WriteOp::PutLink {
                    from: id,
                    rel: REL_REPLY_TO.into(),
                    to: u,
                    attrs: Vec::new(),
                });
            }
            self.write(ops).expect("turn");
            id
        }

        fn fact(&self, turn: Id, text: &str, attribute: &str, value: &str) {
            self.fact_span(turn, 0..text.len(), attribute, value);
        }

        /// 根拠の範囲 `span` (保存した本文の位置) の候補を当てる。
        fn fact_span(&self, turn: Id, span: Range<usize>, attribute: &str, value: &str) {
            self.fact_at(turn, span, 0, attribute, value);
        }

        /// 改訂の番号 `rev` の本文の根拠の範囲 `span` の候補を当てる。
        fn fact_at(&self, turn: Id, span: Range<usize>, rev: u32, attribute: &str, value: &str) {
            let safety = Safety::bundled();
            let ctx = FactCtx {
                key: &self.key,
                attributes: attributes(),
                safety: &safety,
                ids: &self.ids,
                now: NOW,
            };
            let c = Candidate {
                source: Source {
                    turn,
                    start: u32::try_from(span.start).expect("short"),
                    end: u32::try_from(span.end).expect("short"),
                    rev,
                },
                subject: "self".into(),
                attribute: attribute.into(),
                value: value.into(),
                vk: value_key(&self.key, value),
                mood: Mood::Fact,
                tense: Tense::Current,
                old_vk: None,
                signals: Signals::default(),
            };
            let ops = {
                let tx = self.store.read().expect("read");
                consolidate(&tx, &ctx, vec![Event::Candidate(c)])
                    .expect("consolidate")
                    .0
            };
            self.write(ops).expect("fact");
        }

        fn forget_env(&self) -> ForgetEnv<'_> {
            ForgetEnv {
                key: &self.key,
                attributes: attributes(),
                marker: M,
                hidden_marker: "[非表示]",
                ids: &self.ids,
                now: NOW,
            }
        }

        fn row(&self, attribute: &str) -> FactKey {
            let tx = self.store.read().expect("read");
            let set = load_facts(&tx).expect("facts").set;
            let f = set
                .facts
                .iter()
                .find(|f| f.attribute == attribute && f.state == FactState::Live)
                .expect("a live fact");
            row_key(f).expect("row")
        }

        fn plan(&self, attribute: &str, options: ForgetOptions) -> Result<ForgetPlan, ForgetAbort> {
            let row = self.row(attribute);
            let tx = self.store.read().expect("read");
            plan_forget(&tx, &self.forget_env(), &row, options).expect("plan")
        }

        fn replay(&self) -> Replay {
            let r = {
                let tx = self.store.read().expect("read");
                replay_forgets(&tx, &self.forget_env()).expect("replay")
            };
            if r.changed() {
                self.write(r.ops.clone()).expect("replay ops");
            }
            r
        }

        fn exists(&self, kind: &str, id: Id) -> bool {
            let tx = self.store.read().expect("read");
            tx.get(kind, id).expect("get").is_some()
        }

        fn text(&self, id: Id) -> (String, u32) {
            let tx = self.store.read().expect("read");
            let Some(Read::Ok(e)) = tx.get(KIND_TURN, id).expect("get") else {
                panic!("turn {id}");
            };
            let s = snap(&e).expect("readable");
            (s.text, s.rev)
        }

        fn writes(&self) -> Writes<'_> {
            Writes {
                clock: &*self.clock,
                ids: &self.ids,
            }
        }

        fn jobs(&self) -> Vec<JobBody> {
            let tx = self.store.read().expect("read");
            let mut out = Vec::new();
            tx.for_each(KIND_JOB, |r| {
                if let Read::Ok(e) = r
                    && let Ok(b) = e.body_as::<JobBody>()
                {
                    out.push(b);
                }
                true
            })
            .expect("jobs");
            out
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            self.handle.shutdown();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn pattern() -> PatternMatch {
        PatternMatch {
            pattern: "{old}じゃなくて{new}".into(),
            dir_old: Some("大阪".into()),
            dir_new: "東京".into(),
        }
    }

    fn correction(env: &Env, turn: Id, rev: u32) -> Id {
        let body = CorrectionBody::candidate(turn, rev, None, &pattern());
        let ops = {
            let tx = env.store.read().expect("read");
            candidate_ops(&tx, &env.ids, NOW, &body)
                .expect("read")
                .expect("a candidate")
        };
        let WriteOp::Put(e) = &ops[0] else {
            panic!("a put first");
        };
        let id = e.id;
        env.write(ops).expect("correction");
        id
    }

    fn verified(env: &Env, turn: Id, old: &str, new: &str) -> Verdict {
        let src = |s: u32, e: u32| Source {
            turn,
            start: s,
            end: e,
            rev: 0,
        };
        Verdict::Verified(VerifiedCorrection::assume_verified(
            src(0, 6),
            Some(src(6, 12)),
            CorrectionTarget::Fact,
            old.into(),
            value_key(&env.key, old),
            new.into(),
            value_key(&env.key, new),
        ))
    }

    /// 忘れるは、伏せ字にしたターンの派生の記録 (チャンク・ノート・埋め込み・訂正) と、忘れた値を持つ訂正を同じ
    /// コミットで消し、`purge_indexes`・`forget_backups` の job と `compact.pending` を書く。読んでから書くまでに
    /// 忘れるが入った派生の記録の書き込みは断られる。忘れたターンからは訂正の候補を書かず、検証の結果が忘れた
    /// 値なら値を書かずに `forgotten` で落とす。
    /// verifies: 04 §7.1 の表、§7.2 の表、§7.5 の「忘れると訂正」、§6 の「候補を見つける時機」「検証の時機」、
    /// §7.7 の 4
    #[test]
    fn a_forget_removes_derived_records_and_stops_racing_writes() {
        let env = Env::new("forget-derived");
        let text = "私は大阪に住んでいて、趣味は登山です";
        let u1 = env.turn("user", text, None, serde_json::Value::Null);
        let a1 = env.turn("assistant", "大阪にお住まいなんですね", Some(u1), 0.into());
        let u2 = env.turn(
            "user",
            "大阪じゃなくて東京です",
            None,
            serde_json::Value::Null,
        );
        let u3 = env.turn("user", "犬が好き", None, serde_json::Value::Null);
        env.fact(u1, text, "residence", "大阪");
        // 派生の記録。
        let w = env.writes();
        let chunk = ChunkBody {
            turn: u1,
            session: env.session,
            start: 0,
            end: 9,
            rules: 1,
            src_rev: Some(0),
        };
        let (made, ops) = w.chunks(&[], &[chunk]).expect("chunks");
        env.write(ops).expect("chunk");
        let chunk_id = made[0].0;
        let emb = |target: Id| EmbeddingBody {
            target,
            unit: crate::kinds::UNIT_CHUNK.into(),
            session: env.session,
            model_key: "m".into(),
            dim: 1,
            key: None,
            src_rev: Some(0),
        };
        let (chunk_emb, ops) = w.embedding(&emb(chunk_id), &[0.5]).expect("embedding");
        env.write(ops).expect("embedding");
        let note = NoteBody {
            session: env.session,
            exchanges: vec![u1],
            keys: vec!["住まい".into()],
            model_key: "m".into(),
            rules: 1,
            src_revs: vec![SrcRev { turn: u1, rev: 0 }, SrcRev { turn: a1, rev: 0 }],
            forgets_seen: None,
        };
        let (note_id, ops) = w.note(&[], &[], &note).expect("note");
        env.write(ops).expect("note");
        let c_u1 = correction(&env, u1, 0);
        let c_u2 = correction(&env, u2, 0);
        let checked = {
            let tx = env.store.read().expect("read");
            checked_op(&tx, c_u2, verified(&env, u2, "大阪", "東京"), NOW)
                .expect("read")
                .expect("an op")
        };
        assert!(matches!(checked.1, Verdict::Verified(_)));
        env.write(vec![checked.0]).expect("verified");
        // 忘れるの前に作った (古い本文を読んだ) 派生の記録の書き込み。
        let racing_note = w.note(&[], &[], &note).expect("note").1;
        let racing_chunk = {
            let (_, mut ops) = w.chunks(&[], &[chunk]).expect("chunks");
            ops.push(crate::write::expect_turn(u1, 0));
            ops
        };

        let plan = env
            .plan("residence", ForgetOptions::default())
            .expect("a plan");
        assert_eq!(plan.redacted_turns, {
            let mut v = vec![u1, a1];
            v.sort_unstable();
            v
        });
        assert_eq!(plan.preview.samples.len(), 2);
        env.write(plan.ops.clone()).expect("forget");

        assert_eq!(
            env.text(u1),
            ("私は[伏せ字]に住んでいて、趣味は登山です".into(), 1)
        );
        assert_eq!(env.text(a1), ("[伏せ字]にお住まいなんですね".into(), 1));
        for (kind, id) in [
            (KIND_CHUNK, chunk_id),
            (KIND_EMBEDDING, chunk_emb),
            (KIND_NOTE, note_id),
            (KIND_CORRECTION, c_u1),
            (KIND_CORRECTION, c_u2),
        ] {
            assert!(!env.exists(kind, id), "{kind} {id} must be gone");
            assert!(plan.derived.contains(&id), "{kind} {id} in the plan");
        }
        let jobs = env.jobs();
        let backups = jobs
            .iter()
            .find(|j| j.task == JOB_FORGET_BACKUPS)
            .expect("a forget_backups job");
        assert_eq!(backups.directive, Some(plan.directive));
        assert!(backups.ids.contains(&chunk_id));
        let index = jobs
            .iter()
            .find(|j| j.task == JOB_PURGE_INDEXES)
            .expect("a purge_indexes job");
        assert!(index.ids.contains(&plan.retracted[0]), "the fact key");
        assert!(env.store.compact_pending().expect("meta"));
        // 読んでから書くまでに忘れるが入った書き込みは断られる。
        for ops in [racing_note, racing_chunk] {
            assert!(
                matches!(
                    env.write(ops),
                    Err(WriteError::Store(StoreError::Stale { .. }))
                ),
                "a stale write must be refused"
            );
        }
        // 忘れたターンからは訂正の候補を書かない。ほかのターンからは書く。
        {
            let tx = env.store.read().expect("read");
            let body = CorrectionBody::candidate(u1, 1, None, &pattern());
            assert_eq!(
                candidate_ops(&tx, &env.ids, NOW, &body)
                    .expect("read")
                    .err(),
                Some(crate::correction::CandidateSkip::Forgotten)
            );
            let body = CorrectionBody::candidate(u3, 0, None, &pattern());
            assert!(
                candidate_ops(&tx, &env.ids, NOW, &body)
                    .expect("read")
                    .is_ok()
            );
        }
        // 検証の結果が忘れた値なら、値を書かずに `forgotten` で落とす。
        let c_u3 = correction(&env, u3, 0);
        let (op, verdict) = {
            let tx = env.store.read().expect("read");
            checked_op(&tx, c_u3, verified(&env, u3, "大阪", "京都"), NOW)
                .expect("read")
                .expect("an op")
        };
        assert_eq!(verdict, Verdict::Unverifiable(Unverifiable::Forgotten));
        let WriteOp::Put(e) = op else {
            panic!("a put");
        };
        let b: CorrectionBody = e.body_as().expect("body");
        assert_eq!(b.rejected.as_deref(), Some("forgotten"));
        assert_eq!(
            (b.dir_old, b.dir_new, b.old, b.new, b.old_vk, b.new_vk),
            (None, None, None, None, None, None)
        );
        // 当て直しは何も変えない。
        assert!(!env.replay().changed());
    }

    /// 伏せ字にするとき、出典のターンが読めない・出典の改訂の番号が今と合わないなら中止する。伏せ字にしないとき
    /// は中止せず、範囲を求められないターンを飛ばして数える。
    /// verifies: 04 §7.1 の「中止」、§7.3
    #[test]
    fn unreadable_or_stale_sources_abort_a_redacting_forget() {
        let env = Env::new("forget-abort");
        let text = "趣味は登山です";
        let bad = env.turn("user", text, None, "not a number".into());
        env.fact(bad, text, "hobby", "登山");
        assert_eq!(
            env.plan("hobby", ForgetOptions::default()).err(),
            Some(ForgetAbort::UnreadableTurn(bad))
        );
        let facts_only = env
            .plan(
                "hobby",
                ForgetOptions {
                    redact: false,
                    ..ForgetOptions::default()
                },
            )
            .expect("a facts-only plan");
        assert!(facts_only.hidden_turns.is_empty());
        assert_eq!(facts_only.preview.skipped_unreadable, 1);
        assert_eq!(facts_only.retracted.len(), 1);

        let env = Env::new("forget-stale");
        let t = env.turn("user", text, None, serde_json::Value::Null);
        env.fact(t, text, "hobby", "登山");
        // ターンの改訂の番号だけが進んだ (事実の出典は古い番号のまま)。
        env.write(vec![WriteOp::Patch {
            kind: KIND_TURN.into(),
            id: t,
            fields: turn_patch(text, 1).expect("patch"),
            updated_at: None,
        }])
        .expect("patch");
        assert_eq!(
            env.plan("hobby", ForgetOptions::default()).err(),
            Some(ForgetAbort::StaleSource(t))
        );
        // 計画の後に会話が増えても、計画のハッシュは変わらない (ほかのターンの数は覆わない)。
        let env = Env::new("forget-hash");
        let t = env.turn("user", text, None, serde_json::Value::Null);
        env.fact(t, text, "hobby", "登山");
        let before = env.plan("hobby", ForgetOptions::default()).expect("plan");
        env.turn("user", "登山の本を読んだ", None, serde_json::Value::Null);
        let after = env.plan("hobby", ForgetOptions::default()).expect("plan");
        assert_eq!(before.hash, after.hash);
        assert_eq!(
            (before.preview.other_turns, after.preview.other_turns),
            (0, 1)
        );
        // ほかのターンも伏せる選択では、範囲が変わるのでハッシュが変わる。
        let with_others = env
            .plan(
                "hobby",
                ForgetOptions {
                    others: true,
                    ..ForgetOptions::default()
                },
            )
            .expect("plan");
        assert_ne!(with_others.hash, after.hash);
    }

    /// 当て直しは、伏せ字の前の番号から作った派生の記録を消し、本文の合わないターンを報告する。ジャーナルの層は、
    /// 伏せ字の後の行があるターンだけを書き直しの対象にする。
    /// verifies: 04 §7.2 の「当て直しの一覧」の 4、DB の層、ジャーナルの層
    #[test]
    fn replay_drops_stale_derived_records_and_reports_mismatches() {
        let env = Env::new("forget-replay");
        let text = "私は大阪に住んでいます";
        let u1 = env.turn("user", text, None, serde_json::Value::Null);
        env.fact(u1, text, "residence", "大阪");
        let plan = env
            .plan(
                "residence",
                ForgetOptions {
                    echo: false,
                    ..ForgetOptions::default()
                },
            )
            .expect("plan");
        env.write(plan.ops).expect("forget");
        // 古い版が、伏せ字の前の本文から作ったチャンク (`src_rev` が無い = 0)。
        let w = env.writes();
        let old = ChunkBody {
            turn: u1,
            session: env.session,
            start: 0,
            end: 6,
            rules: 1,
            src_rev: None,
        };
        let (made, ops) = w.chunks(&[], &[old]).expect("chunks");
        env.write(ops).expect("chunk");
        let r = env.replay();
        assert!(r.changed());
        assert_eq!(r.deleted, [made[0].0]);
        assert!(!env.exists(KIND_CHUNK, made[0].0));
        assert!(!env.replay().changed(), "idempotent");
        // 伏せ字の後の番号で本文が合わないターン。
        env.write(vec![WriteOp::Patch {
            kind: KIND_TURN.into(),
            id: u1,
            fields: BodyPatch::new().set("text", &"別の本文").expect("patch"),
            updated_at: None,
        }])
        .expect("tamper");
        let r = env.replay();
        assert_eq!(r.mismatched, [u1]);
        assert!(r.patched.is_empty());
        // ジャーナルの層: 伏せ字の後の番号 (1) の行があり、前の行も残っているときだけ。
        let tx = env.store.read().expect("read");
        let cut = |min, max| {
            journal_cuts(&tx, &|t| (t == u1).then_some(RevSpan { min, max })).expect("cuts")
        };
        assert_eq!(cut(0, 1).get(&u1), Some(&JournalCut::Below(1)));
        assert!(cut(1, 1).is_empty(), "already rewritten");
        assert!(cut(0, 0).is_empty(), "no redacted line yet");
    }

    /// 伏せ字の範囲に値が掛かったほかの事実 (自動では取り消さない) を後から忘れるときは、値の一部がもう伏せ字
    /// なので、その出典を伏せ字にせずに見本に示し、事実を取り消す (中止しない)。
    /// verifies: 04 §7.1 の「影響するほかの事実」
    #[test]
    fn an_affected_fact_can_be_forgotten_later() {
        let env = Env::new("forget-affected");
        let text = "大阪のたこ焼きが好きで、大阪に住んでいます";
        let t = env.turn("user", text, None, serde_json::Value::Null);
        env.fact(t, text, "residence", "大阪");
        // 「大阪のたこ焼き」の根拠は先頭の 30 バイト (「大阪のたこ焼きが好き」)。
        let likes_end = text.find("で、").expect("comma");
        {
            let safety = Safety::bundled();
            let ctx = FactCtx {
                key: &env.key,
                attributes: attributes(),
                safety: &safety,
                ids: &env.ids,
                now: NOW,
            };
            let c = Candidate {
                source: Source {
                    turn: t,
                    start: 0,
                    end: u32::try_from(likes_end).expect("short"),
                    rev: 0,
                },
                subject: "self".into(),
                attribute: "likes".into(),
                value: "大阪のたこ焼き".into(),
                vk: value_key(&env.key, "大阪のたこ焼き"),
                mood: Mood::Fact,
                tense: Tense::Current,
                old_vk: None,
                signals: Signals::default(),
            };
            let ops = {
                let tx = env.store.read().expect("read");
                consolidate(&tx, &ctx, vec![Event::Candidate(c)])
                    .expect("consolidate")
                    .0
            };
            env.write(ops).expect("likes");
        }
        let first = env
            .plan("residence", ForgetOptions::default())
            .expect("plan");
        assert_eq!(first.preview.affected.len(), 1, "the likes row");
        env.write(first.ops).expect("forget residence");
        let second = env.plan("likes", ForgetOptions::default()).expect("plan");
        assert_eq!(second.preview.unredacted_sources, [t]);
        assert!(second.redacted_turns.is_empty());
        assert_eq!(second.retracted.len(), 1);
        env.write(second.ops).expect("forget likes");
        assert_eq!(
            env.text(t),
            (
                "[伏せ字]のたこ焼きが好きで、[伏せ字]に住んでいます".into(),
                1
            )
        );
        assert!(!env.replay().changed());
    }

    fn redact_off() -> ForgetOptions {
        ForgetOptions {
            redact: false,
            ..ForgetOptions::default()
        }
    }

    /// 出典の発話の、根拠の範囲の外の一致も、同じ往復の応答と同じ選択 (既定で伏せる) で伏せ、見本に数える。
    /// verifies: 04 §7.1 の確認の画面 (出典の発話の根拠の範囲の外)
    #[test]
    fn matches_outside_the_evidence_span_follow_the_echo_choice() {
        let env = Env::new("forget-outside");
        let text = "私は大阪に住んでいます。大阪はいい街です";
        let t = env.turn("user", text, None, serde_json::Value::Null);
        let first = text.find('。').expect("period");
        env.fact_span(t, 0..first, "residence", "大阪");
        let no_echo = env
            .plan(
                "residence",
                ForgetOptions {
                    echo: false,
                    ..ForgetOptions::default()
                },
            )
            .expect("plan");
        assert_eq!(no_echo.preview.source_outside, 1);
        assert_eq!(
            no_echo.preview.samples[0].after,
            "私は[伏せ字]に住んでいます。大阪はいい街です"
        );
        let plan = env
            .plan("residence", ForgetOptions::default())
            .expect("plan");
        assert_eq!(plan.preview.source_outside, 1);
        assert_ne!(plan.hash, no_echo.hash);
        env.write(plan.ops).expect("forget");
        assert_eq!(
            env.text(t),
            ("私は[伏せ字]に住んでいます。[伏せ字]はいい街です".into(), 1)
        );
        assert!(!env.replay().changed());
    }

    /// 伏せ字にしないとき、根拠の範囲に値の一致が無い出典は、根拠の範囲の全体を `[非表示]` にする。伏せ字にする
    /// ときは中止する。
    /// verifies: 04 §7.1 (伏せ字にしないときの `[非表示]`)、§7.3
    #[test]
    fn a_span_without_the_value_is_hidden_whole_when_not_redacting() {
        let env = Env::new("forget-whole-span");
        let text = "趣味は登山です。週末は山に行きます";
        let t = env.turn("user", text, None, serde_json::Value::Null);
        let span = text.find("週末").expect("span")..text.len();
        env.fact_span(t, span.clone(), "hobby", "登山");
        assert_eq!(
            env.plan("hobby", ForgetOptions::default()).err(),
            Some(ForgetAbort::ValueMismatch(t))
        );
        let plan = env
            .plan(
                "hobby",
                ForgetOptions {
                    echo: false,
                    ..redact_off()
                },
            )
            .expect("plan");
        assert_eq!(plan.hidden_turns, [t]);
        assert_eq!(plan.preview.samples[0].after, "趣味は登山です。[非表示]");
    }

    /// 伏せ字にするターンが前の忘れるの指示と合わない (合わないターン) なら、伏せ字にするときは中止し、伏せ字に
    /// しないときは飛ばして数える。`[非表示]` だけのターンは、読めなくても合わないターンにしない。
    /// verifies: 04 §7.2 の「合わないターン」
    #[test]
    fn mismatched_turns_abort_a_redaction_and_are_skipped_otherwise() {
        let env = Env::new("forget-mismatched");
        let text = "趣味は登山です。私は大阪に住んでいます";
        let t = env.turn("user", text, None, serde_json::Value::Null);
        let first = text.find('。').expect("period");
        env.fact_span(t, first + "。".len()..text.len(), "residence", "大阪");
        // 伏せ字にしないで忘れる (改訂の番号 0 の `[非表示]` の範囲)。
        let plan = env.plan("residence", redact_off()).expect("plan");
        env.write(plan.ops).expect("forget");
        // 伏せ字の指示なしに改訂の番号だけが進んだターン (指示と合わない)。
        env.write(vec![WriteOp::Patch {
            kind: KIND_TURN.into(),
            id: t,
            fields: turn_patch(text, 1).expect("patch"),
            updated_at: None,
        }])
        .expect("bump");
        env.fact_at(t, 0..first, 1, "hobby", "登山");
        assert_eq!(
            env.plan("hobby", ForgetOptions::default()).err(),
            Some(ForgetAbort::MismatchedTurn(t))
        );
        let off = env.plan("hobby", redact_off()).expect("plan");
        assert_eq!(off.preview.mismatched_turns, [t]);
        assert!(off.hidden_turns.is_empty());

        // `[非表示]` だけのターンは、改訂の番号が読めなくなっても合わないターンにしない。
        let env = Env::new("forget-hidden-only");
        let text = "趣味は登山です";
        let t = env.turn("user", text, None, serde_json::Value::Null);
        env.fact(t, text, "hobby", "登山");
        let plan = env.plan("hobby", redact_off()).expect("plan");
        assert_eq!(plan.hidden_turns, [t]);
        env.write(plan.ops).expect("forget");
        env.write(vec![WriteOp::Patch {
            kind: KIND_TURN.into(),
            id: t,
            fields: BodyPatch::new().set("rev", &"bad").expect("patch"),
            updated_at: None,
        }])
        .expect("tamper");
        let r = {
            let tx = env.store.read().expect("read");
            replay_forgets(&tx, &env.forget_env()).expect("replay")
        };
        assert!(r.mismatched.is_empty(), "{r:?}");
    }

    /// 2 つの応答のうち古い方だけを伏せ字にした往復で、新しい応答から作り直したノートは、当て直しで消さない
    /// (読み出しと同じ判定)。伏せ字にしないときは、`forgets_seen` が無いか指示より前のノートだけを消す。
    /// verifies: 04 §7.2 の当て直しの一覧の 4、§7.7 の 1
    #[test]
    fn replay_keeps_notes_that_match_and_drops_notes_made_without_the_directive() {
        let env = Env::new("forget-notes");
        let text = "私は大阪に住んでいます";
        let u1 = env.turn("user", text, None, serde_json::Value::Null);
        let _a1 = env.turn("assistant", "大阪ですね", Some(u1), 0.into());
        let a2 = env.turn("assistant", "いいですね", Some(u1), 0.into());
        env.fact(u1, text, "residence", "大阪");
        let plan = env
            .plan("residence", ForgetOptions::default())
            .expect("plan");
        env.write(plan.ops).expect("forget");
        let w = env.writes();
        let note = |seen: Option<Id>, src_revs: Vec<SrcRev>| NoteBody {
            session: env.session,
            exchanges: vec![u1],
            keys: vec!["住まい".into()],
            model_key: "m".into(),
            rules: 1,
            src_revs,
            forgets_seen: seen,
        };
        let rebuilt = note(
            Some(plan.directive),
            vec![SrcRev { turn: u1, rev: 1 }, SrcRev { turn: a2, rev: 0 }],
        );
        let (kept, ops) = w.note(&[], &[], &rebuilt).expect("note");
        env.write(ops).expect("note");
        assert!(!env.replay().changed(), "the rebuilt note matches");
        assert!(env.exists(KIND_NOTE, kept));

        let env = Env::new("forget-hidden-notes");
        let u1 = env.turn("user", text, None, serde_json::Value::Null);
        env.fact(u1, text, "residence", "大阪");
        let plan = env.plan("residence", redact_off()).expect("plan");
        env.write(plan.ops).expect("forget");
        let w = env.writes();
        let unaware = note(None, vec![SrcRev { turn: u1, rev: 0 }]);
        let aware = note(Some(plan.directive), vec![SrcRev { turn: u1, rev: 0 }]);
        let (old_id, ops) = w.note(&[], &[], &unaware).expect("note");
        env.write(ops).expect("note");
        let (new_id, ops) = w.note(&[], &[], &aware).expect("note");
        env.write(ops).expect("note");
        let r = env.replay();
        assert_eq!(r.deleted, [old_id]);
        assert!(env.exists(KIND_NOTE, new_id));
        assert!(!env.replay().changed());
        // 最も新しい忘れるの指示。
        let tx = env.store.read().expect("read");
        assert_eq!(latest_forget(&tx).expect("read"), Some(plan.directive));
    }

    /// 規則 12 の (1): 古い版が関係を作らずに取り込んだ指示の副索引 (`mark` など) を作り直す。
    /// verifies: 03 §4.2 の規則 12 の (1)
    #[test]
    fn directive_indexes_are_rebuilt_from_bodies() {
        let env = Env::new("forget-indexes");
        let mark = "ab".repeat(32);
        let body = serde_json::json!({
            "op": "forget", "subject": "self", "attribute": "hobby", "vk": null, "norm": 1,
            "fact": null, "target": null, "scope": null, "marks": [mark], "turns": [],
            "redactions": [], "hidden": [], "history": false, "redact": false,
        });
        let id = env.ids.next_id();
        let mut e = Envelope::new(id, KIND_FACT_DIRECTIVE, 1, NOW, "");
        e.set_body(&body).expect("body");
        env.write(vec![WriteOp::Put(e)]).expect("directive");
        let marks = |env: &Env| {
            let tx = env.store.read().expect("read");
            let all = LookupRange::Between {
                from: None,
                to: None,
            };
            tx.lookup(KIND_FACT_DIRECTIVE, "mark", &all, false, None)
                .expect("lookup")
                .len()
        };
        assert_eq!(marks(&env), 0);
        let ops = {
            let tx = env.store.read().expect("read");
            rebuild_directive_indexes(&tx).expect("ops")
        };
        env.write(ops.clone()).expect("rebuild");
        assert_eq!(marks(&env), 1);
        env.write(ops).expect("again");
        assert_eq!(marks(&env), 1);
        let jobs = {
            let tx = env.store.read().expect("read");
            forget_backups_for_all(&tx, &env.ids, NOW).expect("jobs")
        };
        assert_eq!(jobs.len(), 2, "one job and compact.pending");
    }

    /// `[非表示]` の範囲は、その指示より後の伏せ字で今の本文の位置に写して伏せる。
    /// verifies: 04 §7.3 の「位置の写し直し」(`[非表示]` の範囲)、§7.5 の「想起での private」
    #[test]
    fn hidden_ranges_follow_later_redactions() {
        let turn = Id::from_bytes([5; 16]);
        let text = "aa XX bb";
        let bb = text.find("bb").expect("bb");
        let (r, t1) = Redaction::plan(text, 0, std::iter::once(3..5), M).expect("plan");
        let body = |op_hidden: Vec<HiddenBody>, reds: Vec<RedactionBody>| FactDirectiveBody {
            op: OP_FORGET.into(),
            subject: "self".into(),
            attribute: "hobby".into(),
            vk: None,
            norm: Some(NORM_V1),
            fact: None,
            target: None,
            scope: None,
            marks: Vec::new(),
            vks: Vec::new(),
            turns: Vec::new(),
            redactions: reds,
            hidden: op_hidden,
            history: false,
            redact: false,
            origin: ORIGIN_USER.into(),
        };
        let forgets = TurnForgets::from_bodies(vec![
            (
                Id::from_bytes([1; 16]),
                body(
                    vec![HiddenBody {
                        turn,
                        rev: 0,
                        start: u32::try_from(bb).expect("short"),
                        end: u32::try_from(bb + 2).expect("short"),
                    }],
                    Vec::new(),
                ),
            ),
            (
                Id::from_bytes([2; 16]),
                body(
                    Vec::new(),
                    vec![RedactionBody {
                        turn,
                        rev_before: 0,
                        rev_after: 1,
                        start: 3,
                        end: 5,
                        hash: r.hash().to_string(),
                    }],
                ),
            ),
        ]);
        let m = Masker::new(&FactSet::default(), forgets.clone(), M);
        let masked = m.mask(
            &[TurnText {
                id: turn,
                rev: 1,
                text: &t1,
            }],
            "[非表示]",
        );
        assert_eq!(masked, [format!("aa {M} [非表示]")]);
        assert_eq!(
            m.mask_hidden(
                &[TurnText {
                    id: turn,
                    rev: 1,
                    text: &t1
                }],
                "[非表示]"
            ),
            masked
        );
        let marks = forgets.marks(turn, &t1, 1, M).expect("marks");
        assert_eq!(marks.len(), 2);
        // 伏せ字がまだ当たっていない本文では印を求めない (合わないターン)。呼び手は除き、伏せるときは全体を伏せる。
        assert_eq!(forgets.marks(turn, text, 0, M), None);
        let stale = TurnText {
            id: turn,
            rev: 0,
            text,
        };
        assert!(m.excluded(&stale));
        assert_eq!(m.mask(&[stale], "[非表示]"), ["[非表示]"]);
    }
}
