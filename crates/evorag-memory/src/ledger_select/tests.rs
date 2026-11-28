use std::collections::BTreeMap;

use super::*;
use crate::ledger::{CALL_SEGMENT, LedgerRef, SentenceSpan, Through};

const SESSION: Id = Id::from_bytes([0xEE; 16]);
const CORR_V: u32 = 3;
const V: &str = "sr1";
const A: &str = "model-a";
const B: &str = "model-b";
const CHAT: &str = "chat-model";
/// 今呼ぶ先の llama-server の `build_info` (台帳の見本も同じ)。
const BUILD: &str = "b100-0123abc";

/// 発話 `n` のターンの ID。
fn turn(n: u8) -> Id {
    let mut b = [0u8; 16];
    b[15] = n;
    Id::from_bytes(b)
}

/// 時刻 `ms` に作った台帳の ID (UUID v7 の先頭 48 ビットが時刻)。
fn lid(ms: u64) -> Id {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[15] = 0xAA;
    Id::from_bytes(b)
}

fn segment(n: u8, closed: bool) -> SegmentBody {
    SegmentBody {
        session: SESSION,
        pairs: (1..=n).map(turn).collect(),
        closed,
        chars: 0,
    }
}

fn seg_id() -> Id {
    segment(1, false)
        .id()
        .expect("a segment with pairs has an id")
}

/// 往復 1..=`through` を入れた、今の規則の版の台帳。
fn ledger(model_key: &str, status: &str, through: u8) -> LedgerBody {
    let mut body = LedgerBody {
        call: CALL_SEGMENT.to_owned(),
        segment: Some(seg_id()),
        schema: 1,
        variant: V.to_owned(),
        model_key: model_key.to_owned(),
        build_info: Some(BUILD.to_owned()),
        status: status.to_owned(),
        ..LedgerBody::default()
    };
    body.sentences = (1..=through)
        .map(|n| SentenceSpan::new(u32::from(n), turn(n), 1, 0, 3))
        .collect();
    body.inputs.prefilter = PREFILTER_V1;
    body.inputs.splitter = SPLITTER_V1;
    body.inputs.correction = CORR_V;
    body.inputs.role_model_key = Some(model_key.to_owned());
    body.inputs.through = Some(Through {
        turn: turn(through),
        pairs: u32::from(through),
    });
    body
}

/// 縮退で `called` を呼んで作った台帳 (意図した役割は `intended`)。
fn fallback_ledger(called: &str, intended: Option<&str>, through: u8) -> LedgerBody {
    let mut body = ledger(called, "complete", through);
    body.inputs.role_fallback = true;
    body.inputs.role_model_key = intended.map(str::to_owned);
    body
}

/// ターン 1..=20 の改訂の番号はどれも 1。
fn revs() -> BTreeMap<Id, u32> {
    (1..=20).map(|n| (turn(n), 1)).collect()
}

type Revs = BTreeMap<Id, u32>;

fn env_with<'a>(
    revs: &'a Revs,
    current: Current<'a>,
    previous: Option<Selection<'a>>,
) -> SelectEnv<'a, impl Fn(Id) -> Option<u32> + 'a> {
    SelectEnv {
        current,
        build_info: BUILD,
        previous,
        versions: RuleVersions::current(CORR_V),
        turn_rev: move |t: Id| revs.get(&t).copied(),
    }
}

const fn sel(model_key: &str) -> Selection<'_> {
    Selection {
        variant: V,
        model_key,
    }
}

/// 今の選択が A (直前は B)。
fn env_a(revs: &Revs) -> SelectEnv<'_, impl Fn(Id) -> Option<u32> + '_> {
    env_with(revs, Current::Ready(sel(A)), Some(sel(B)))
}

fn stored<'a>(entries: &[(u64, &'a LedgerBody)]) -> Vec<StoredLedger<'a>> {
    entries
        .iter()
        .map(|&(ms, body)| StoredLedger { id: lid(ms), body })
        .collect()
}

fn picked(
    ledgers: &[StoredLedger<'_>],
    env: &SelectEnv<'_, impl Fn(Id) -> Option<u32>>,
) -> Option<(u64, Stage)> {
    select_ledger(seg_id(), ledgers, env).map(|s| {
        let ms =
            u64::try_from(s.ledger.id.timestamp().as_millis()).expect("test times are positive");
        (ms, s.stage)
    })
}

// ---- 有効性 ----

#[test]
fn a_ledger_matching_turns_and_versions_is_valid() {
    let revs = revs();
    let mut l = ledger(A, "complete", 3);
    l.inputs.refs = vec![LedgerRef::new(turn(10), 1)];
    // `forgets_seen` と `input_hash` は有効性に使わない。
    l.inputs.forgets_seen = Some(turn(99));
    l.inputs.input_hash = None;
    let v = ledger_valid(&l, |t| revs.get(&t).copied(), RuleVersions::current(CORR_V));
    assert!(v.is_valid(), "{v:?}");
    assert!(v.turns_match());
}

#[test]
fn missing_turns_and_changed_revisions_invalidate() {
    let mut revs = revs();
    revs.remove(&turn(2));
    revs.insert(turn(3), 2);
    revs.insert(turn(10), 5);
    let mut l = ledger(A, "complete", 3);
    l.inputs.refs = vec![LedgerRef::new(turn(10), 1)];
    let v = ledger_valid(&l, |t| revs.get(&t).copied(), RuleVersions::current(CORR_V));
    assert_eq!(
        v.problems,
        vec![
            Invalid::MissingTurn { turn: turn(2) },
            Invalid::RevChanged {
                turn: turn(3),
                ledger: 1,
                current: 2
            },
            Invalid::RevChanged {
                turn: turn(10),
                ledger: 1,
                current: 5
            },
        ]
    );
    assert!(!v.is_valid());
    assert!(!v.turns_match());
}

#[test]
fn each_rule_version_is_checked() {
    let revs = revs();
    let l = ledger(A, "complete", 2);
    let now = RuleVersions::current(CORR_V);
    for (rule, versions) in [
        (
            Rule::Prefilter,
            RuleVersions {
                prefilter: now.prefilter + 1,
                ..now
            },
        ),
        (
            Rule::Splitter,
            RuleVersions {
                splitter: now.splitter + 1,
                ..now
            },
        ),
        (
            Rule::Correction,
            RuleVersions {
                correction: now.correction + 1,
                ..now
            },
        ),
    ] {
        let v = ledger_valid(&l, |t| revs.get(&t).copied(), versions);
        assert!(!v.is_valid(), "{rule:?}");
        assert!(v.turns_match(), "{rule:?}: only the version differs");
        assert!(
            matches!(v.problems.as_slice(), [Invalid::Version { rule: r, .. }] if *r == rule),
            "{rule:?}: {v:?}"
        );
    }
}

#[test]
fn a_ledger_without_versions_is_not_valid() {
    // 項目を欠く台帳は版 0 で読む (有効でない側に倒す、04 §5.2.1 の「台帳の読み方」)。
    let revs = revs();
    let mut l = ledger(A, "complete", 1);
    l.inputs = crate::ledger::LedgerInputs::default();
    let v = ledger_valid(&l, |t| revs.get(&t).copied(), RuleVersions::current(CORR_V));
    assert_eq!(v.problems.len(), 3);
}

// ---- 選び方 ----

#[test]
fn stages_are_tried_in_order() {
    let revs = revs();
    let env = env_a(&revs);
    let current = ledger(A, "complete", 2);
    let previous = ledger(B, "complete", 2);
    let other = ledger("model-c", "complete", 2);
    let mut old_rules = ledger(A, "complete", 2);
    old_rules.inputs.prefilter = 0;

    // 新しさより段を先に見る。
    let all = stored(&[
        (10, &current),
        (20, &previous),
        (30, &other),
        (40, &old_rules),
    ]);
    assert_eq!(picked(&all, &env), Some((10, Stage::Current)));
    let all = stored(&[(20, &previous), (30, &other), (40, &old_rules)]);
    assert_eq!(picked(&all, &env), Some((20, Stage::Previous)));
    let all = stored(&[(30, &other), (40, &old_rules)]);
    assert_eq!(picked(&all, &env), Some((30, Stage::Latest)));
    let all = stored(&[(40, &old_rules)]);
    assert_eq!(picked(&all, &env), Some((40, Stage::VersionMismatch)));
    assert!(!Stage::VersionMismatch.uses_corrections());
    assert!(Stage::Latest.uses_corrections());
}

#[test]
fn the_newest_ledger_of_a_stage_wins() {
    let revs = revs();
    let env = env_a(&revs);
    let old = ledger(A, "complete", 2);
    let new = ledger(A, "partial", 3);
    let x = ledger("model-c", "complete", 2);
    let y = ledger("model-d", "complete", 2);
    for order in [[(10, &old), (20, &new)], [(20, &new), (10, &old)]] {
        assert_eq!(picked(&stored(&order), &env), Some((20, Stage::Current)));
    }
    assert_eq!(
        picked(&stored(&[(31, &y), (30, &x)]), &env),
        Some((31, Stage::Latest))
    );
}

#[test]
fn failed_and_unknown_ledgers_are_never_selected() {
    let revs = revs();
    let env = env_a(&revs);
    let failed = ledger(A, "failed", 2);
    let unknown = ledger(A, "someday", 2);
    let old = ledger(B, "complete", 2);
    assert_eq!(
        picked(&stored(&[(50, &failed), (60, &unknown)]), &env),
        None
    );
    assert_eq!(
        picked(&stored(&[(50, &failed), (60, &unknown), (10, &old)]), &env),
        Some((10, Stage::Previous))
    );
}

#[test]
fn invalid_turns_are_never_selected() {
    let mut revs = revs();
    revs.insert(turn(1), 2);
    let env = env_a(&revs);
    let l = ledger(A, "complete", 2);
    assert_eq!(picked(&stored(&[(10, &l)]), &env), None);
}

#[test]
fn ledgers_of_other_segments_and_calls_are_ignored() {
    let revs = revs();
    let env = env_a(&revs);
    let mut other_segment = ledger(A, "complete", 2);
    other_segment.segment = Some(turn(77));
    let mut other_call = ledger(A, "complete", 2);
    other_call.call = "topic_title".to_owned();
    assert_eq!(
        picked(&stored(&[(10, &other_segment), (20, &other_call)]), &env),
        None
    );
}

#[test]
fn during_fallback_the_fallback_ledgers_are_current() {
    let revs = revs();
    let env = env_with(
        &revs,
        Current::Fallback {
            variant: V,
            intended: Some(A),
            called: CHAT,
        },
        Some(sel(B)),
    );
    let made_in_fallback = fallback_ledger(CHAT, Some(A), 2);
    let before_fallback = ledger(A, "complete", 2);
    let other_chat = fallback_ledger("other-chat", Some(A), 2);
    let other_role = fallback_ledger(CHAT, Some(B), 2);
    let plain_chat = ledger(CHAT, "complete", 2);
    assert_eq!(
        picked(&stored(&[(10, &made_in_fallback)]), &env),
        Some((10, Stage::Current))
    );
    // 縮退の前に役割のモデルで作った台帳も今の選択の台帳。新しいものを使う。
    assert_eq!(
        picked(
            &stored(&[(10, &made_in_fallback), (20, &before_fallback)]),
            &env
        ),
        Some((20, Stage::Current))
    );
    // ほかの chat のモデル・ほかの役割から縮退した台帳と、縮退でない chat のモデルの台帳は今の選択ではない。
    for l in [&other_chat, &other_role, &plain_chat] {
        assert_eq!(picked(&stored(&[(30, l)]), &env), Some((30, Stage::Latest)));
    }
}

#[test]
fn fallback_without_a_recorded_role_model() {
    let revs = revs();
    let env = env_with(
        &revs,
        Current::Fallback {
            variant: V,
            intended: None,
            called: CHAT,
        },
        None,
    );
    let made = fallback_ledger(CHAT, None, 2);
    let with_role = fallback_ledger(CHAT, Some(A), 2);
    assert_eq!(
        picked(&stored(&[(10, &made)]), &env),
        Some((10, Stage::Current))
    );
    assert_eq!(
        picked(&stored(&[(10, &with_role)]), &env),
        Some((10, Stage::Latest))
    );
}

#[test]
fn after_the_role_returns_fallback_ledgers_are_previous() {
    let revs = revs();
    let env = env_a(&revs);
    let made_in_fallback = fallback_ledger(CHAT, Some(A), 2);
    let previous = ledger(B, "complete", 2);
    let fallback_of_b = fallback_ledger(CHAT, Some(B), 2);
    assert_eq!(
        picked(&stored(&[(10, &made_in_fallback)]), &env),
        Some((10, Stage::Previous))
    );
    assert_eq!(
        picked(&stored(&[(10, &made_in_fallback), (5, &previous)]), &env),
        Some((10, Stage::Previous))
    );
    // 直前の選択から縮退した台帳は、直前の選択の台帳ではない。
    assert_eq!(
        picked(&stored(&[(10, &fallback_of_b)]), &env),
        Some((10, Stage::Latest))
    );
}

// ---- needs_call ----

fn decide(
    seg: &SegmentBody,
    entries: &[(u64, &LedgerBody)],
    env: &SelectEnv<'_, impl Fn(Id) -> Option<u32>>,
) -> CallDecision {
    needs_call(seg, &stored(entries), env)
}

#[test]
fn a_segment_without_ledgers_is_called() {
    let revs = revs();
    let env = env_a(&revs);
    assert_eq!(
        decide(&segment(3, true), &[], &env),
        CallDecision::Call(CallReason::NoLedger)
    );
    assert_eq!(
        decide(
            &SegmentBody {
                pairs: Vec::new(),
                ..segment(1, true)
            },
            &[],
            &env
        ),
        CallDecision::Keep(KeepReason::NoPairs)
    );
}

#[test]
fn an_invalid_ledger_is_called_again() {
    let mut revs = revs();
    revs.insert(turn(2), 7);
    let env = env_a(&revs);
    let l = ledger(A, "complete", 3);
    assert_eq!(
        decide(&segment(3, true), &[(10, &l)], &env),
        CallDecision::Call(CallReason::NoLedger)
    );
}

#[test]
fn a_covered_segment_is_not_called() {
    let revs = revs();
    let env = env_a(&revs);
    let l = ledger(A, "complete", 3);
    assert_eq!(
        decide(&segment(3, true), &[(10, &l)], &env),
        CallDecision::Keep(KeepReason::Covered(Stage::Current))
    );
}

#[test]
fn old_segments_are_not_rebuilt_after_a_model_or_rule_change() {
    let revs = revs();
    let env = env_a(&revs);
    // モデルを B から A に替えた: B の台帳は直前の選択の台帳として使い続ける。
    let of_b = ledger(B, "complete", 3);
    assert_eq!(
        decide(&segment(3, true), &[(10, &of_b)], &env),
        CallDecision::Keep(KeepReason::Covered(Stage::Previous))
    );
    // 規則の版が上がった: 版の不一致の段で使い続ける。
    let mut old_rules = ledger(A, "complete", 3);
    old_rules.inputs.splitter = 0;
    assert_eq!(
        decide(&segment(3, true), &[(10, &old_rules)], &env),
        CallDecision::Keep(KeepReason::Covered(Stage::VersionMismatch))
    );
}

#[test]
fn partial_ledgers_are_always_called_again() {
    let revs = revs();
    let env = env_a(&revs);
    let current = ledger(A, "partial", 3);
    let previous = ledger(B, "partial", 3);
    for l in [&current, &previous] {
        assert_eq!(
            decide(&segment(3, true), &[(10, l)], &env),
            CallDecision::Call(CallReason::Partial)
        );
    }
}

#[test]
fn a_failed_ledger_is_retried_only_after_the_target_changes() {
    let revs = revs();
    let env = env_a(&revs);
    let failed = ledger(A, "failed", 3);
    // model_key も build_info も変わっていない。
    assert_eq!(
        decide(&segment(3, true), &[(500, &failed)], &env),
        CallDecision::Keep(KeepReason::FailedUnchanged)
    );
    // build_info が変わった。
    let rebuilt = SelectEnv {
        build_info: "b200-4567def",
        ..env
    };
    assert_eq!(
        decide(&segment(3, true), &[(500, &failed)], &rebuilt),
        CallDecision::Call(CallReason::FailedRetry)
    );
    // build_info の無い (項目を足す前の) 台帳は、model_key が同じなら変わっていないとする。
    let mut old = failed.clone();
    old.build_info = None;
    assert_eq!(
        decide(&segment(3, true), &[(500, &old)], &rebuilt),
        CallDecision::Keep(KeepReason::FailedUnchanged)
    );
    // model_key が変わった: 失敗した台帳は今の選択のものでない。
    let env_b = env_with(&revs, Current::Ready(sel(B)), Some(sel(A)));
    assert_eq!(
        decide(&segment(3, true), &[(500, &failed)], &env_b),
        CallDecision::Call(CallReason::NoLedger)
    );
    // 縮退に入った: 役割のモデルで失敗した台帳は、chat のモデルで呼び直す。
    let fallback = env_with(
        &revs,
        Current::Fallback {
            variant: V,
            intended: Some(A),
            called: CHAT,
        },
        Some(sel(B)),
    );
    assert_eq!(
        decide(&segment(3, true), &[(500, &failed)], &fallback),
        CallDecision::Call(CallReason::FailedRetry)
    );
}

#[test]
fn a_failed_ledger_does_not_block_when_turns_changed_or_pairs_were_added() {
    let revs = revs();
    let env = env_a(&revs);
    let failed = ledger(A, "failed", 3);
    assert_eq!(
        decide(&segment(5, false), &[(500, &failed)], &env),
        CallDecision::Call(CallReason::TailGrew)
    );
    assert_eq!(
        decide(&segment(4, true), &[(500, &failed)], &env),
        CallDecision::Call(CallReason::Closed)
    );
    let mut changed = revs.clone();
    changed.insert(turn(1), 2);
    let env = env_a(&changed);
    assert_eq!(
        decide(&segment(3, true), &[(500, &failed)], &env),
        CallDecision::Call(CallReason::NoLedger)
    );
}

#[test]
fn a_failed_ledger_does_not_hide_an_older_usable_one() {
    let revs = revs();
    let env = env_a(&revs);
    let failed = ledger(A, "failed", 3);
    let of_b = ledger(B, "complete", 3);
    assert_eq!(
        decide(&segment(3, true), &[(500, &failed), (10, &of_b)], &env),
        CallDecision::Keep(KeepReason::Covered(Stage::Previous))
    );
}

#[test]
fn failed_parts_are_retried_only_after_the_target_changes() {
    let revs = revs();
    let env = env_a(&revs);
    let mut l = ledger(A, "complete", 3);
    l.failed_parts = 1;
    assert_eq!(
        decide(&segment(3, true), &[(500, &l)], &env),
        CallDecision::Keep(KeepReason::FailedPartsUnchanged)
    );
    let rebuilt = SelectEnv {
        build_info: "b200-4567def",
        ..env
    };
    assert_eq!(
        decide(&segment(3, true), &[(500, &l)], &rebuilt),
        CallDecision::Call(CallReason::FailedPartsRetry)
    );
    let mut old = l.clone();
    old.build_info = None;
    assert_eq!(
        decide(&segment(3, true), &[(500, &old)], &rebuilt),
        CallDecision::Keep(KeepReason::FailedPartsUnchanged)
    );
    // 直前の選択の台帳なら、自動では作り直さない。
    let mut of_b = ledger(B, "complete", 3);
    of_b.failed_parts = 1;
    assert_eq!(
        decide(&segment(3, true), &[(500, &of_b)], &rebuilt),
        CallDecision::Keep(KeepReason::Covered(Stage::Previous))
    );
}

#[test]
fn the_tail_is_called_every_two_pairs_and_on_close() {
    let revs = revs();
    let env = env_a(&revs);
    let l = ledger(A, "complete", 3);
    let cases = [
        (
            segment(3, false),
            CallDecision::Keep(KeepReason::Covered(Stage::Current)),
        ),
        (
            segment(4, false),
            CallDecision::Keep(KeepReason::Covered(Stage::Current)),
        ),
        (segment(5, false), CallDecision::Call(CallReason::TailGrew)),
        (
            segment(3, true),
            CallDecision::Keep(KeepReason::Covered(Stage::Current)),
        ),
        (segment(4, true), CallDecision::Call(CallReason::Closed)),
    ];
    for (seg, want) in cases {
        assert_eq!(decide(&seg, &[(10, &l)], &env), want, "{seg:?}");
    }
    // `through` が無い台帳は、入れた往復が分からないので呼ぶ。
    let mut no_through = ledger(A, "complete", 3);
    no_through.inputs.through = None;
    assert_eq!(
        decide(&segment(3, true), &[(10, &no_through)], &env),
        CallDecision::Call(CallReason::Closed)
    );
    // 直前の選択の台帳でも、往復が増えたら今の選択で呼ぶ。
    let of_b = ledger(B, "complete", 3);
    assert_eq!(
        decide(&segment(5, false), &[(10, &of_b)], &env),
        CallDecision::Call(CallReason::TailGrew)
    );
}

#[test]
fn entering_and_leaving_fallback_does_not_call_again() {
    let revs = revs();
    let made_in_fallback = fallback_ledger(CHAT, Some(A), 3);
    let before = ledger(A, "complete", 3);
    let in_fallback = env_with(
        &revs,
        Current::Fallback {
            variant: V,
            intended: Some(A),
            called: CHAT,
        },
        Some(sel(B)),
    );
    // 縮退に入っても、役割のモデルで作った台帳を使い続ける。
    assert_eq!(
        decide(&segment(3, true), &[(10, &before)], &in_fallback),
        CallDecision::Keep(KeepReason::Covered(Stage::Current))
    );
    assert_eq!(
        decide(&segment(3, true), &[(10, &made_in_fallback)], &in_fallback),
        CallDecision::Keep(KeepReason::Covered(Stage::Current))
    );
    // 役割が戻っても、縮退で作った台帳を直前の選択の台帳として使い続ける。
    let returned = env_a(&revs);
    assert_eq!(
        decide(&segment(3, true), &[(10, &made_in_fallback)], &returned),
        CallDecision::Keep(KeepReason::Covered(Stage::Previous))
    );
}
