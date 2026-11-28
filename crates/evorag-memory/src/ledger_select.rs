//! 台帳の有効性・使う台帳の選び方・呼び直すかの判定 (04 §5.2.1、v0.5)。I/O と LLM を使わない純粋な関数。
//!
//! - [`ledger_valid`]: `sentences` と `inputs` の `refs` の `{turn, rev}` が今のターンと合い、規則の版 (前のふるい・
//!   文の分け方・訂正の字句の型) が今と合うか。合わない理由を全部返す。
//! - [`select_ledger`]: 区切りごとに使う台帳を 1 つ選ぶ (今 → 直前 → 最新 → 版の不一致の段の順)。選んだ段を返す。
//! - [`needs_call`]: 区切りを呼ぶか (呼び直すか) を決める。
//!
//! 台帳を読むのも書くのも呼び手 (`evorag-core`) が行う。旧エンジンのモジュールに依存しない。

use std::collections::BTreeSet;

use evorag_types::Id;

use crate::kinds::SrcRev;
use crate::ledger::{LedgerBody, LedgerStatus};
use crate::prefilter::PREFILTER_V1;
use crate::segment::SegmentBody;
use crate::segment_plan::tail_call_due;
use crate::sentences::SPLITTER_V1;

/// 台帳の有効性に使う規則の版 (台帳の `inputs` の `prefilter`・`splitter`・`correction` と比べる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuleVersions {
    /// 前のふるいの規則の版。
    pub prefilter: u32,
    /// 文の分け方の規則の版。
    pub splitter: u32,
    /// 訂正の字句の型の版 (候補を見つける側が持つ)。
    pub correction: u32,
}

impl RuleVersions {
    /// この版の前のふるいと文の分け方の版に、訂正の字句の型の版 `correction` を合わせたもの。
    #[must_use]
    pub const fn current(correction: u32) -> Self {
        Self {
            prefilter: PREFILTER_V1,
            splitter: SPLITTER_V1,
            correction,
        }
    }
}

/// 規則の種類 ([`Invalid::Version`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rule {
    Prefilter,
    Splitter,
    Correction,
}

/// 台帳が有効でない理由の 1 つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Invalid {
    /// 台帳が指すターンが今は無い (読めない・隔離した・消えた)。
    MissingTurn { turn: Id },
    /// 台帳が指すターンの改訂の番号が今と違う (伏せ字の後に掃除で写し直していないなど)。
    RevChanged { turn: Id, ledger: u32, current: u32 },
    /// 規則の版が今と違う。
    Version {
        rule: Rule,
        ledger: u32,
        current: u32,
    },
}

/// 台帳の有効性 ([`ledger_valid`])。理由は、ターンの理由 (出典の `{turn, rev}` の順) の後に規則の版の理由。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Validity {
    pub problems: Vec<Invalid>,
}

impl Validity {
    /// 有効か (理由が無い)。
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.problems.is_empty()
    }

    /// `sentences` と `refs` の `{turn, rev}` がすべて今のターンと合うか (規則の版は問わない。選び方の
    /// 「版の不一致」の段の条件)。
    #[must_use]
    pub fn turns_match(&self) -> bool {
        self.problems
            .iter()
            .all(|p| matches!(p, Invalid::Version { .. }))
    }
}

/// 台帳が有効かを確かめる (04 §5.2.1 の「有効な台帳」)。`turn_rev` は、ターンの ID から今の改訂の番号を返す
/// (ターンが無いか、改訂の番号を読めなければ `None`)。`forgets_seen` と `input_hash` は見ない (掃除で忘れるが
/// 当たっていることは、掃除の関数が保つ)。
#[must_use]
pub fn ledger_valid(
    ledger: &LedgerBody,
    turn_rev: impl Fn(Id) -> Option<u32>,
    versions: RuleVersions,
) -> Validity {
    let sources: BTreeSet<SrcRev> = ledger.sources();
    let mut problems: Vec<Invalid> = sources
        .into_iter()
        .filter_map(|s| match turn_rev(s.turn) {
            None => Some(Invalid::MissingTurn { turn: s.turn }),
            Some(current) if current != s.rev => Some(Invalid::RevChanged {
                turn: s.turn,
                ledger: s.rev,
                current,
            }),
            Some(_) => None,
        })
        .collect();
    let inputs = &ledger.inputs;
    for (rule, ledger, current) in [
        (Rule::Prefilter, inputs.prefilter, versions.prefilter),
        (Rule::Splitter, inputs.splitter, versions.splitter),
        (Rule::Correction, inputs.correction, versions.correction),
    ] {
        if ledger != current {
            problems.push(Invalid::Version {
                rule,
                ledger,
                current,
            });
        }
    }
    Validity { problems }
}

/// 読めた台帳 1 つ (記録の ID と本体)。ID は UUID v7 で、大きいほど新しい。本体を型として読めない台帳
/// ([`crate::LedgerRead::Unreadable`]) は有効でない台帳なので渡さない。
#[derive(Debug, Clone, Copy)]
pub struct StoredLedger<'a> {
    pub id: Id,
    pub body: &'a LedgerBody,
}

/// 選択 (変種と `memory` の役割の model_key の組、04 §5.2.1 の「選択」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Selection<'a> {
    pub variant: &'a str,
    pub model_key: &'a str,
}

/// 今の選択と、`memory` の役割の状態 (06 §2.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Current<'a> {
    /// 役割のモデルで呼んでいる (`llm.memory` を設定しないときは chat のモデル)。
    Ready(Selection<'a>),
    /// 縮退で、chat のモデル `called` で呼んでいる。`intended` は意図した `memory` の役割の model_key (台帳の
    /// `inputs` の `role_model_key`。まだ記録していなければ `None`)。
    Fallback {
        variant: &'a str,
        intended: Option<&'a str>,
        called: &'a str,
    },
}

/// 選び方と判定の入力のうち、区切りに依らないもの。
#[derive(Debug, Clone, Copy)]
pub struct SelectEnv<'a, F> {
    pub current: Current<'a>,
    /// 今呼ぶ先 (縮退の間は chat のモデル) の llama-server の `build_info`。[`needs_call`] が、`failed` と
    /// `failed_parts` > 0 の台帳の `build_info` と比べる。
    pub build_info: &'a str,
    /// 直前の選択 (06 §3.1 の履歴の前の model_key と、そのとき使っていた変種)。無ければ `None`。
    pub previous: Option<Selection<'a>>,
    /// 今の規則の版。
    pub versions: RuleVersions,
    /// ターンの ID → 今の改訂の番号 (無いか読めなければ `None`)。
    pub turn_rev: F,
}

/// 使う台帳を選んだ段 (04 §5.2.1 の「使う台帳の選び方」)。`doctor` が段ごとの区切りの数を示す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// 今の選択の有効な台帳 (縮退の間は、その縮退で作った台帳を含む)。
    Current,
    /// 直前の選択の有効な台帳 (役割が戻った後は、今の選択から縮退して作った台帳を含む)。
    Previous,
    /// ほかの選択の有効な台帳。
    Latest,
    /// `{turn, rev}` は合い、規則の版だけが合わない台帳。関門は今の規則で当て直し、`corrections` は使わない。
    VersionMismatch,
}

impl Stage {
    /// この段の台帳の `corrections` を導出に使うか (版の不一致の段では使わない。`corr` の表が古い規則の型で
    /// 作られているため)。
    #[must_use]
    pub fn uses_corrections(self) -> bool {
        self != Stage::VersionMismatch
    }
}

/// 選んだ台帳 ([`select_ledger`])。
#[derive(Debug, Clone, Copy)]
pub struct Selected<'a> {
    pub ledger: StoredLedger<'a>,
    pub stage: Stage,
}

/// 選択に合う、縮退でない台帳か。
fn of_selection(l: &LedgerBody, s: Selection<'_>) -> bool {
    !l.inputs.role_fallback && l.variant == s.variant && l.model_key == s.model_key
}

/// 縮退で作った台帳のうち、変種 `variant` で、意図した役割の model_key が `intended` のものか。
fn of_fallback(l: &LedgerBody, variant: &str, intended: Option<&str>) -> bool {
    l.inputs.role_fallback && l.variant == variant && l.inputs.role_model_key.as_deref() == intended
}

impl Current<'_> {
    /// 台帳がこの選択のものか (状態と有効性は問わない。選び方の「今」の段の台帳の作り)。縮退の間は、同じ chat の
    /// モデルで同じ役割から縮退して作った台帳を含む。台帳を書く側が、置き換えて消す台帳を選ぶときにも使う。
    #[must_use]
    pub fn owns(self, l: &LedgerBody) -> bool {
        match self {
            Current::Ready(s) => of_selection(l, s),
            Current::Fallback {
                variant,
                intended,
                called,
            } => {
                intended.is_some_and(|model_key| of_selection(l, Selection { variant, model_key }))
                    || (of_fallback(l, variant, intended) && l.model_key == called)
            }
        }
    }
}

impl<F> SelectEnv<'_, F> {
    /// 今の選択の台帳か (状態は問わない)。縮退の間は、同じ chat のモデルで同じ役割から縮退して作った台帳を含む。
    fn is_current(&self, l: &LedgerBody) -> bool {
        self.current.owns(l)
    }

    /// 直前の選択の台帳か。役割が戻った後は、今の選択から縮退して作った台帳を含む。
    fn is_previous(&self, l: &LedgerBody) -> bool {
        let fallback_of_current = match self.current {
            Current::Ready(s) => of_fallback(l, s.variant, Some(s.model_key)),
            Current::Fallback { .. } => false,
        };
        fallback_of_current || self.previous.is_some_and(|p| of_selection(l, p))
    }
}

/// 選び方と判定の対象になる台帳か (区切りの呼び出しで、この区切りのもの)。
fn of_segment(l: &LedgerBody, segment: Id) -> bool {
    l.is_segment_call() && l.segment == Some(segment)
}

/// 使える状態か (`complete`・`partial`。`failed` と知らない値は使わない)。
fn usable(l: &LedgerBody) -> bool {
    matches!(l.status(), LedgerStatus::Complete | LedgerStatus::Partial)
}

/// 区切り `segment` に使う台帳を 1 つ選ぶ (04 §5.2.1 の「使う台帳の選び方」)。
///
/// 段の順 ([`Stage`]) に、最初に見つかった台帳を使う。同じ段に 2 つ以上あれば、ID の最も大きい (最も新しい)
/// 台帳を使う (ID は UUID v7 で重ならないので、決定論になる)。`failed` と知らない状態の台帳はどの段でも使わず、
/// `partial` は使える台帳として扱う。区切りの呼び出しでない台帳と、ほかの区切りの台帳は見ない。
#[must_use]
pub fn select_ledger<'a, F: Fn(Id) -> Option<u32>>(
    segment: Id,
    ledgers: &[StoredLedger<'a>],
    env: &SelectEnv<'_, F>,
) -> Option<Selected<'a>> {
    let mut best: Option<Selected<'a>> = None;
    for &l in ledgers {
        let body = l.body;
        if !of_segment(body, segment) || !usable(body) {
            continue;
        }
        let validity = ledger_valid(body, &env.turn_rev, env.versions);
        let stage = if validity.is_valid() {
            if env.is_current(body) {
                Stage::Current
            } else if env.is_previous(body) {
                Stage::Previous
            } else {
                Stage::Latest
            }
        } else if validity.turns_match() {
            Stage::VersionMismatch
        } else {
            continue;
        };
        let better = best.is_none_or(|b| {
            (stage, std::cmp::Reverse(l.id)) < (b.stage, std::cmp::Reverse(b.ledger.id))
        });
        if better {
            best = Some(Selected { ledger: l, stage });
        }
    }
    best
}

/// 区切りを呼ぶ理由 ([`CallDecision::Call`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallReason {
    /// 使える台帳 (どの段でも) が無く、呼び直しを止める `failed` の台帳も無い。
    NoLedger,
    /// 使う台帳が `partial` (次の機会に毎回呼び直す)。
    Partial,
    /// 開いている末尾の区切りで、使う台帳の `through` から往復が [`crate::TAIL_CALL_PAIRS`] 以上増えた (`through`
    /// が区切りに合わないときを含む)。
    TailGrew,
    /// 区切りが閉じて、使う台帳の `through` が区切りのすべての往復を入れていない (合わないときを含む)。
    Closed,
    /// 今の選択の `failed` の台帳の後に、呼ぶ先の model_key か `build_info` が変わった。
    FailedRetry,
    /// 使う今の選択の台帳が `failed_parts` > 0 の `complete` で、その後に呼ぶ先の model_key か `build_info` が
    /// 変わった。
    FailedPartsRetry,
}

/// 区切りを呼ばない理由 ([`CallDecision::Keep`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeepReason {
    /// 使う台帳がその段にあり、呼び直す条件に当たらない (古い区切りは自動では作り直さない)。
    Covered(Stage),
    /// 使える台帳が無いが、今の選択の有効な `failed` の台帳の後に、呼ぶ先が変わっていない。
    FailedUnchanged,
    /// 使う今の選択の台帳が `failed_parts` > 0 の `complete` で、その後に呼ぶ先が変わっていない。
    FailedPartsUnchanged,
    /// 区切りに往復が無い (呼べない)。
    NoPairs,
}

/// 区切りを呼ぶか ([`needs_call`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallDecision {
    Call(CallReason),
    Keep(KeepReason),
}

impl CallDecision {
    /// 呼ぶか。
    #[must_use]
    pub fn is_call(self) -> bool {
        matches!(self, CallDecision::Call(_))
    }
}

/// 往復の増えと閉じたことで呼ぶか (`through` が無い台帳は、入れた往復が分からないので呼ぶ)。
fn tail_reason(segment: &SegmentBody, l: &LedgerBody) -> Option<CallReason> {
    let due = l
        .inputs
        .through
        .as_ref()
        .is_none_or(|t| tail_call_due(segment, t));
    due.then_some(if segment.closed {
        CallReason::Closed
    } else {
        CallReason::TailGrew
    })
}

impl<F> SelectEnv<'_, F> {
    /// 台帳を呼んだ先から、今呼ぶ先 (model_key と `build_info` の組) が変わったか。`build_info` の無い (null の)
    /// 台帳は、model_key だけで比べる (`build_info` を足す前の台帳を、まとめて呼び直さないため)。
    fn target_changed(&self, l: &LedgerBody) -> bool {
        let model_key = match self.current {
            Current::Ready(s) => s.model_key,
            Current::Fallback { called, .. } => called,
        };
        l.model_key != model_key
            || l.build_info
                .as_deref()
                .is_some_and(|b| b != self.build_info)
    }
}

/// 区切りを呼ぶか (呼び直すか) を決める (04 §5.2.1 の `needs_call`。裏の処理と `evorag sleep` の判定)。
///
/// 「呼ぶ先が変わった」は、台帳の `model_key`・`build_info` と、今呼ぶ先 (縮退の間は chat のモデル) の
/// model_key と [`SelectEnv::build_info`] の組を比べて決める (`build_info` の無い台帳は model_key だけで比べる)。
///
/// 次の順に決める。
/// 1. 使う台帳 ([`select_ledger`]) が無い: 今の選択の、有効な `failed` の台帳 (最も新しいもの) があれば、その後に
///    呼ぶ先が変わったときと、その `through` から往復の増え・閉じたことで呼ぶときだけ呼ぶ。無ければ呼ぶ。
/// 2. 使う台帳が `partial`: 呼ぶ (段を問わない)。
/// 3. 使う台帳の `through` から、開いている末尾の区切りの往復が 2 つ以上増えたか、区切りが閉じて入れていない往復が
///    ある: 呼ぶ (段を問わない。新しい往復は今の選択で呼ぶ)。
/// 4. 使う台帳が今の選択の `failed_parts` > 0 の `complete`: その後に呼ぶ先が変わったときだけ呼ぶ。
/// 5. ほかは呼ばない (今の選択でない段の台帳も、自動では作り直さない)。
#[must_use]
pub fn needs_call<F: Fn(Id) -> Option<u32>>(
    segment: &SegmentBody,
    ledgers: &[StoredLedger<'_>],
    env: &SelectEnv<'_, F>,
) -> CallDecision {
    let Some(id) = segment.id() else {
        return CallDecision::Keep(KeepReason::NoPairs);
    };
    let Some(selected) = select_ledger(id, ledgers, env) else {
        let failed = ledgers
            .iter()
            .filter(|l| {
                of_segment(l.body, id)
                    && l.body.status() == LedgerStatus::Failed
                    && env.is_current(l.body)
                    && ledger_valid(l.body, &env.turn_rev, env.versions).is_valid()
            })
            .max_by_key(|l| l.id);
        return match failed {
            None => CallDecision::Call(CallReason::NoLedger),
            Some(l) if env.target_changed(l.body) => CallDecision::Call(CallReason::FailedRetry),
            Some(l) => tail_reason(segment, l.body).map_or(
                CallDecision::Keep(KeepReason::FailedUnchanged),
                CallDecision::Call,
            ),
        };
    };
    let l = selected.ledger;
    if l.body.status() == LedgerStatus::Partial {
        return CallDecision::Call(CallReason::Partial);
    }
    if let Some(reason) = tail_reason(segment, l.body) {
        return CallDecision::Call(reason);
    }
    if selected.stage == Stage::Current && l.body.failed_parts > 0 {
        return if env.target_changed(l.body) {
            CallDecision::Call(CallReason::FailedPartsRetry)
        } else {
            CallDecision::Keep(KeepReason::FailedPartsUnchanged)
        };
    }
    CallDecision::Keep(KeepReason::Covered(selected.stage))
}

#[cfg(test)]
mod tests;
