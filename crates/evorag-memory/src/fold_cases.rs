//! 統合と忘れるの回帰ケース (`tests/memory-cases/consolidation.toml`・`forget.toml`、09 §3.5) を、純粋な核
//! (`fold`) と実際のストア (書き込みの入口・忘れるの計画と当て直し) の両方で走らせる。
//!
//! 書式は `tests/memory-cases/README.md`。どの事例も飛ばさない (M3.4 の忘れるで、保留にしていた C28・C29・C34・
//! C35 も通る)。
//!
//! 忘れるの事例は、さらに次を確かめる。
//! - 忘れるを当てた直後に当て直し (`replay_forgets`) をしても何も変わらない (2 回当てても 1 回と同じ)。
//! - 鏡のストアに、忘れるの指示と伏せ字にしたターンの行だけを取り込んで当て直すと、操作をしたストアと同じ
//!   事実とターンになる (ジャーナルからの当て直しと、操作の結果が一致する、04 §7.2)。
//!
//! verifies: AC-FR-304-3 (候補の順を入れ替えても同じ)、AC-FR-305-1 (単値・多値の置き換え)、AC-FR-307-4
//! (apply_forget の冪等)、AC-FR-307-5 (値の鍵が合わなければ中止)

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use evorag_types::Id;
use toml::Value;

use crate::fold::{
    Candidate, Choice, Directive, DirectiveKind, Directives, Event, FactSet, FactState, FoldEnv,
    Mood, Scope, Source, Tense, VerifiedCorrection, drop_sources, fold, refresh_private,
    table_order,
};
use crate::forget::{ForgetOptions, forget_in_core};
use crate::kinds::{KIND_TURN, TurnRole};
use crate::lexical::signals;
use crate::rules::attributes;
use crate::value_key::{BlockMark, ForgetKey, block_mark, normalize_v1, value_key};

fn cases(file: &str) -> Vec<Value> {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let path = dir.join("../../tests/memory-cases").join(file);
    let text = std::fs::read_to_string(&path).expect(file);
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

/// 実際のストアに同じ手順を当てる鏡 (書き込みの入口 `facts.rs` を通る)。
mod db {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    use evorag_store::key::OwnedPart;
    use evorag_store::testutil::TempDir;
    use evorag_store::{Envelope, OpenOptions, Store, WriteOp, WriteRequest, Writer, WriterHandle};
    use evorag_types::{Id, IdGen, ManualClock, Timestamp, UuidV7Gen};

    use evorag_store::{BodyPatch, Read};

    use crate::facts::{FactCtx, add_directive, consolidate, load_directives, load_facts, rebuild};
    use crate::fold::{DirectiveKind, Event, FactSet, FactState};
    use crate::forget::{
        ForgetAbort, ForgetEnv, ForgetOptions, ForgetPlan, Replay, plan_forget, replay_forgets,
    };
    use crate::gates::Safety;
    use crate::kinds::{
        KIND_SESSION, KIND_TURN, LOOKUP_UPDATED_AT, REL_IN_SESSION, REL_REPLY_TO, SessionBody,
        SessionState, TurnBody, TurnRole, TurnState,
    };
    use crate::mask::{Masker, TurnText};
    use crate::value_key::BlockMark;

    use super::id_at;

    /// 記録の時刻 (回帰ケースの時刻とは別。ID の順だけが意味を持つ)。
    const NOW: Timestamp = Timestamp::from_millis(1_762_650_000_000);
    use crate::rules::attributes;
    use crate::value_key::{ForgetKey, ValueKey};

    /// 回帰ケースの時刻に合わせて ID を作る (指示の時点とターンの順を保つため)。
    pub struct CaseIds(Mutex<(i64, u64)>);

    impl CaseIds {
        pub fn at(&self, ms: i64) {
            self.0.lock().expect("ids").0 = ms;
        }
    }

    impl IdGen for CaseIds {
        fn next_id(&self) -> Id {
            let mut g = self.0.lock().expect("ids");
            g.1 += 1;
            // 純粋な核の側の ID と重ならないように、通し番号を大きく離す。
            id_at(g.0, (1 << 40) + g.1)
        }
    }

    pub struct Db {
        _dir: TempDir,
        store: Arc<Store>,
        handle: WriterHandle,
        thread: Option<std::thread::JoinHandle<()>>,
        pub ids: CaseIds,
        safety: Safety<'static>,
        /// 純粋な核の側の指示の ID → ストアの側の指示の ID。
        pub directives: BTreeMap<Id, Id>,
        /// 書いたセッション。
        sessions: BTreeSet<Id>,
    }

    impl Db {
        pub fn new() -> Self {
            let dir = TempDir::new("fold-cases");
            let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
            let (store, _) = Store::open(OpenOptions {
                data_root: dir.path().to_path_buf(),
                evorag_version: "0.3.0".into(),
                kinds: crate::kinds(),
                clock: clock.clone(),
                ids: Arc::new(UuidV7Gen::with_seed(clock, 1)),
                config_file: None,
            })
            .expect("open");
            let store = Arc::new(store);
            let (writer, handle) = Writer::new(store.clone()).expect("writer");
            let thread = std::thread::spawn(move || writer.run());
            Self {
                _dir: dir,
                store,
                handle,
                thread: Some(thread),
                ids: CaseIds(Mutex::new((0, 0))),
                safety: Safety::bundled(),
                directives: BTreeMap::new(),
                sessions: BTreeSet::new(),
            }
        }

        fn submit(&self, ops: Vec<WriteOp>) {
            self.handle
                .submit_wait(WriteRequest::conversation(ops))
                .expect("write facts");
        }

        /// ターンを書く (書き込みの入口が、出典の改訂の番号を今のターンと突き合わせるため)。応答は `reply_to`
        /// (同じセッションの直前の発話) を持つ。セッションが無ければ作る (セッションの削除が、関係からターンを
        /// たどるため)。
        pub fn add_turn(
            &mut self,
            id: Id,
            session: Id,
            text: &str,
            role: TurnRole,
            reply_to: Option<Id>,
        ) {
            let mut ops = Vec::new();
            if self.sessions.insert(session) {
                let mut s = Envelope::new(session, KIND_SESSION, 1, NOW, "");
                s.set_body(&SessionBody {
                    title: "case".into(),
                    mode: "chat".into(),
                    state: SessionState::Active,
                    memory: None,
                })
                .expect("session body");
                ops.push(WriteOp::Put(s));
                ops.push(WriteOp::PutLookup {
                    kind: KIND_SESSION.into(),
                    field: LOOKUP_UPDATED_AT.into(),
                    value: OwnedPart::Int(NOW.as_millis()),
                    id: session,
                });
            }
            let mut e = Envelope::new(id, KIND_TURN, 1, NOW, "");
            e.set_body(&TurnBody {
                session,
                role,
                state: TurnState::Confirmed,
                text: text.to_owned(),
                model_key: None,
                generation: None,
                attached: Vec::new(),
                reply_to,
                error: None,
                recall: None,
                obs: None,
            })
            .expect("turn body");
            ops.push(WriteOp::Put(e));
            ops.push(WriteOp::PutLink {
                from: id,
                rel: REL_IN_SESSION.into(),
                to: session,
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
            self.submit(ops);
        }

        /// ターンの本文を、規則を通さずに書き換える (改訂の番号は変えない。中止の試験だけに使う)。
        pub fn tamper(&self, id: Id, text: &str) {
            self.submit(vec![WriteOp::Patch {
                kind: KIND_TURN.into(),
                id,
                fields: BodyPatch::new().set("text", &text).expect("patch"),
                updated_at: None,
            }]);
        }

        fn forget_env<'k>(&'k self, key: &'k ForgetKey) -> ForgetEnv<'k> {
            ForgetEnv {
                key,
                attributes: attributes(),
                marker: "[伏せ字]",
                hidden_marker: "[非表示]",
                ids: &self.ids,
                now: NOW,
            }
        }

        /// 指した値の行 (今の値か `Contested` の値) の参照。
        fn row(&self, target: (&str, &str, ValueKey)) -> evorag_types::api::FactKey {
            let set = self.facts();
            let fact = set
                .facts
                .iter()
                .find(|f| {
                    f.subject == target.0
                        && f.attribute == target.1
                        && f.vk == Some(target.2)
                        && matches!(f.state, FactState::Live | FactState::Contested { .. })
                })
                .expect("the target row");
            crate::view::row_key(fact).expect("a row")
        }

        /// 忘れる (計画を作って書く)。中止なら何も書かない。
        pub fn forget(
            &mut self,
            key: &ForgetKey,
            pure_id: Id,
            target: (&str, &str, ValueKey),
            options: ForgetOptions,
        ) -> Result<ForgetPlan, ForgetAbort> {
            let row = self.row(target);
            let plan = {
                let tx = self.store.read().expect("read");
                plan_forget(&tx, &self.forget_env(key), &row, options).expect("plan")
            }?;
            // 見本と実行で、同じ読み取りからは同じ計画になる (ハッシュが同じ)。
            let again = {
                let tx = self.store.read().expect("read");
                plan_forget(&tx, &self.forget_env(key), &row, options)
                    .expect("plan")
                    .expect("the same plan")
            };
            assert_eq!(again.hash, plan.hash, "the plan hash is stable");
            self.submit(plan.ops.clone());
            self.directives.insert(pure_id, plan.directive);
            Ok(plan)
        }

        /// 忘れるの当て直しを当てる。
        pub fn replay(&self, key: &ForgetKey) -> Replay {
            let r = {
                let tx = self.store.read().expect("read");
                replay_forgets(&tx, &self.forget_env(key)).expect("replay")
            };
            if r.changed() {
                self.submit(r.ops.clone());
            }
            r
        }

        /// `from` の記録 (忘れるの指示と伏せ字にしたターン) を、ジャーナルの取り込みのように丸ごと書く。
        pub fn import(&mut self, from: &Db, records: &[(&str, Id)], pure_id: Id, directive: Id) {
            let ops: Vec<WriteOp> = {
                let tx = from.store.read().expect("read");
                records
                    .iter()
                    .filter_map(|(kind, id)| match tx.get(kind, *id).expect("get") {
                        Some(Read::Ok(e)) => Some(WriteOp::Put(e)),
                        _ => None,
                    })
                    .collect()
            };
            // 取り込みは関係と副索引を本体から作り直す (`derive`)。
            let mut all = Vec::new();
            for op in ops {
                if let WriteOp::Put(e) = &op
                    && e.kind == crate::facts::KIND_FACT_DIRECTIVE
                {
                    all.extend(crate::facts::derive_directive(e));
                }
                all.push(op);
            }
            self.submit(all);
            self.directives.insert(pure_id, directive);
        }

        /// ターンの本文と改訂の番号。
        pub fn turn(&self, id: Id) -> (String, u32) {
            self.turn_opt(id)
                .unwrap_or_else(|| panic!("turn {id} is missing"))
        }

        /// ターンの本文と改訂の番号 (消えたターンは `None`)。
        pub fn turn_opt(&self, id: Id) -> Option<(String, u32)> {
            let tx = self.store.read().expect("read");
            match tx.get(KIND_TURN, id).expect("get") {
                Some(Read::Ok(e)) => Some((
                    e.body_as::<TurnBody>().expect("turn").text,
                    crate::extract::turn_rev(&e),
                )),
                None => None,
                other => panic!("turn {id}: {other:?}"),
            }
        }

        /// 想起で添えるときの本文 (往復 `exchange` の `turn` のもの)。
        pub fn recall_text(&self, exchange: &[Id], turn: Id) -> String {
            let texts: Vec<(Id, String, u32)> = exchange
                .iter()
                .map(|t| {
                    let (text, rev) = self.turn(*t);
                    (*t, text, rev)
                })
                .collect();
            let tx = self.store.read().expect("read");
            let m = Masker::load(&tx, "[伏せ字]").expect("masker");
            let view: Vec<TurnText<'_>> = texts
                .iter()
                .map(|(id, text, rev)| TurnText {
                    id: *id,
                    rev: *rev,
                    text,
                })
                .collect();
            let masked = m.mask(&view, "[非表示]");
            let i = exchange
                .iter()
                .position(|t| *t == turn)
                .expect("in exchange");
            masked[i].clone()
        }

        /// 効いている再び覚えない印 (印を解いた指示を除く)。
        pub fn active_marks(&self) -> BTreeSet<BlockMark> {
            let tx = self.store.read().expect("read");
            let dirs = load_directives(&tx).expect("directives");
            dirs.iter()
                .filter(|d| !dirs.is_unblocked(d.id))
                .filter_map(|d| match &d.kind {
                    DirectiveKind::Forget { marks } | DirectiveKind::Unknown { marks } => {
                        Some(marks.clone())
                    }
                    _ => None,
                })
                .flatten()
                .collect()
        }

        /// セッションを消す (書き込みの入口を通して、事実の出典も外す、04 §7.1)。
        pub fn delete_session(&self, session: Id) {
            let ops = {
                let tx = self.store.read().expect("read");
                let s = crate::read::load_session(&tx, session).expect("the session");
                let clock = ManualClock::new(NOW);
                let writes = crate::write::Writes {
                    clock: &clock,
                    ids: &self.ids,
                };
                crate::write::delete_session(&tx, &s.envelope, &writes).expect("delete session")
            };
            self.submit(ops);
        }

        fn ctx<'a>(&'a self, key: &'a ForgetKey) -> FactCtx<'a> {
            FactCtx {
                key,
                attributes: attributes(),
                safety: &self.safety,
                ids: &self.ids,
                now: NOW,
            }
        }

        pub fn consolidate(&self, key: &ForgetKey, events: Vec<Event>) {
            let ops = {
                let tx = self.store.read().expect("read");
                consolidate(&tx, &self.ctx(key), events)
                    .expect("consolidate")
                    .0
            };
            self.submit(ops);
        }

        pub fn rebuild(&self, key: &ForgetKey, events: Vec<Event>) {
            let ops = {
                let tx = self.store.read().expect("read");
                rebuild(&tx, &self.ctx(key), events).expect("rebuild").0
            };
            self.submit(ops);
        }

        pub fn add_directive(
            &mut self,
            key: &ForgetKey,
            pure_id: Id,
            kind: DirectiveKind,
            target: (&str, &str, Option<ValueKey>),
        ) {
            let kind = match kind {
                DirectiveKind::PrivateOff { target } => DirectiveKind::PrivateOff {
                    target: self.directives[&target],
                },
                DirectiveKind::Unblock { target } => DirectiveKind::Unblock {
                    target: self.directives[&target],
                },
                k => k,
            };
            let (id, ops) = {
                let tx = self.store.read().expect("read");
                add_directive(&tx, &self.ctx(key), kind, target.0, target.1, target.2)
                    .expect("add directive")
            };
            self.submit(ops);
            self.directives.insert(pure_id, id);
        }

        pub fn facts(&self) -> FactSet {
            let tx = self.store.read().expect("read");
            load_facts(&tx).expect("load").set
        }

        /// すべての事実の記録について、本体から作る関係と副索引の集合と、ストアに実際にある集合が
        /// 一致する (書き込みの差と、repair・ジャーナルの取り込みでの作り直しが同じ結果になる)。
        pub fn check_indexes(&self, id: &str) {
            use crate::facts::{IndexEntry, KIND_FACT, REL_FACT_SRC, fact_entries};
            use evorag_store::key::OwnedPart;
            use evorag_store::{LookupRange, Read};
            let tx = self.store.read().expect("read");
            let mut want: BTreeSet<(Id, IndexEntry)> = BTreeSet::new();
            let mut ids = Vec::new();
            tx.for_each(KIND_FACT, |r| {
                if let Read::Ok(e) = r {
                    ids.push(e.id);
                    want.extend(fact_entries(&e).into_iter().map(|x| (e.id, x)));
                }
                true
            })
            .expect("facts");
            let mut got: BTreeSet<(Id, IndexEntry)> = BTreeSet::new();
            for field in ["sa", "vk"] {
                let all = LookupRange::Between {
                    from: None,
                    to: None,
                };
                for entry in tx
                    .lookup(KIND_FACT, field, &all, false, None)
                    .expect("lookup")
                {
                    let OwnedPart::Str(value) = entry.value else {
                        panic!("{id}: a non-string lookup value");
                    };
                    let field = if field == "sa" { "sa" } else { "vk" };
                    got.insert((entry.id, IndexEntry::Lookup { field, value }));
                }
            }
            for fid in ids {
                for (to, _) in tx.links_from(fid, REL_FACT_SRC).expect("links") {
                    got.insert((
                        fid,
                        IndexEntry::Link {
                            rel: REL_FACT_SRC,
                            to,
                        },
                    ));
                }
            }
            assert_eq!(got, want, "{id}: the indexes must match the fact bodies");
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            self.handle.shutdown();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }
}

struct Runner {
    key: ForgetKey,
    seq: u64,
    last_ms: i64,
    turns: BTreeMap<String, (Id, String)>,
    /// ターンの ID → 往復 (利用者の発話と、それへの応答)。
    exchanges: BTreeMap<Id, Vec<Id>>,
    /// セッションの名前 → 最後の利用者の発話 (応答の `reply_to`)。
    last_user: BTreeMap<String, Id>,
    /// セッションの名前 → ID と、そのターン。
    sessions: BTreeMap<String, (Id, BTreeSet<Id>)>,
    set: FactSet,
    dirs: Directives,
    /// それまでに当てた候補と訂正 (作り直しと順の入れ替えで当て直す)。
    history: Vec<Event>,
    batch: Vec<Event>,
    db: db::Db,
    /// 忘れるの鏡: 忘れるの指示 (と、`true` なら伏せ字にしたターンの行) だけを取り込み、当て直しで同じ結果に
    /// なるかを確かめる。指示だけの鏡では、当て直しがターンを伏せ字にする (核の「当てた本文」の枝)。
    mirrors: Vec<(db::Db, bool)>,
    /// 直前の操作が中止された。
    aborted: bool,
}

impl Runner {
    fn new(mirror: bool) -> Self {
        Self {
            key: ForgetKey::from_bytes([1; 32]),
            seq: 0,
            last_ms: 0,
            turns: BTreeMap::new(),
            exchanges: BTreeMap::new(),
            last_user: BTreeMap::new(),
            sessions: BTreeMap::new(),
            set: FactSet::default(),
            dirs: Directives::default(),
            history: Vec::new(),
            batch: Vec::new(),
            db: db::Db::new(),
            mirrors: if mirror {
                vec![(db::Db::new(), true), (db::Db::new(), false)]
            } else {
                Vec::new()
            },
            aborted: false,
        }
    }

    /// 本体と鏡の両方のストアに当てる。
    fn both(&mut self, mut f: impl FnMut(&mut db::Db)) {
        f(&mut self.db);
        for (m, _) in &mut self.mirrors {
            f(m);
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
        let (ms, key) = (self.last_ms, self.key.clone());
        self.both(|d| {
            d.ids.at(ms);
            d.consolidate(&key, batch.clone());
        });
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
        let (ms, key) = (self.last_ms, self.key.clone());
        self.both(|d| {
            d.ids.at(ms);
            d.rebuild(&key, events.clone());
        });
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

    /// 効いている再び覚えない印 (純粋な核の側)。
    fn active_marks(&self) -> BTreeSet<BlockMark> {
        self.dirs
            .iter()
            .filter(|d| !self.dirs.is_unblocked(d.id))
            .filter_map(|d| match &d.kind {
                DirectiveKind::Forget { marks } => Some(marks.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// 忘れる (04 §7.1)。ストアの側で中止されたら、純粋な核の側も変えない。
    fn forget(&mut self, id: &str, did: Id, body: &Value) {
        let (subject, attribute, vk) = self.target(body);
        let vk = vk.expect("a value");
        let flag = |k: &str, d: bool| body.get(k).and_then(Value::as_bool).unwrap_or(d);
        let options = ForgetOptions {
            redact: flag("redact", true),
            history: flag("history", false),
            echo: flag("echo", true),
            others: flag("others", false),
        };
        self.db.ids.at(self.last_ms);
        let plan = match self
            .db
            .forget(&self.key, did, (&subject, &attribute, vk), options)
        {
            Ok(p) => p,
            Err(_) => {
                self.aborted = true;
                return;
            }
        };
        // 2 回目の当て直しは何も変えない (apply_forget の冪等、AC-FR-307-4)。
        let again = self.db.replay(&self.key);
        assert!(
            !again.changed(),
            "{id}: replaying right after the forget must change nothing: {again:?}"
        );
        // 鏡: 指示 (と伏せ字にしたターン) だけを取り込み、当て直す。
        for (m, with_turns) in &mut self.mirrors {
            let mut records: Vec<(&str, Id)> =
                vec![(crate::facts::KIND_FACT_DIRECTIVE, plan.directive)];
            if *with_turns {
                records.extend(plan.redacted_turns.iter().map(|t| (KIND_TURN, *t)));
            }
            m.ids.at(self.last_ms);
            m.import(&self.db, &records, did, plan.directive);
            let first = m.replay(&self.key);
            assert!(
                first.mismatched.is_empty(),
                "{id}: no mismatched turns: {first:?}"
            );
            let mut patched = first.patched.clone();
            patched.sort_unstable();
            let want = if *with_turns {
                Vec::new()
            } else {
                plan.redacted_turns.clone()
            };
            assert_eq!(patched, want, "{id}: turns redacted by the replay");
            let second = m.replay(&self.key);
            assert!(!second.changed(), "{id}: the mirror replay is idempotent");
        }
        let fact = self
            .set
            .facts
            .iter()
            .find(|f| {
                f.subject == subject
                    && f.attribute == attribute
                    && f.vk == Some(vk)
                    && matches!(f.state, FactState::Live | FactState::Contested { .. })
            })
            .unwrap_or_else(|| panic!("{id}: the target in the pure core"))
            .id;
        forget_in_core(
            &mut self.set,
            &mut self.dirs,
            &self.key,
            attributes(),
            fact,
            options.history,
            did,
        );
        // 伏せ字にしたターンの候補と訂正は、作り直しでは出典の改訂の番号が合わずに捨てられる (書き込みの入口)。
        let redacted: BTreeSet<Id> = plan.redacted_turns.iter().copied().collect();
        self.history.retain(|e| match e {
            Event::Candidate(c) => !redacted.contains(&c.source.turn),
            Event::Correction(c) => !redacted.contains(&c.source().turn),
            Event::Choose { .. } => true,
        });
    }

    fn step(&mut self, id: &str, step: &Value) {
        let (name, body) = step.as_table().expect("table").iter().next().expect("one");
        if name != "expect" {
            self.aborted = false;
        }
        match name.as_str() {
            "turn" => {
                self.flush();
                let ms = parse_ms(s(body, "at").expect("at"));
                self.last_ms = ms;
                let tid = self.next_id(ms);
                let text = s(body, "text").expect("text").to_owned();
                let name = s(body, "session").unwrap_or("default").to_owned();
                let sid = match self.sessions.get(&name) {
                    Some((sid, _)) => *sid,
                    None => self.next_id(ms),
                };
                self.sessions
                    .entry(name.clone())
                    .or_insert_with(|| (sid, BTreeSet::new()))
                    .1
                    .insert(tid);
                let (role, reply_to) = match s(body, "role").unwrap_or("user") {
                    "assistant" => (TurnRole::Assistant, self.last_user.get(&name).copied()),
                    _ => {
                        self.last_user.insert(name, tid);
                        (TurnRole::User, None)
                    }
                };
                match reply_to {
                    Some(u) => {
                        let ex = self.exchanges.entry(u).or_insert_with(|| vec![u]);
                        ex.push(tid);
                        let ex = ex.clone();
                        for t in &ex {
                            self.exchanges.insert(*t, ex.clone());
                        }
                    }
                    None => {
                        self.exchanges.insert(tid, vec![tid]);
                    }
                }
                self.both(|d| d.add_turn(tid, sid, &text, role, reply_to));
                self.turns
                    .insert(s(body, "id").expect("id").to_owned(), (tid, text));
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
                let old = s(body, "old").expect("old");
                let span = |v: &str| {
                    let start = text.find(v).expect("the value in the turn");
                    Source {
                        turn,
                        start: u32::try_from(start).expect("short"),
                        end: u32::try_from(start + v.len()).expect("short"),
                        rev: 0,
                    }
                };
                let c = VerifiedCorrection::assume_verified(
                    span(new),
                    Some(span(old)),
                    crate::correction::Target::Fact,
                    s(body, "old").expect("old").to_owned(),
                    value_key(&self.key, s(body, "old").expect("old")),
                    new.to_owned(),
                    value_key(&self.key, new),
                );
                self.batch.push(Event::Correction(c));
            }
            "tamper" => {
                self.flush();
                let (turn, _) = self.turns[s(body, "id").expect("id")].clone();
                let text = s(body, "text").expect("text").to_owned();
                self.both(|d| d.tamper(turn, &text));
            }
            "op" => {
                self.flush();
                let kind = s(body, "kind").expect("kind");
                let did = self.next_id(self.last_ms);
                match kind {
                    "forget" => self.forget(id, did, body),
                    "delete_session" => {
                        let name = s(body, "session").expect("session");
                        let (sid, gone) = self.sessions.remove(name).expect("a known session");
                        let ms = self.last_ms;
                        self.both(|d| {
                            d.ids.at(ms);
                            d.delete_session(sid);
                        });
                        let is_gone = |t: Id| gone.contains(&t);
                        let mut seq = self.seq;
                        let mut new_id = || {
                            seq += 1;
                            id_at(ms, seq)
                        };
                        drop_sources(
                            &mut self.set,
                            &mut self.dirs,
                            attributes(),
                            &is_gone,
                            &mut new_id,
                        );
                        self.seq = seq;
                        // 消したターンの候補と訂正は、作り直しでも当て直さない (書き込みの入口が、ターンの
                        // 無い出典を捨てるのと同じ)。
                        self.history.retain(|e| match e {
                            Event::Candidate(c) => !is_gone(c.source.turn),
                            Event::Correction(c) => !is_gone(c.source().turn),
                            Event::Choose { .. } => true,
                        });
                    }
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
                        let (ms, key) = (self.last_ms, self.key.clone());
                        self.both(|d| {
                            d.ids.at(ms);
                            d.add_directive(&key, did, dk.clone(), (&subject, &attribute, vk));
                        });
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
                                choice: if kind == "revert" {
                                    Choice::Revert
                                } else {
                                    Choice::Accept
                                },
                            };
                            let mut set = std::mem::take(&mut self.set);
                            self.fold_events(&mut set, vec![ev]);
                            self.set = set;
                        }
                    }
                    "private_off" | "unblock" => {
                        let target = if kind == "unblock" {
                            self.find_directive(|k| matches!(k, DirectiveKind::Forget { .. }), body)
                        } else {
                            self.find_directive(
                                |k| matches!(k, DirectiveKind::Private { .. }),
                                body,
                            )
                        };
                        let dk = if kind == "unblock" {
                            DirectiveKind::Unblock { target }
                        } else {
                            DirectiveKind::PrivateOff { target }
                        };
                        let (subject, attribute, vk) = self.target(body);
                        let (ms, key) = (self.last_ms, self.key.clone());
                        self.both(|d| {
                            d.ids.at(ms);
                            d.add_directive(&key, did, dk.clone(), (&subject, &attribute, vk));
                        });
                        self.dirs.push(Directive {
                            id: did,
                            kind: dk,
                            subject,
                            attribute,
                            vk,
                        });
                        refresh_private(&mut self.set, &self.dirs);
                    }
                    other => panic!("{id}: unknown op `{other}`"),
                }
            }
            "expect" => {
                self.flush();
                self.check(id, body);
            }
            other => panic!("{id}: unknown step `{other}`"),
        }
    }

    fn turn_name(&self, tid: Id) -> String {
        self.turns
            .iter()
            .find(|(_, (t, _))| *t == tid)
            .map(|(k, _)| k.clone())
            .expect("a known turn")
    }

    /// 事実を、期待と同じ形の文字列の並び (整列済み) にする。重複も見えるように集合にしない。`spans` なら出典の
    /// 範囲と改訂の番号も書く (鏡との比べ)。
    fn describe_with(&self, set: &FactSet, spans: bool) -> Vec<String> {
        let value_of = |fid: Id| match set.facts.iter().find(|f| f.id == fid) {
            Some(f) if f.state == FactState::Retracted => "(retracted)".to_owned(),
            Some(f) => f.value.clone().unwrap_or_default(),
            None => "(deleted)".to_owned(),
        };
        let mut out: Vec<String> = set
            .facts
            .iter()
            .map(|f| {
                let (state, extra) = match f.state {
                    FactState::Live => ("Live", String::new()),
                    FactState::Contested { against } => {
                        ("Contested", format!(" against={}", value_of(against)))
                    }
                    FactState::Superseded { by, .. } => (
                        "Superseded",
                        by.map(|b| format!(" by={}", value_of(b)))
                            .unwrap_or_default(),
                    ),
                    FactState::Retracted => ("Retracted", String::new()),
                };
                let tense = match f.tense {
                    Tense::Current => "current",
                    Tense::Past => "past",
                    Tense::Planned => "planned",
                };
                let sources: BTreeSet<String> = f
                    .sources
                    .iter()
                    .map(|s| {
                        let name = self.turn_name(s.turn);
                        if spans {
                            format!("{name}@{}:{}-{}", s.rev, s.start, s.end)
                        } else {
                            name
                        }
                    })
                    .collect();
                format!(
                    "{}/{}/{} {state}{extra} tn={tense} private={} sources={sources:?}",
                    f.subject,
                    f.attribute,
                    f.value.clone().unwrap_or_default(),
                    f.private
                )
            })
            .collect();
        out.sort();
        out
    }

    fn describe(&self, set: &FactSet) -> Vec<String> {
        self.describe_with(set, false)
    }

    /// 束の分け方に依らないこと: それまでの出来事を位置の順に並べ、1 件ずつ別の束で当てても、
    /// 1 つの束で当て直したのと同じになる (状態が記録に出し切れているか)。
    fn check_batch_independence(&mut self, id: &str) {
        let mut events = self.history.clone();
        events.extend(self.dirs.choices());
        events.sort_by_cached_key(Event::pos);
        let mut whole = FactSet::default();
        self.fold_events(&mut whole, events.clone());
        let mut one_by_one = FactSet::default();
        for e in events {
            self.fold_events(&mut one_by_one, vec![e]);
        }
        assert_eq!(
            self.describe(&one_by_one),
            self.describe(&whole),
            "{id}: folding one event at a time must equal one batch"
        );
    }

    fn marks_of(&self, list: &[Value]) -> BTreeSet<BlockMark> {
        list.iter()
            .map(|m| {
                let vk = value_key(&self.key, s(m, "v").expect("v"));
                block_mark(
                    &self.key,
                    &normalize_v1(s(m, "s").expect("s")),
                    s(m, "a").expect("a"),
                    &vk,
                )
            })
            .collect()
    }

    fn check(&self, id: &str, body: &Value) {
        // 実際のストアを通した結果も、純粋な核と同じ (書き込みの入口の差と写しの検査)。
        self.db.check_indexes(id);
        let stored = self.db.facts();
        assert_eq!(
            self.describe(&stored),
            self.describe(&self.set),
            "{id}: the store must hold what the pure core holds"
        );
        // 鏡 (指示と伏せ字の行だけを取り込んで当て直したストア) も、事実 (出典の範囲と番号まで) とターンが同じ。
        for (m, _) in &self.mirrors {
            m.check_indexes(id);
            assert_eq!(
                self.describe_with(&m.facts(), true),
                self.describe_with(&stored, true),
                "{id}: replaying the directive must give the same facts"
            );
            for (tid, _) in self.turns.values() {
                assert_eq!(m.turn_opt(*tid), self.db.turn_opt(*tid), "{id}: turn {tid}");
            }
        }
        let names = |set: &FactSet| -> Vec<String> {
            table_order(set, attributes())
                .into_iter()
                .map(|f| format!("{}/{}", f.subject, f.value.clone().unwrap_or_default()))
                .collect()
        };
        assert_eq!(
            names(&stored),
            names(&self.set),
            "{id}: table order in the store"
        );
        if let Some(a) = body.get("aborted").and_then(Value::as_bool) {
            assert_eq!(self.aborted, a, "{id}: aborted");
        }
        if let Some(facts) = body.get("facts").and_then(Value::as_array) {
            // 実際の事実を、期待と同じ形の文字列にして、並び (重複を含む) として比べる。
            let actual = self.describe(&self.set);
            let mut expected: Vec<String> = facts
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
            expected.sort();
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
        if let Some(turns) = body.get("turns").and_then(Value::as_array) {
            for t in turns {
                let (tid, _) = &self.turns[s(t, "id").expect("id")];
                assert_eq!(
                    self.db.turn(*tid).0,
                    s(t, "text").expect("text"),
                    "{id}: the text of turn {}",
                    s(t, "id").unwrap_or_default()
                );
            }
        }
        if let Some(recall) = body.get("recall").and_then(Value::as_array) {
            for t in recall {
                let (tid, _) = &self.turns[s(t, "turn").expect("turn")];
                let exchange = &self.exchanges[tid];
                assert_eq!(
                    self.db.recall_text(exchange, *tid),
                    s(t, "text").expect("text"),
                    "{id}: the recalled text of turn {}",
                    s(t, "turn").unwrap_or_default()
                );
            }
        }
        if let Some(blocked) = body.get("blocked").and_then(Value::as_array) {
            let want = self.marks_of(blocked);
            assert_eq!(self.active_marks(), want, "{id}: blocked (pure core)");
            assert_eq!(self.db.active_marks(), want, "{id}: blocked (store)");
        }
    }
}

/// 事例を走らせる。通った数を返す。
fn run_cases(file: &str, mirror: bool) -> usize {
    let mut passed = 0;
    for case in cases(file) {
        let id = s(&case, "id").expect("id").to_owned();
        let mut r = Runner::new(mirror);
        for step in case["step"].as_array().expect("steps") {
            r.step(&id, step);
        }
        r.check_batch_independence(&id);
        passed += 1;
    }
    passed
}

#[test]
fn consolidation_cases_hold_on_the_pure_core() {
    let passed = run_cases("consolidation.toml", false);
    assert!(passed >= 33, "passed {passed}");
}

/// verifies: 04 §7.1〜§7.3・§7.5 (忘れるの回帰ケース F01〜F15)、AC-FR-307-4、AC-FR-307-5
#[test]
fn forget_cases_hold_in_the_store_and_replay_the_same() {
    let passed = run_cases("forget.toml", true);
    assert_eq!(passed, 15, "passed {passed}");
}
