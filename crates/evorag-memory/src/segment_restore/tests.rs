use evorag_store::Envelope;
use evorag_types::Timestamp;

use super::*;
use crate::kinds::{TurnBody, TurnState};
use crate::ledger::{
    ByteRange, CALL_SEGMENT, CorrInput, LedgerRef, SentenceSpan, Through, UnknownFields,
};
use crate::ledger_select::StoredLedger;
use crate::prefilter::MODE_CHAT;
use crate::segment_plan::{SegmentChange, plan_segments};

const S: Id = Id::from_bytes([0xEE; 16]);
const T0: i64 = 1_000_000;

fn id(n: u16) -> Id {
    let mut b = [0u8; 16];
    b[14..].copy_from_slice(&n.to_be_bytes());
    Id::from_bytes(b)
}

fn turn(n: u16, role: TurnRole, reply_to: Option<u16>, text: &str) -> Turn {
    let body = TurnBody {
        session: S,
        role,
        text: text.into(),
        state: if role == TurnRole::User {
            TurnState::Confirmed
        } else {
            TurnState::Completed
        },
        model_key: None,
        generation: None,
        reply_to: reply_to.map(id),
        attached: Vec::new(),
        error: None,
        recall: None,
        obs: None,
    };
    let mut envelope = Envelope::new(id(n), "turn", 1, Timestamp::from_millis(T0), "test");
    envelope.set_body(&body).expect("turn body encodes");
    Turn { envelope, body }
}

/// 発話 1, 3, 5, … (`texts` の順) と、それぞれへの応答 2, 4, 6, …。
fn session(texts: &[&str]) -> Vec<Turn> {
    texts
        .iter()
        .zip(0u16..)
        .flat_map(|(t, i)| {
            [
                turn(2 * i + 1, TurnRole::User, None, t),
                turn(2 * i + 2, TurnRole::Assistant, Some(2 * i + 1), "応答です"),
            ]
        })
        .collect()
}

/// 発話 `first`..=`last` (奇数) の区切りの台帳。番号を振った文は `sentences` の発話に 1 つずつ。
fn ledger(first: u16, last: u16, sentences: &[u16]) -> LedgerBody {
    let mut body = LedgerBody {
        call: CALL_SEGMENT.to_owned(),
        segment: Some(segment_id(id(first))),
        status: "complete".to_owned(),
        ..LedgerBody::default()
    };
    body.sentences = sentences
        .iter()
        .zip(1u32..)
        .map(|(&t, s)| SentenceSpan::new(s, id(t), 1, 0, 3))
        .collect();
    body.inputs.through = Some(Through {
        turn: id(last),
        pairs: u32::from((last - first) / 2 + 1),
    });
    body
}

fn stored(bodies: &[LedgerBody]) -> Vec<StoredLedger<'_>> {
    bodies
        .iter()
        .zip(1u8..)
        .map(|(body, n)| {
            let mut b = [0x10u8; 16];
            b[15] = n;
            StoredLedger {
                id: Id::from_bytes(b),
                body,
            }
        })
        .collect()
}

const TEXT: &str = "東京に住んでいて、休日は将棋を指します";

fn restore(
    turns: &[Turn],
    first: u16,
    ledgers: &[LedgerBody],
    known: &[SegmentBody],
) -> Result<SegmentBody, RestoreError> {
    restore_segment(segment_id(id(first)), S, turns, &stored(ledgers), known)
}

#[test]
fn pairs_are_restored_from_through_including_unsent_utterances() {
    // 発話 5 は挨拶で番号を振らない (台帳の `sentences` に無い) が、往復として区切りに入る。
    let turns = session(&[TEXT, TEXT, "こんにちは", TEXT, TEXT]);
    let l = ledger(3, 7, &[3, 7]);
    let got = restore(&turns, 3, &[l], &[]).expect("restores");
    assert_eq!(got.pairs, vec![id(3), id(5), id(7)]);
    assert_eq!(got.session, S);
    assert!(got.closed);
    let one = utterance_chars(TEXT);
    assert_eq!(got.chars, 2 * one);
    assert_eq!(got.id(), Some(segment_id(id(3))));
}

#[test]
fn the_longest_ledger_gives_the_pairs() {
    let turns = session(&[TEXT, TEXT, TEXT, TEXT]);
    let older = ledger(1, 3, &[1, 3]);
    let newer = ledger(1, 7, &[1, 3, 5, 7]);
    for ledgers in [
        [older.clone(), newer.clone()],
        [newer.clone(), older.clone()],
    ] {
        let got = restore(&turns, 1, &ledgers, &[]).expect("restores");
        assert_eq!(got.pairs, vec![id(1), id(3), id(5), id(7)]);
    }
}

#[test]
fn without_through_it_cannot_be_restored() {
    let turns = session(&[TEXT, TEXT]);
    let mut l = ledger(1, 3, &[1, 3]);
    l.inputs.through = None;
    assert_eq!(
        restore(&turns, 1, &[l], &[]),
        Err(RestoreError::NoLedger {
            segment: segment_id(id(1))
        })
    );
    // ほかの区切りとほかの呼び出しの台帳は使わない。
    let other_segment = ledger(3, 3, &[3]);
    let mut other_call = ledger(1, 3, &[1, 3]);
    other_call.call = "topic_title".to_owned();
    assert!(matches!(
        restore(&turns, 1, &[other_segment, other_call], &[]),
        Err(RestoreError::NoLedger { .. })
    ));
}

#[test]
fn a_missing_last_pair_is_refused() {
    let mut turns = session(&[TEXT, TEXT, TEXT]);
    turns.retain(|t| t.envelope.id != id(5));
    assert!(matches!(
        restore(&turns, 1, &[ledger(1, 5, &[1])], &[]),
        Err(RestoreError::LastPairMissing { turn, .. }) if turn == id(5)
    ));
}

#[test]
fn a_missing_or_added_middle_utterance_is_refused() {
    // 間の発話 3 が読めない: 数えると最初の往復が 1 の前にずれる (数が足りない) か、別の発話になる。
    let mut turns = session(&[TEXT, TEXT, TEXT, TEXT]);
    turns.retain(|t| t.envelope.id != id(3));
    assert!(matches!(
        restore(&turns, 1, &[ledger(1, 5, &[1, 5])], &[]),
        Err(RestoreError::PairCount { .. })
    ));
    assert!(matches!(
        restore(&turns, 3, &[ledger(3, 7, &[5, 7])], &[]),
        Err(RestoreError::FirstPair { .. })
    ));
    // 台帳の後に、区切りの中に発話が増えた (読めなかった発話が読めるようになった)。
    let turns = session(&[TEXT, TEXT, TEXT, TEXT]);
    let mut l = ledger(3, 7, &[3, 7]);
    l.inputs.through = Some(Through {
        turn: id(7),
        pairs: 2,
    });
    assert!(matches!(
        restore(&turns, 3, &[l], &[]),
        Err(RestoreError::FirstPair { .. })
    ));
}

#[test]
fn disagreeing_ledgers_are_refused() {
    let turns = session(&[TEXT, TEXT, TEXT, TEXT]);
    let good = ledger(1, 5, &[1, 3, 5]);
    let mut bad = ledger(1, 7, &[1, 7]);
    bad.inputs.through = Some(Through {
        turn: id(7),
        pairs: 3,
    });
    assert!(matches!(
        restore(&turns, 1, &[good, bad], &[]),
        Err(RestoreError::FirstPair { .. })
    ));
}

#[test]
fn turns_outside_the_pairs_are_refused() {
    let turns = session(&[TEXT, TEXT, TEXT]);
    let sentence_outside = ledger(1, 3, &[1, 5]);
    assert_eq!(
        restore(&turns, 1, &[sentence_outside], &[]),
        Err(RestoreError::Outside {
            ledger: stored(&[ledger(1, 1, &[])])[0].id,
            turn: id(5)
        })
    );
    let mut corr_outside = ledger(1, 3, &[1, 3]);
    corr_outside.inputs.corr = vec![CorrInput {
        id: 1,
        turn: id(5),
        rev: 1,
        pattern: "p".to_owned(),
        new: ByteRange { start: 0, end: 3 },
        old: None,
        extra: UnknownFields::new(),
    }];
    assert!(matches!(
        restore(&turns, 1, &[corr_outside], &[]),
        Err(RestoreError::Outside { turn, .. }) if turn == id(5)
    ));
    // 応答 6 は発話 5 への応答で、区切りの外。読めない応答 (99) は確かめない。
    let mut refs = ledger(1, 3, &[1, 3]);
    refs.inputs.refs = vec![LedgerRef::new(id(2), 1), LedgerRef::new(id(99), 1)];
    assert!(restore(&turns, 1, &[refs.clone()], &[]).is_ok());
    refs.inputs.refs.push(LedgerRef::new(id(6), 1));
    assert!(matches!(
        restore(&turns, 1, &[refs], &[]),
        Err(RestoreError::Outside { turn, .. }) if turn == id(6)
    ));
}

#[test]
fn overlapping_a_recorded_segment_is_refused() {
    let turns = session(&[TEXT, TEXT, TEXT, TEXT]);
    let known = SegmentBody {
        session: S,
        pairs: vec![id(5), id(7)],
        closed: true,
        chars: 0,
    };
    assert_eq!(
        restore(
            &turns,
            3,
            &[ledger(3, 5, &[3])],
            std::slice::from_ref(&known)
        ),
        Err(RestoreError::Overlap {
            segment: segment_id(id(5))
        })
    );
    // 範囲が交わるだけでも重なりとする。
    let around = SegmentBody {
        pairs: vec![id(1), id(7)],
        ..known.clone()
    };
    assert!(matches!(
        restore(&turns, 3, &[ledger(3, 5, &[3])], &[around]),
        Err(RestoreError::Overlap { .. })
    ));
    // 前の区切りとは重ならない。
    let before = SegmentBody {
        pairs: vec![id(1)],
        ..known
    };
    assert!(restore(&turns, 3, &[ledger(3, 5, &[3])], &[before]).is_ok());
}

#[test]
fn a_restored_segment_is_kept_by_the_plan_and_later_pairs_start_a_new_one() {
    let turns = session(&[TEXT, TEXT, TEXT, TEXT, TEXT]);
    let restored = restore(&turns, 1, &[ledger(1, 5, &[1, 3, 5])], &[]).expect("restores");
    let plan = plan_segments(
        S,
        MODE_CHAT,
        &turns,
        std::slice::from_ref(&restored),
        Timestamp::from_millis(T0),
    )
    .expect("plans");
    assert_eq!(plan.segments.len(), 2);
    assert_eq!(plan.segments[0].body, restored);
    assert_eq!(plan.segments[0].change, SegmentChange::Kept);
    assert_eq!(plan.segments[1].body.pairs, vec![id(7), id(9)]);
}
