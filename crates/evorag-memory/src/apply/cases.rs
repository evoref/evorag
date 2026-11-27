//! M5.0 の事例 (`tests/memory-cases/v0.5/`、書式は同じ場所の `README.md`) のうち、指示の当て方の純粋な関数だけで
//! 判定できる部分を走らせる (M5.2)。
//!
//! 走らせるのは、台帳の掃除 (04 §7.2) の期待 `ledgers` と、区切りの鍵の期待 `seg_keys` (掃除と private の
//! ふるい、§5.5)、伏せ字にしたターンの本文の期待 `turns` (走らせる側の伏せ字の作り方の確かめ)。単位・プロファイル・
//! 想起など、導出と書き込みの経路の要る期待は読み飛ばす (M5.3・M5.4 の走らせる側で判定する)。
//!
//! 文番号は、利用者の発話を文の分け方 v1 ([`split_v1`]) で切って振る。
//!
//! 走らせる側の近似 (事例の書式が走らせる側に任せている部分。M5.4 の走らせる側で本物に替える):
//! - 訂正の候補は「X じゃなくて Y です」の型だけを見つける (訂正の字句の型の代わり)。
//! - 忘れる単位は、すべての台帳の単位の候補のうち、当て方の述語 ([`DirectiveSet::forgotten_by`]) で新しい忘れるに
//!   当たるもの (関門 1〜7 は当てない)。伏せ字 (と `[非表示]`) の範囲は、その根拠の発話の全体と同じ往復の応答の
//!   値の一致 (04 §7.1 の確認の画面の既定)。
//! - 区切りの鍵は、区切りの最新の台帳の `keys` から private のふるいで除いたもの (関門 6 などは当てない)。

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;

use evorag_store::forget::Redaction;
use evorag_types::Id;
use toml::Value;

use super::{
    DirectiveSet, Located, Probe, ProbeAspect, ResolvedEntity, ScrubContext, SubstringKeys,
    TurnTexts, ValueKeys,
};
use crate::directive::{DirectiveBody, DirectiveOp, DirectiveRedaction, PrivateScope, TurnSpan};
use crate::ledger::{
    ByteRange, CALL_SEGMENT, CorrInput, LedgerBody, LedgerCorrectionOut, LedgerLayer, LedgerMood,
    LedgerOutput, LedgerRef, LedgerTense, LedgerUnitOut, LedgerVerdict, SentenceSpan,
    UnknownFields,
};
use crate::sentences::split_v1;
use crate::value_key::{ForgetKey, aspect_key, normalize_v1};

const K: ForgetKey = ForgetKey::from_bytes_const([0x42; 32]);

/// 走らせる事例 (ファイル, ID)。ほかの事例は、導出か書き込みの経路の期待が要る (M5.3・M5.4)。
const RUNNABLE: &[(&str, &str)] = &[
    ("forget.toml", "FG02"),
    ("forget.toml", "FG03"),
    ("forget.toml", "FG06"),
    ("forget.toml", "FG30"),
    ("forget.toml", "FG31"),
    ("forget.toml", "FG40"),
    ("forget.toml", "FG41"),
    ("forget.toml", "FG42"),
    ("forget.toml", "FG50"),
    ("governance.toml", "GV01"),
    ("governance.toml", "GV02"),
];

/// 走らせる側が当てない操作 (導出・job・chat の送信・選択は、台帳の掃除と区切りの鍵の期待を変えない)。
const IGNORED_OPS: &[&str] = &["derive", "jobs", "send", "select"];

/// 主語を明示する語 (走らせる側の近似。04 §5.1 の `subject_self` の一部)。
const SELF_WORDS: &[&str] = &["私", "僕", "自分", "i", "me"];

fn root() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

fn case(file: &str, id: &str) -> Value {
    let path = root().join("../../tests/memory-cases/v0.5").join(file);
    let text = std::fs::read_to_string(&path).expect(file);
    let doc: Value = toml::from_str(&text).expect("parses");
    doc["case"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("{file}: no case {id}"))
        .clone()
}

/// 同梱の辞書の属性の名前。
fn dictionary() -> BTreeSet<String> {
    let text = std::fs::read_to_string(root().join("attributes.toml")).expect("attributes.toml");
    let doc: Value = toml::from_str(&text).expect("parses");
    doc["attribute"]
        .as_array()
        .expect("attributes")
        .iter()
        .filter_map(|a| a["name"].as_str().map(str::to_owned))
        .collect()
}

/// 走らせる側の今の規則 (主語を明示する語を `self` に、ほかは正規化 v1 に)。
fn resolve(raw: &str) -> String {
    let n = normalize_v1(raw);
    if SELF_WORDS.contains(&n.as_str()) {
        "self".to_owned()
    } else {
        n
    }
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn to_u32(p: usize) -> u32 {
    u32::try_from(p).expect("fits")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Redact,
    Keep,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Redact => "redact",
            Mode::Keep => "keep",
        }
    }
}

#[derive(Debug, Clone)]
struct Turn {
    id: Id,
    session: String,
    user: bool,
    reply_to: Option<String>,
    rev: u32,
    text: String,
    /// 伏せ字の前の本文 (改訂の番号ごと)。
    before: Vec<(u32, String)>,
}

#[derive(Debug, Clone)]
struct Ledger {
    name: String,
    seg: String,
    body: LedgerBody,
}

struct Run {
    case: String,
    mode: Option<Mode>,
    marker: &'static str,
    dict: BTreeSet<String>,
    next: u64,
    turns: BTreeMap<String, Turn>,
    order: Vec<String>,
    segments: BTreeMap<String, Vec<String>>,
    ledgers: Vec<Ledger>,
    directives: Vec<(Id, DirectiveBody)>,
    forgets: BTreeMap<String, Id>,
    snapshot: BTreeMap<String, LedgerBody>,
    /// 判定した期待の数 (台帳ごと・区切りごと・ターンごと)。
    checks: usize,
}

impl Run {
    fn new(case: &Value, mode: Option<Mode>, dict: BTreeSet<String>) -> Self {
        let marker = if s(case, "lang") == Some("en") {
            "[redacted]"
        } else {
            "[伏せ字]"
        };
        Self {
            case: s(case, "id").expect("id").to_owned(),
            mode,
            marker,
            dict,
            next: 0,
            turns: BTreeMap::new(),
            order: Vec::new(),
            segments: BTreeMap::new(),
            ledgers: Vec::new(),
            directives: Vec::new(),
            forgets: BTreeMap::new(),
            snapshot: BTreeMap::new(),
            checks: 0,
        }
    }

    fn label(&self) -> String {
        match self.mode {
            Some(m) => format!("{} ({})", self.case, m.name()),
            None => self.case.clone(),
        }
    }

    /// 段の順に増える ID (ターンも指示も同じ列に並ぶ。UUID v7 の順の代わり)。
    fn next_id(&mut self) -> Id {
        self.next += 1;
        let mut b = [0u8; 16];
        b[8..].copy_from_slice(&self.next.to_be_bytes());
        Id::from_bytes(b)
    }

    fn set(&self) -> DirectiveSet {
        DirectiveSet::new(self.directives.iter().cloned(), self.marker, resolve)
    }

    fn turn(&mut self, t: &Value) {
        let name = s(t, "id").expect("turn id").to_owned();
        let session = s(t, "session").expect("session").to_owned();
        let user = s(t, "role").unwrap_or("user") == "user";
        let reply_to = if user {
            None
        } else {
            s(t, "reply_to").map(str::to_owned).or_else(|| {
                self.order
                    .iter()
                    .rev()
                    .find(|n| self.turns[*n].user && self.turns[*n].session == session)
                    .cloned()
            })
        };
        let id = self.next_id();
        self.turns.insert(
            name.clone(),
            Turn {
                id,
                session,
                user,
                reply_to,
                rev: 0,
                text: s(t, "text").expect("text").to_owned(),
                before: Vec::new(),
            },
        );
        self.order.push(name);
    }

    fn texts(&self) -> TurnTexts<'_> {
        let mut out = TurnTexts::new();
        for t in self.turns.values() {
            out.insert(t.id, t.rev, &t.text);
            for (rev, text) in &t.before {
                out.insert(t.id, *rev, text);
            }
        }
        out
    }

    fn first_turn(&self, seg: &str) -> Id {
        self.turns[&self.segments[seg][0]].id
    }

    fn scrub(&mut self, which: Option<usize>) {
        let set = self.set();
        let mut ledgers = std::mem::take(&mut self.ledgers);
        let firsts: Vec<Id> = ledgers.iter().map(|l| self.first_turn(&l.seg)).collect();
        let texts = self.texts();
        for (i, l) in ledgers.iter_mut().enumerate() {
            if which.is_some_and(|w| w != i) {
                continue;
            }
            super::scrub_ledger(
                &mut l.body,
                &ScrubContext {
                    key: &K,
                    directives: &set,
                    first_turn: Some(firsts[i]),
                    texts: &texts,
                },
            )
            .expect("scrub");
        }
        self.ledgers = ledgers;
    }

    fn ledger(&mut self, l: &Value) {
        let name = s(l, "name").expect("ledger name").to_owned();
        let seg = s(l, "seg").expect("seg").to_owned();
        let pairs = self.segments[&seg].clone();
        let mut body = LedgerBody {
            call: CALL_SEGMENT.to_owned(),
            schema: 1,
            variant: s(l, "variant").unwrap_or("v1").to_owned(),
            model_key: s(l, "model").unwrap_or("now").to_owned(),
            status: s(l, "status").unwrap_or("complete").to_owned(),
            ..LedgerBody::default()
        };
        body.inputs.input_hash = Some("0".repeat(64));
        let mut n = 0;
        let mut corr_id = 0;
        for p in &pairs {
            let t = &self.turns[p];
            for r in split_v1(&t.text) {
                n += 1;
                body.sentences.push(SentenceSpan::new(
                    n,
                    t.id,
                    t.rev,
                    to_u32(r.start),
                    to_u32(r.end),
                ));
            }
            if let Some((new, old)) = correction_candidate(&t.text) {
                corr_id += 1;
                body.inputs.corr.push(CorrInput {
                    id: corr_id,
                    turn: t.id,
                    rev: t.rev,
                    pattern: "instead".to_owned(),
                    new: ByteRange {
                        start: to_u32(new.start),
                        end: to_u32(new.end),
                    },
                    old: Some(ByteRange {
                        start: to_u32(old.start),
                        end: to_u32(old.end),
                    }),
                    extra: UnknownFields::new(),
                });
            }
            for a in self.turns.values() {
                if !a.user && a.reply_to.as_deref() == Some(p.as_str()) {
                    body.inputs.refs.push(LedgerRef::new(a.id, a.rev));
                }
            }
        }
        let keys = strings(l.get("keys"));
        body.output
            .set_section(LedgerOutput::KEYS, &keys)
            .expect("keys");
        let units: Vec<LedgerUnitOut> = l
            .get("units")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(unit_out).collect())
            .unwrap_or_default();
        body.output
            .set_section(LedgerOutput::UNITS, &units)
            .expect("units");
        let corrections: Vec<LedgerCorrectionOut> = l
            .get("corrections")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|c| LedgerCorrectionOut {
                        id: u32::try_from(c["id"].as_integer().expect("id")).expect("fits"),
                        verdict: match s(c, "verdict").expect("verdict") {
                            "accept" => LedgerVerdict::Accept,
                            "reject" => LedgerVerdict::Reject,
                            _ => LedgerVerdict::Unclear,
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();
        body.output
            .set_section(LedgerOutput::CORRECTIONS, &corrections)
            .expect("corrections");
        self.ledgers.push(Ledger { name, seg, body });
        // 台帳を書く前に、効いているすべての忘れるの指示で掃除する (04 §7.5)。
        self.scrub(Some(self.ledgers.len() - 1));
    }

    /// 事例の `target` の (entity, 辞書の属性の名前か自由記述の属性の鍵, 値)。
    fn target(&self, t: &Value) -> (String, Result<String, String>, String) {
        let entity = normalize_v1(s(t, "entity").unwrap_or("self"));
        let aspect = s(t, "aspect").expect("aspect");
        let free = t
            .get("free")
            .and_then(Value::as_bool)
            .unwrap_or(!self.dict.contains(aspect));
        let aspect = if free {
            Err(aspect_key(&K, aspect).to_hex())
        } else {
            Ok(aspect.to_owned())
        };
        (entity, aspect, s(t, "value").expect("value").to_owned())
    }

    fn directive(&self, op: DirectiveOp, t: &Value) -> DirectiveBody {
        let (entity, aspect, value) = self.target(t);
        let mut b = DirectiveBody::new(op);
        b.entity = Some(entity);
        match aspect {
            Ok(a) => b.aspect = Some(a),
            Err(m) => b.aspect_mac = Some(m),
        }
        b.vks = ValueKeys::of(&K, &value).to_hex();
        b
    }

    /// 台帳の単位の候補の鍵 (属性の決め方と主語の明示の語の近似)。
    fn probes(&self) -> Vec<(Id, ResolvedEntity, Result<String, String>, ValueKeys)> {
        let mut out = Vec::new();
        for l in &self.ledgers {
            for u in l.body.output.units().items {
                let Some(sent) = l.body.sentences.iter().find(|x| x.s == u.sentence) else {
                    continue;
                };
                let entity = ResolvedEntity::new(&u.entity, resolve);
                let a = normalize_v1(&u.aspect);
                let aspect = if self.dict.contains(&a) {
                    Ok(a)
                } else {
                    Err(aspect_key(&K, &u.aspect).to_hex())
                };
                out.push((sent.turn, entity, aspect, ValueKeys::of(&K, &u.value)));
            }
        }
        out
    }

    fn forget(&mut self, op: &Value) {
        let t = &op["target"];
        let redact = op
            .get("redact")
            .and_then(Value::as_bool)
            .unwrap_or(self.mode != Some(Mode::Keep));
        let value = self.target(t).2;
        let id = self.next_id();
        let mut body = self.directive(DirectiveOp::Forget, t);
        body.redact = redact;
        // 忘れる単位の根拠のターン: 当て方の述語で新しい忘れるに当たる候補のターン。
        let probe_set = DirectiveSet::new([(id, body.clone())], self.marker, resolve);
        let mut evidence: BTreeSet<Id> = BTreeSet::new();
        for (turn, entity, aspect, keys) in self.probes() {
            let pa = match &aspect {
                Ok(name) => ProbeAspect::Dict {
                    name,
                    was_free: None,
                },
                Err(mac) => ProbeAspect::Free { mac },
            };
            if probe_set
                .forgotten_by(&Probe::new(entity, pa, keys, turn))
                .is_some()
            {
                evidence.insert(turn);
            }
        }
        body.turns = evidence.iter().copied().collect();
        // 伏せる範囲: 根拠の発話の全体と、同じ往復の応答の値の一致。
        let names: Vec<String> = self
            .order
            .iter()
            .filter(|n| {
                let t = &self.turns[*n];
                evidence.contains(&t.id)
                    || t.reply_to
                        .as_ref()
                        .is_some_and(|r| evidence.contains(&self.turns[r].id))
            })
            .cloned()
            .collect();
        let vks = probe_set.forgets()[0].vks().clone();
        for n in names {
            let t = self.turns.get_mut(&n).expect("turn");
            let Located::Ranges(ranges) =
                super::locate_forgotten(&K, &vks, &t.text, None, &[value.as_str()])
            else {
                panic!("{}: cannot place {value} in {n}", self.case);
            };
            if ranges.is_empty() {
                continue;
            }
            if redact {
                let (r, after) =
                    Redaction::plan(&t.text, t.rev, ranges, self.marker).expect("plan");
                body.redactions
                    .extend(r.ranges().iter().map(|x| DirectiveRedaction {
                        turn: t.id,
                        rev_before: r.rev_before(),
                        rev_after: r.rev_after(),
                        start: to_u32(x.start),
                        end: to_u32(x.end),
                        hash: r.hash().to_string(),
                    }));
                let before = std::mem::replace(&mut t.text, after);
                t.before.push((t.rev, before));
                t.rev = r.rev_after();
            } else {
                body.hidden
                    .extend(ranges.into_iter().map(|x: Range<usize>| TurnSpan {
                        turn: t.id,
                        rev: t.rev,
                        start: to_u32(x.start),
                        end: to_u32(x.end),
                    }));
            }
        }
        if let Some(name) = s(op, "name") {
            self.forgets.insert(name.to_owned(), id);
        }
        self.directives.push((id, body));
        self.scrub(None);
    }

    fn op(&mut self, op: &Value) {
        let kind = s(op, "kind").expect("op kind");
        match kind {
            "forget" => self.forget(op),
            "unblock" => {
                let target = self.forgets[s(op, "forget").expect("forget name")];
                let id = self.next_id();
                let mut b = DirectiveBody::new(DirectiveOp::Unblock);
                b.target = Some(target);
                self.directives.push((id, b));
            }
            "private" => {
                let id = self.next_id();
                let mut b = self.directive(DirectiveOp::Private, &op["target"]);
                b.scope = Some(
                    s(op, "scope")
                        .unwrap_or(PrivateScope::Successors.name())
                        .to_owned(),
                );
                self.directives.push((id, b));
            }
            "private_off" => {
                let want = self.directive(DirectiveOp::Private, &op["target"]);
                let set = self.set();
                let targets: Vec<Id> = self
                    .directives
                    .iter()
                    .filter(|(id, b)| {
                        b.op() == DirectiveOp::Private
                            && set.privates().iter().any(|p| p.id() == *id)
                            && (&b.entity, &b.aspect, &b.aspect_mac, &b.vks)
                                == (&want.entity, &want.aspect, &want.aspect_mac, &want.vks)
                    })
                    .map(|(id, _)| *id)
                    .collect();
                assert!(
                    !targets.is_empty(),
                    "{}: private_off with no private",
                    self.label()
                );
                for t in targets {
                    let id = self.next_id();
                    let mut b = DirectiveBody::new(DirectiveOp::PrivateOff);
                    b.target = Some(t);
                    self.directives.push((id, b));
                }
            }
            k if IGNORED_OPS.contains(&k) => {}
            k => panic!("{}: the runner does not know op {k}", self.label()),
        }
    }

    fn expect(&mut self, e: &Value) {
        if let Some(m) = s(e, "mode")
            && self.mode.map(Mode::name) != Some(m)
        {
            return;
        }
        let label = self.label();
        if let Some(ls) = e.get("ledgers").and_then(Value::as_array) {
            for x in ls {
                self.expect_ledger(&label, x);
                self.checks += 1;
            }
        }
        if let Some(seg_keys) = e.get("seg_keys").and_then(Value::as_table) {
            let set = self.set();
            for (seg, want) in seg_keys {
                let l = self
                    .ledgers
                    .iter()
                    .rev()
                    .find(|l| &l.seg == seg)
                    .unwrap_or_else(|| panic!("{label}: no ledger for {seg}"));
                let got: Vec<String> = l
                    .body
                    .output
                    .keys()
                    .items
                    .into_iter()
                    .filter(|k| !set.screens_key(&SubstringKeys::of(&K, k), &BTreeSet::new()))
                    .collect();
                assert_eq!(got, strings(Some(want)), "{label}: seg_keys {seg}");
                self.checks += 1;
            }
        }
        if let Some(ts) = e.get("turns").and_then(Value::as_array) {
            for t in ts {
                let turn = &self.turns[s(t, "id").expect("id")];
                assert_eq!(turn.text, s(t, "text").expect("text"), "{label}: turn text");
                if let Some(rev) = t.get("rev").and_then(Value::as_integer) {
                    assert_eq!(i64::from(turn.rev), rev, "{label}: turn rev");
                }
                self.checks += 1;
            }
        }
        self.snapshot = self
            .ledgers
            .iter()
            .map(|l| (l.name.clone(), l.body.clone()))
            .collect();
    }

    fn expect_ledger(&self, label: &str, x: &Value) {
        let name = s(x, "name").expect("ledger name");
        let l = self
            .ledgers
            .iter()
            .find(|l| l.name == name)
            .unwrap_or_else(|| panic!("{label}: no ledger {name}"));
        let at = format!("{label}: {name}");
        if let Some(p) = x.get("present").and_then(Value::as_bool) {
            assert!(p, "{at}: the runner never deletes a ledger");
        }
        if x.get("unchanged").and_then(Value::as_bool) == Some(true) {
            assert_eq!(Some(&l.body), self.snapshot.get(name), "{at}: unchanged");
        }
        if let Some(want) = x.get("units") {
            let got: Vec<String> = l
                .body
                .output
                .units()
                .items
                .into_iter()
                .map(|u| {
                    if u.entity == "self" {
                        format!("{}={}", u.aspect, u.value)
                    } else {
                        format!("{}/{}={}", u.entity, u.aspect, u.value)
                    }
                })
                .collect();
            assert_eq!(got, strings(Some(want)), "{at}: units");
        }
        if let Some(want) = x.get("keys") {
            assert_eq!(
                l.body.output.keys().items,
                strings(Some(want)),
                "{at}: keys"
            );
        }
        let ids = |v: &Value| -> Vec<u32> {
            v.as_array()
                .expect("ids")
                .iter()
                .map(|i| u32::try_from(i.as_integer().expect("id")).expect("fits"))
                .collect()
        };
        if let Some(want) = x.get("corrections") {
            let got: Vec<u32> = l
                .body
                .output
                .corrections()
                .items
                .iter()
                .map(|c| c.id)
                .collect();
            assert_eq!(got, ids(want), "{at}: corrections");
        }
        if let Some(want) = x.get("corr") {
            let got: Vec<u32> = l.body.inputs.corr.iter().map(|c| c.id).collect();
            assert_eq!(got, ids(want), "{at}: corr");
        }
        if let Some(want) = x.get("scrubbed").and_then(Value::as_integer) {
            assert_eq!(i64::from(l.body.scrubbed), want, "{at}: scrubbed");
        }
        if let Some(want) = s(x, "input_hash") {
            let got = if l.body.inputs.input_hash.is_some() {
                "set"
            } else {
                "null"
            };
            assert_eq!(got, want, "{at}: input_hash");
        }
        if let Some(want) = s(x, "status") {
            assert_eq!(l.body.status, want, "{at}: status");
        }
        if let Some(want) = x.get("rev").and_then(Value::as_table) {
            for (turn, rev) in want {
                let id = self.turns[turn].id;
                let got: BTreeSet<u32> = l
                    .body
                    .sentences
                    .iter()
                    .filter(|x| x.turn == id)
                    .map(|x| x.rev)
                    .collect();
                let rev = u32::try_from(rev.as_integer().expect("rev")).expect("fits");
                assert_eq!(got, BTreeSet::from([rev]), "{at}: rev of {turn}");
            }
        }
    }

    fn step(&mut self, st: &Value) {
        if let Some(t) = st.get("turn") {
            self.turn(t);
        } else if let Some(seg) = st.get("segment") {
            let name = s(seg, "name").expect("segment name").to_owned();
            self.segments.insert(name, strings(seg.get("pairs")));
        } else if let Some(l) = st.get("ledger") {
            self.ledger(l);
        } else if let Some(op) = st.get("op") {
            self.op(op);
        } else if let Some(e) = st.get("expect") {
            self.expect(e);
        } else {
            panic!("{}: the runner does not know step {st:?}", self.label());
        }
    }
}

fn unit_out(u: &Value) -> LedgerUnitOut {
    LedgerUnitOut {
        sentence: u32::try_from(u["sentence"].as_integer().expect("sentence")).expect("fits"),
        layer: match s(u, "layer") {
            Some("episode") => LedgerLayer::Episode,
            _ => LedgerLayer::World,
        },
        entity: s(u, "entity").unwrap_or("self").to_owned(),
        aspect: s(u, "aspect").expect("aspect").to_owned(),
        value: s(u, "value").expect("value").to_owned(),
        mood: match s(u, "mood") {
            Some("negated") => LedgerMood::Negated,
            Some("question") => LedgerMood::Question,
            Some("hypothetical") => LedgerMood::Hypothetical,
            _ => LedgerMood::Fact,
        },
        tense: match s(u, "tense") {
            Some("past") => LedgerTense::Past,
            Some("planned") => LedgerTense::Planned,
            _ => LedgerTense::Current,
        },
        old: s(u, "old").unwrap_or_default().to_owned(),
        when: s(u, "when").unwrap_or_default().to_owned(),
    }
}

/// 「X じゃなくて Y です」の訂正の候補 (新しい値, 古い値) の範囲 (走らせる側の近似)。
fn correction_candidate(text: &str) -> Option<(Range<usize>, Range<usize>)> {
    const MID: &str = "じゃなくて";
    let mid = text.find(MID)?;
    let old_start = ["は", "、"]
        .iter()
        .filter_map(|d| text[..mid].rfind(d).map(|i| i + d.len()))
        .max()?;
    let new_start = mid + MID.len();
    let new_end = new_start + text[new_start..].find("です")?;
    Some((new_start..new_end, old_start..mid))
}

/// verifies: 04 §7.2 (台帳の掃除。M5.0 の事例 FG02・FG03・FG06・FG30・FG31・FG40・FG41・FG42・FG50 の台帳の期待)、§7.5 (当て方の述語・unblock)、§5.5 (private の区切りの鍵のふるい。GV01・GV02)
#[test]
fn m50_cases_for_the_pure_functions() {
    let dict = dictionary();
    let mut checks = 0;
    for (file, id) in RUNNABLE {
        let c = case(file, id);
        let modes: Vec<Option<Mode>> = match c.get("modes") {
            Some(_) => vec![Some(Mode::Redact), Some(Mode::Keep)],
            None => vec![None],
        };
        for mode in modes {
            let mut run = Run::new(&c, mode, dict.clone());
            for st in c["step"].as_array().expect("steps") {
                run.step(st);
            }
            assert!(run.checks > 0, "{}: no expectation was judged", run.label());
            checks += run.checks;
        }
    }
    // 事例の期待を読み飛ばしすぎていないこと (事例を足し引きしたら数え直す)。
    assert_eq!(checks, EXPECTED_CHECKS);
}

/// [`m50_cases_for_the_pure_functions`] が判定する期待の数。
const EXPECTED_CHECKS: usize = 88;

#[test]
fn v1_splits_sentences_like_the_case_notes() {
    let text = "Me? I still live in Maplewood.";
    let parts: Vec<&str> = split_v1(text).into_iter().map(|r| &text[r]).collect();
    assert_eq!(parts, ["Me?", "I still live in Maplewood."]);
    let text = "霧ヶ丘に住んでいて、柴犬を飼っています。";
    let whole = 0..text.len();
    assert_eq!(split_v1(text), [whole]);
    let text = "出身は霧ヶ丘じゃなくて白樺台です。";
    let (new, old) = correction_candidate(text).expect("candidate");
    assert_eq!((&text[new], &text[old]), ("白樺台", "霧ヶ丘"));
    let text = "好きな麺は、うどんじゃなくてそばです。";
    let (new, old) = correction_candidate(text).expect("candidate");
    assert_eq!((&text[new], &text[old]), ("そば", "うどん"));
}
