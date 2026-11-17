//! 統合の回帰ケース (`tests/memory-cases/consolidation.toml`、09 §3.5) を、純粋な核 (`fold`) で走らせる。
//!
//! 書式は `tests/memory-cases/README.md`。まだ作っていない操作 (忘れる・セッションの削除、M3.4) を
//! 使う事例は `PENDING` に明示して載せ、黙って飛ばさない。`PENDING` の事例が通ったら、一覧から外すよう
//! 試験を落とす。
//!
//! verifies: AC-FR-304-3 (候補の順を入れ替えても同じ)、AC-FR-305-1 (単値・多値の置き換え)

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use evorag_types::Id;
use toml::Value;

use crate::fold::{
    Candidate, Directive, DirectiveKind, Directives, Event, FactSet, FactState, FoldEnv, Mood,
    Scope, Source, Tense, VerifiedCorrection, fold, refresh_private, table_order,
};
use crate::lexical::signals;
use crate::rules::attributes;
use crate::value_key::{ForgetKey, normalize_v1, value_key};

/// M3.4 (忘れる・セッションの削除) で通す事例。
const PENDING: &[(&str, &str)] = &[
    ("C21", "M3.4: delete_session"),
    ("C22", "M3.4: delete_session"),
    ("C23", "M3.4: delete_session"),
    ("C28", "M3.4: forget"),
    ("C29", "M3.4: forget"),
    ("C34", "M3.4: forget"),
    ("C35", "M3.4: forget"),
];

fn cases() -> Vec<Value> {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let path = dir.join("../../tests/memory-cases/consolidation.toml");
    let text = std::fs::read_to_string(&path).expect("consolidation.toml");
    let doc: Value = toml::from_str(&text).expect("parses");
    doc["case"].as_array().expect("cases").clone()
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// `at` の時刻 (ミリ秒) と通し番号から、時刻の順に並ぶ ID を作る。
fn id_at(ms: i64, seq: u64) -> Id {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x70;
    b[8..16].copy_from_slice(&seq.to_be_bytes());
    Id::from_bytes(b)
}

fn parse_ms(at: &str) -> i64 {
    // "2026-01-05T10:00:00Z" を、比べられるだけの単調な値にする (暦の正しさは要らない)。
    let digits: String = at.chars().filter(char::is_ascii_digit).collect();
    digits.parse::<i64>().expect("at")
}

enum Outcome {
    Passed,
    Pending(&'static str),
}

struct Runner {
    key: ForgetKey,
    seq: u64,
    last_ms: i64,
    turns: BTreeMap<String, (Id, String)>,
    set: FactSet,
    dirs: Directives,
    /// それまでに当てた候補と訂正 (作り直しと順の入れ替えで当て直す)。
    history: Vec<Event>,
    batch: Vec<Event>,
}

impl Runner {
    fn new() -> Self {
        Self {
            key: ForgetKey::from_bytes([1; 32]),
            seq: 0,
            last_ms: 0,
            turns: BTreeMap::new(),
            set: FactSet::default(),
            dirs: Directives::default(),
            history: Vec::new(),
            batch: Vec::new(),
        }
    }

    fn next_id(&mut self, ms: i64) -> Id {
        self.seq += 1;
        id_at(ms, self.seq)
    }

    fn fold_events(&mut self, set: &mut FactSet, events: Vec<Event>) {
        let mut seq = self.seq;
        let ms = self.last_ms;
        let mut new_id = || {
            seq += 1;
            id_at(ms, seq)
        };
        let mut env = FoldEnv {
            key: &self.key,
            attributes: attributes(),
            new_id: &mut new_id,
        };
        fold(set, &self.dirs, events, &mut env);
        self.seq = seq;
    }

    fn flush(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.batch);
        self.history.extend(batch.iter().cloned());
        let mut set = std::mem::take(&mut self.set);
        self.fold_events(&mut set, batch);
        self.set = set;
    }

    /// 事実を捨て、それまでの候補と訂正と、採る・戻すを当て直す (`rebuild --facts`・`shuffle`)。
    fn replay(&mut self, reverse: bool) {
        let mut events = self.history.clone();
        if reverse {
            events.reverse();
        }
        events.extend(self.dirs.choices());
        let mut set = FactSet::default();
        self.fold_events(&mut set, events);
        self.set = set;
    }

    fn target(&self, op: &Value) -> (String, String, Option<crate::value_key::ValueKey>) {
        let t = &op["target"];
        let subject = normalize_v1(s(t, "s").expect("s"));
        let attribute = s(t, "a").expect("a").to_owned();
        let vk = s(t, "v").map(|v| value_key(&self.key, v));
        (subject, attribute, vk)
    }

    /// 同じ (主語, 属性, 値の鍵) の、まだ解かれていない最後の指示。
    fn find_directive(&self, kind: fn(&DirectiveKind) -> bool, op: &Value) -> Id {
        let (subject, attribute, vk) = self.target(op);
        self.dirs
            .iter()
            .filter(|d| kind(&d.kind) && d.subject == subject && d.attribute == attribute)
            .filter(|d| d.vk == vk)
            .map(|d| d.id)
            .last()
            .expect("a directive to cancel")
    }

    fn step(&mut self, id: &str, step: &Value) -> Result<(), Outcome> {
        let (name, body) = step.as_table().expect("table").iter().next().expect("one");
        match name.as_str() {
            "turn" => {
                self.flush();
                let ms = parse_ms(s(body, "at").expect("at"));
                self.last_ms = ms;
                let tid = self.next_id(ms);
                self.turns.insert(
                    s(body, "id").expect("id").to_owned(),
                    (tid, s(body, "text").expect("text").to_owned()),
                );
            }
            "candidate" => {
                let (turn, text) = self.turns[s(body, "turn").expect("turn")].clone();
                let e = s(body, "e").expect("e");
                let start = text.find(e).expect("evidence in the turn");
                let subject = normalize_v1(s(body, "s").expect("s"));
                let value = s(body, "v").expect("v").to_owned();
                let c = Candidate {
                    source: Source {
                        turn,
                        start: u32::try_from(start).expect("short"),
                        end: u32::try_from(start + e.len()).expect("short"),
                        rev: 0,
                    },
                    signals: signals(e, &subject),
                    subject,
                    attribute: s(body, "a").expect("a").to_owned(),
                    vk: value_key(&self.key, &value),
                    value,
                    mood: match s(body, "m").unwrap_or("fact") {
                        "fact" => Mood::Fact,
                        "negated" => Mood::Negated,
                        other => panic!("{id}: mood `{other}` does not reach consolidation"),
                    },
                    tense: match s(body, "tn").unwrap_or("current") {
                        "past" => Tense::Past,
                        "planned" => Tense::Planned,
                        _ => Tense::Current,
                    },
                    old_vk: s(body, "old").map(|o| value_key(&self.key, o)),
                };
                self.batch.push(Event::Candidate(c));
            }
            "correction" => {
                let (turn, text) = self.turns[s(body, "turn").expect("turn")].clone();
                let new = s(body, "new").expect("new");
                let start = text.find(new).expect("new in the turn");
                let c = VerifiedCorrection::assume_verified(
                    Source {
                        turn,
                        start: u32::try_from(start).expect("short"),
                        end: u32::try_from(start + new.len()).expect("short"),
                        rev: 0,
                    },
                    value_key(&self.key, s(body, "old").expect("old")),
                    new.to_owned(),
                    value_key(&self.key, new),
                );
                self.batch.push(Event::Correction(c));
            }
            "op" => {
                self.flush();
                let kind = s(body, "kind").expect("kind");
                let did = self.next_id(self.last_ms);
                match kind {
                    "forget" => return Err(Outcome::Pending("M3.4: forget")),
                    "delete_session" => return Err(Outcome::Pending("M3.4: delete_session")),
                    "rebuild_facts" => self.replay(false),
                    "shuffle" => self.replay(true),
                    "private" | "accept" | "revert" => {
                        let (subject, attribute, vk) = self.target(body);
                        let dk = match kind {
                            "private" => DirectiveKind::Private {
                                scope: match s(body, "scope").unwrap_or("successors") {
                                    "value_only" => Scope::ValueOnly,
                                    _ => Scope::Successors,
                                },
                            },
                            "accept" => DirectiveKind::Accept,
                            _ => DirectiveKind::Revert,
                        };
                        self.dirs.push(Directive {
                            id: did,
                            kind: dk,
                            subject: subject.clone(),
                            attribute: attribute.clone(),
                            vk,
                        });
                        if kind == "private" {
                            refresh_private(&mut self.set, &self.dirs);
                        } else {
                            let ev = Event::Choose {
                                directive: did,
                                subject,
                                attribute,
                                vk: vk.expect("a value"),
                                revert: kind == "revert",
                            };
                            let mut set = std::mem::take(&mut self.set);
                            self.fold_events(&mut set, vec![ev]);
                            self.set = set;
                        }
                    }
                    "private_off" => {
                        let target = self
                            .find_directive(|k| matches!(k, DirectiveKind::Private { .. }), body);
                        let (subject, attribute, vk) = self.target(body);
                        self.dirs.push(Directive {
                            id: did,
                            kind: DirectiveKind::PrivateOff { target },
                            subject,
                            attribute,
                            vk,
                        });
                        refresh_private(&mut self.set, &self.dirs);
                    }
                    "unblock" => return Err(Outcome::Pending("M3.4: forget")),
                    other => panic!("{id}: unknown op `{other}`"),
                }
            }
            "expect" => {
                self.flush();
                self.check(id, body);
            }
            "tamper" => return Err(Outcome::Pending("M3.4: forget")),
            other => panic!("{id}: unknown step `{other}`"),
        }
        Ok(())
    }

    fn value_of(&self, fid: Id) -> String {
        self.set
            .facts
            .iter()
            .find(|f| f.id == fid)
            .and_then(|f| f.value.clone())
            .unwrap_or_else(|| "(deleted)".into())
    }

    fn turn_name(&self, tid: Id) -> String {
        self.turns
            .iter()
            .find(|(_, (t, _))| *t == tid)
            .map(|(k, _)| k.clone())
            .expect("a known turn")
    }

    fn check(&self, id: &str, body: &Value) {
        if let Some(facts) = body.get("facts").and_then(Value::as_array) {
            // 実際の事実を、期待と同じ形の文字列にして、集合として比べる。
            let actual: BTreeSet<String> = self
                .set
                .facts
                .iter()
                .map(|f| {
                    let (state, extra) = match f.state {
                        FactState::Live => ("Live", String::new()),
                        FactState::Contested { against } => {
                            ("Contested", format!(" against={}", self.value_of(against)))
                        }
                        FactState::Superseded { by, .. } => (
                            "Superseded",
                            by.map(|b| format!(" by={}", self.value_of(b)))
                                .unwrap_or_default(),
                        ),
                        FactState::Retracted => ("Retracted", String::new()),
                    };
                    let tense = match f.tense {
                        Tense::Current => "current",
                        Tense::Past => "past",
                        Tense::Planned => "planned",
                    };
                    let sources: BTreeSet<String> =
                        f.sources.iter().map(|s| self.turn_name(s.turn)).collect();
                    format!(
                        "{}/{}/{} {state}{extra} tn={tense} private={} sources={sources:?}",
                        f.subject,
                        f.attribute,
                        f.value.clone().unwrap_or_default(),
                        f.private
                    )
                })
                .collect();
            let expected: BTreeSet<String> = facts
                .iter()
                .map(|f| {
                    let state = s(f, "state").expect("state");
                    let extra = match state {
                        "Contested" => format!(" against={}", s(f, "against").expect("against")),
                        "Superseded" => s(f, "by").map(|b| format!(" by={b}")).unwrap_or_default(),
                        _ => String::new(),
                    };
                    let sources: BTreeSet<String> = f["sources"]
                        .as_array()
                        .expect("sources")
                        .iter()
                        .map(|v| v.as_str().expect("str").to_owned())
                        .collect();
                    format!(
                        "{}/{}/{} {state}{extra} tn={} private={} sources={sources:?}",
                        normalize_v1(s(f, "s").expect("s")),
                        s(f, "a").expect("a"),
                        s(f, "v").unwrap_or_default(),
                        s(f, "tn").unwrap_or("current"),
                        f.get("private").and_then(Value::as_bool).unwrap_or(false)
                    )
                })
                .collect();
            assert_eq!(actual, expected, "{id}: facts");
        }
        if let Some(table) = body.get("table").and_then(Value::as_array) {
            let actual: Vec<String> = table_order(&self.set, attributes())
                .into_iter()
                .map(|f| {
                    format!(
                        "{}/{}/{}",
                        f.subject,
                        f.attribute,
                        f.value.clone().unwrap_or_default()
                    )
                })
                .collect();
            let expected: Vec<String> = table
                .iter()
                .map(|v| {
                    let v = v.as_str().expect("str");
                    let (subject, rest) = v.split_once('/').expect("s/a/v");
                    format!("{}/{rest}", normalize_v1(subject))
                })
                .collect();
            assert_eq!(actual, expected, "{id}: table");
        }
    }
}

#[test]
fn consolidation_cases_hold_on_the_pure_core() {
    let mut pending = BTreeSet::new();
    let mut passed = 0;
    for case in cases() {
        let id = s(&case, "id").expect("id").to_owned();
        let mut r = Runner::new();
        let mut outcome = Outcome::Passed;
        for step in case["step"].as_array().expect("steps") {
            if let Err(o) = r.step(&id, step) {
                outcome = o;
                break;
            }
        }
        match outcome {
            Outcome::Passed => {
                assert!(
                    !PENDING.iter().any(|(p, _)| *p == id),
                    "{id} passes now: remove it from PENDING"
                );
                passed += 1;
            }
            Outcome::Pending(why) => {
                let listed = PENDING.iter().find(|(p, _)| *p == id);
                assert_eq!(listed.map(|l| l.1), Some(why), "{id}: pending for `{why}`");
                pending.insert(id);
            }
        }
    }
    let listed: BTreeSet<String> = PENDING.iter().map(|(p, _)| (*p).to_owned()).collect();
    assert_eq!(
        pending, listed,
        "PENDING must match the cases that need later steps"
    );
    assert!(passed >= 29, "passed {passed}");
}
