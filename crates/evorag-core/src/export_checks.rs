//! 書き出しと `doctor` の、メモリの層の確かめ (03 §11 の「private と忘れる」「派生の記録の確かめ」、04 §7.2・
//! §7.7 の 1)。
//!
//! 書き出しと `doctor` はデータ根を読むだけで開く (TUI を閉じてから行うので、自分では当て直さない)。ストアが
//! 開いた読み取り ([`ExportView`]) で、次を判定する。
//! - **断る**: 当て直しの済んでいない忘れる (当て直しで伏せ字にできるターン、再び覚えない印に当たる事実) がある。
//! - **含めない**: 忘れるの指示と合わないターン、出典が DB に無い派生の記録 (03 §4.2 の規則 12)、出典の改訂の
//!   番号が今のターンと合わない派生の記録、当て直しが消す訂正とノート。private の事実は除かない (03 §11)。
//!
//! 伏せ字の印はプロンプトの言語で決まるが、書き出しは `meta` を読まないので、どれかの印で合えば合うとみなす。

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};

use evorag_memory::{
    CorrectionBody, FactBody, FactDirectiveBody, KIND_CORRECTION, KIND_FACT, KIND_FACT_DIRECTIVE,
    KIND_NOTE, KIND_RECALL_FEEDBACK, KIND_SESSION, KIND_TURN, NoteBody, OP_FORGET, OP_UNBLOCK,
    REDACTION_MARKERS, RecallFeedbackBody, TurnBody, TurnForgets, TurnRole, ValueKey, note_matches,
    redaction_marker, turn_rev_checked,
};
use evorag_store::Envelope;
use evorag_store::Read;
use evorag_store::export::{ExportChecks, ExportView};
use evorag_types::Id;

use crate::forgets::TurnCheck;

/// 書き出しの確かめ ([`ExportChecks`])。最初に呼ばれたときに、同じ読み取りで材料を集める。
#[derive(Default)]
pub struct ExportFilter {
    prepared: OnceCell<Option<Prepared>>,
}

impl ExportFilter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn prepared(&self, view: &ExportView<'_>) -> Option<&Prepared> {
        self.prepared
            .get_or_init(|| Prepared::load(view, &REDACTION_MARKERS))
            .as_ref()
    }
}

impl std::fmt::Debug for ExportFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportFilter").finish_non_exhaustive()
    }
}

impl ExportChecks for ExportFilter {
    /// 当て直しの済んでいない忘れるがあるか。記録を読めなければ真 (伏せ字の前の本文を書き出さない側に倒す)。
    fn pending_forget(&self, view: &ExportView<'_>) -> bool {
        self.prepared(view).is_none_or(|p| p.pending)
    }

    fn include(&self, view: &ExportView<'_>, env: &Envelope) -> bool {
        self.prepared(view).is_some_and(|p| p.include(view, env))
    }
}

/// `doctor` に示す、忘れるの状態。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForgetCheck {
    /// 忘れるの指示と合わないターンの数 (想起・裏の処理・書き出しから除いている、04 §7.2)。
    pub mismatched: usize,
    /// 当て直しの済んでいない忘れるがある (次の起動で当て直す)。
    pub pending: bool,
}

/// 忘れるの状態を数える (`doctor`)。`prompt_language` はデータ根の `prompt.language` (無ければどの印でもよい)。
/// 記録を読めなければ `None`。
#[must_use]
pub fn forget_check(view: &ExportView<'_>, prompt_language: Option<&str>) -> Option<ForgetCheck> {
    let one;
    let markers: &[&str] = match prompt_language {
        Some(l) => {
            one = [redaction_marker(l)];
            &one
        }
        None => &REDACTION_MARKERS,
    };
    let p = Prepared::load(view, markers)?;
    Some(ForgetCheck {
        mismatched: p.excluded.len(),
        pending: p.pending,
    })
}

/// 伏せ字にしないで忘れた指示 1 つの、当て直しが消す訂正とノートの材料。
struct Unredacted {
    directive: Id,
    turns: BTreeSet<Id>,
    exchanges: BTreeSet<Id>,
}

/// 書き出しの判定の材料。
struct Prepared {
    /// 当て直しの済んでいない忘れるがある。
    pending: bool,
    /// 忘れるの指示と合わないターン。
    excluded: BTreeSet<Id>,
    /// すべての忘れるの指示の `vks` (訂正)。
    vks: BTreeSet<ValueKey>,
    /// 印を解いていない忘れるの指示の (主語, 属性, 値の鍵) (事実)。
    forgotten: BTreeSet<(String, String, ValueKey)>,
    unredacted: Vec<Unredacted>,
    /// 発話 → 応答 (出典の改訂の番号を持たない古いノートの判定で使う。要るときに作る)。
    answers: OnceCell<BTreeMap<Id, Vec<Id>>>,
}

fn turn_body(view: &ExportView<'_>, t: Id) -> Option<TurnBody> {
    match view.get(KIND_TURN, t).ok()?? {
        Read::Ok(e) => e.body_as::<TurnBody>().ok(),
        _ => None,
    }
}

fn exists(view: &ExportView<'_>, kind: &str, id: Id) -> bool {
    matches!(view.get(kind, id), Ok(Some(_)))
}

impl Prepared {
    fn load(view: &ExportView<'_>, markers: &[&str]) -> Option<Self> {
        let mut bodies: Vec<(Id, FactDirectiveBody)> = Vec::new();
        let mut unblocked: BTreeSet<Id> = BTreeSet::new();
        view.for_each(KIND_FACT_DIRECTIVE, |x| {
            if let Read::Ok(e) = x
                && let Ok(b) = e.body_as::<FactDirectiveBody>()
            {
                if b.op == OP_UNBLOCK
                    && let Some(t) = b.target
                {
                    unblocked.insert(t);
                }
                if b.op == OP_FORGET {
                    bodies.push((e.id, b));
                }
            }
            true
        })
        .ok()?;
        let mut pending = false;
        let mut excluded = BTreeSet::new();
        let forgets = TurnForgets::from_directives(bodies.clone());
        for t in forgets.turns() {
            match view.get(KIND_TURN, t).ok()? {
                Some(Read::Ok(e)) => match check_turn_any(&forgets, &e, markers) {
                    TurnCheck::Current => {}
                    TurnCheck::Pending => pending = true,
                    TurnCheck::Mismatched => {
                        excluded.insert(t);
                    }
                },
                Some(_) if forgets.has_redactions(t) => {
                    excluded.insert(t);
                }
                _ => {}
            }
        }
        let parse = |hs: &[String]| -> Vec<ValueKey> {
            hs.iter().filter_map(|h| ValueKey::from_hex(h)).collect()
        };
        let vks = bodies.iter().flat_map(|(_, b)| parse(&b.vks)).collect();
        let forgotten: BTreeSet<(String, String, ValueKey)> = bodies
            .iter()
            .filter(|(id, _)| !unblocked.contains(id))
            .flat_map(|(_, b)| {
                parse(&b.vks)
                    .into_iter()
                    .map(|vk| (b.subject.clone(), b.attribute.clone(), vk))
            })
            .collect();
        // 印に当たる取り消していない事実が残っていれば、当て直しが済んでいない。
        if !pending && !forgotten.is_empty() {
            view.for_each(KIND_FACT, |x| {
                if let Read::Ok(e) = x
                    && let Ok(b) = e.body_as::<FactBody>()
                    && b.state != "retracted"
                    && let Some(vk) = b.vk.as_deref().and_then(ValueKey::from_hex)
                    && forgotten.contains(&(b.subject, b.attribute, vk))
                {
                    pending = true;
                }
                !pending
            })
            .ok()?;
        }
        let unredacted = bodies
            .iter()
            .filter(|(_, b)| !b.redact)
            .map(|(id, b)| Unredacted {
                directive: *id,
                turns: b
                    .turns
                    .iter()
                    .copied()
                    .chain(b.hidden.iter().map(|h| h.turn))
                    .collect(),
                exchanges: b
                    .hidden
                    .iter()
                    .map(|h| match turn_body(view, h.turn) {
                        Some(t) if t.role != TurnRole::User => t.reply_to.unwrap_or(h.turn),
                        _ => h.turn,
                    })
                    .collect(),
            })
            .collect();
        Some(Self {
            pending,
            excluded,
            vks,
            forgotten,
            unredacted,
            answers: OnceCell::new(),
        })
    }

    /// 今のターンの改訂の番号。ターンが無い・読めない・除くターンなら `None`。
    fn rev_of(&self, view: &ExportView<'_>, t: Id) -> Option<u32> {
        if self.excluded.contains(&t) {
            return None;
        }
        match view.get(KIND_TURN, t).ok()?? {
            Read::Ok(e) => turn_rev_checked(&e),
            _ => None,
        }
    }

    fn answers_of(&self, view: &ExportView<'_>, t: Id) -> Vec<Id> {
        self.answers
            .get_or_init(|| {
                let mut m: BTreeMap<Id, Vec<Id>> = BTreeMap::new();
                let _ = view.for_each(KIND_TURN, |x| {
                    if let Read::Ok(e) = x
                        && let Ok(b) = e.body_as::<TurnBody>()
                        && let Some(u) = b.reply_to
                    {
                        m.entry(u).or_default().push(e.id);
                    }
                    true
                });
                m
            })
            .get(&t)
            .cloned()
            .unwrap_or_default()
    }

    fn include(&self, view: &ExportView<'_>, env: &Envelope) -> bool {
        match env.kind.as_str() {
            KIND_TURN => !self.excluded.contains(&env.id),
            KIND_FACT => env.body_as::<FactBody>().map_or(true, |b| {
                let sourceless = !b.sources.is_empty()
                    && b.sources.iter().all(|s| !exists(view, KIND_TURN, s.turn));
                let forgotten = b.state != "retracted"
                    && b.vk
                        .as_deref()
                        .and_then(ValueKey::from_hex)
                        .is_some_and(|vk| {
                            self.forgotten
                                .contains(&(b.subject.clone(), b.attribute.clone(), vk))
                        });
                !sourceless && !forgotten
            }),
            KIND_CORRECTION => env.body_as::<CorrectionBody>().map_or(true, |b| {
                let turn_alive = exists(view, KIND_TURN, b.turn);
                let previous_alive = b.previous.is_some_and(|p| exists(view, KIND_TURN, p));
                let sourceless = !turn_alive && !previous_alive;
                let excluded = self.excluded.contains(&b.turn)
                    || b.previous.is_some_and(|p| self.excluded.contains(&p));
                let stale = turn_alive && self.rev_of(view, b.turn) != Some(b.rev);
                let forgotten = [b.old_vk.as_deref(), b.new_vk.as_deref()]
                    .into_iter()
                    .flatten()
                    .filter_map(ValueKey::from_hex)
                    .any(|v| self.vks.contains(&v));
                let hidden = self
                    .unredacted
                    .iter()
                    .any(|u| u.turns.contains(&b.turn) && env.id < u.directive);
                !(sourceless || excluded || stale || forgotten || hidden)
            }),
            KIND_NOTE => env.body_as::<NoteBody>().map_or(true, |b| {
                let sourceless = !exists(view, KIND_SESSION, b.session);
                let excluded = b
                    .exchanges
                    .iter()
                    .chain(b.src_revs.iter().map(|s| &s.turn))
                    .any(|t| self.excluded.contains(t));
                let current =
                    note_matches(&b, |t| self.rev_of(view, t), |t| self.answers_of(view, t));
                let hidden = self.unredacted.iter().any(|u| {
                    b.exchanges.iter().any(|x| u.exchanges.contains(x))
                        && b.forgets_seen.is_none_or(|s| s < u.directive)
                });
                !sourceless && !excluded && current && !hidden
            }),
            KIND_RECALL_FEEDBACK => env.body_as::<RecallFeedbackBody>().map_or(true, |b| {
                exists(view, KIND_TURN, b.answer) && exists(view, KIND_TURN, b.target)
            }),
            _ => true,
        }
    }
}

/// どれかの印で合えば合う。
fn check_turn_any(forgets: &TurnForgets, turn: &Envelope, markers: &[&str]) -> TurnCheck {
    markers
        .iter()
        .map(|m| crate::forgets::check_turn(forgets, turn, m))
        .min()
        .unwrap_or(TurnCheck::Mismatched)
}
