//! 導出 (B4) の試験の材料: ターン・台帳・指示を組み立てる小さな世界と、決定的な擬似乱数 (`derive_properties`
//! と `bench` で使う)。台帳の文の範囲は、旧エンジンと同じ前のふるい (`chat`) と文の分け方 v1 で作る。値は作った
//! 地名・語だけ (11 §9.7)。

#![allow(
    dead_code,
    reason = "several test crates use this module, each a part of it"
)]

use std::collections::BTreeMap;
use std::ops::Range;

use evorag_memory::b3::{Dictionary, Env, PREFILTER_VERSION, Rules};
use evorag_memory::corrections::{CORRECTION_V1, find_holes};
use evorag_memory::derive::{
    self, Components, Derived, DirectiveStamp, Fingerprint, GateCache, Inputs, LedgerDigest,
    Speaker, TurnInput, Versions, dates,
};
use evorag_memory::{
    ByteRange, CALL_SEGMENT, CorrInput, Current, DirectiveBody, DirectiveSet, ForgetKey,
    LedgerBody, LedgerOutput, MODE_CHAT, SPLITTER_V1, Screened, Selected, Selection, SentenceSpan,
    Stage, StoredLedger, prefilter, split_v1,
};
use evorag_types::Id;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// 値の鍵の鍵 (試験の固定の値)。
pub fn key() -> ForgetKey {
    ForgetKey::from_bytes([7; 32])
}

/// 2026-04-01T00:00:00Z。
pub const T0: i64 = 1_775_001_600_000;

/// 台帳の変種と model_key (今の選択)。
pub const VARIANT: &str = "sr2";
pub const MODEL: &str = "m";

/// 時刻 `ms` の UUID v7 の形の ID (`n` で同じ時刻の中を分ける)。
pub fn id_at(ms: i64, n: u16) -> Id {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x70;
    b[7..9].copy_from_slice(&n.to_be_bytes());
    b[9] = 0x80;
    Id::from_bytes(b)
}

/// 同じ時刻の、`id` のすぐ後の ID (`id_at` の `n` の最上位のビットを立てる。`n` が 0x8000 未満のときだけ)。
pub fn just_after(id: Id) -> Id {
    let mut b = *id.as_bytes();
    assert!(b[7] < 0x80, "the sequence {:#x} leaves no room", b[7]);
    b[7] |= 0x80;
    Id::from_bytes(b)
}

/// 小さな擬似乱数 (xorshift64*)。
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // 0 を避け、近い種でも列が離れるように混ぜる。
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `0..n` の 1 つ (`n` は 1 以上)。
    pub fn below(&mut self, n: usize) -> usize {
        let n64 = u64::try_from(n).expect("fits");
        usize::try_from(self.next() % n64).expect("fits")
    }

    /// `num / den` の確率で真。
    pub fn chance(&mut self, num: usize, den: usize) -> bool {
        self.below(den) < num
    }

    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }

    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

/// ターン 1 つ。
#[derive(Debug, Clone)]
pub struct TurnData {
    pub session: Id,
    pub speaker: Speaker,
    pub text: String,
    pub tz: Option<i32>,
    pub long: Vec<Range<usize>>,
}

/// 台帳 1 つ (本体と、本体に入れた候補と判定の wire の形。順を乱すときに使う)。
#[derive(Debug, Clone)]
pub struct Ledger {
    pub id: Id,
    pub body: LedgerBody,
    pub units: Vec<Value>,
    pub verdicts: Vec<Value>,
}

impl Ledger {
    /// 候補と判定を本体に入れ直す (`units`・`verdicts` を変えた後)。
    pub fn rewrite(&mut self) {
        self.body
            .output
            .set_section(LedgerOutput::UNITS, &self.units)
            .expect("units");
        self.body
            .output
            .set_section(LedgerOutput::CORRECTIONS, &self.verdicts)
            .expect("corrections");
    }
}

/// 台帳に入れる候補 1 つ: 区切りの中の発話の番号 (`turn`)、その発話の中で根拠の文を指すバイト位置 (`at`)、wire
/// の形の要素 (`s` は台帳を作るときに埋める)。
#[derive(Debug, Clone)]
pub struct Cand {
    pub turn: usize,
    pub at: usize,
    pub unit: Value,
}

/// 候補の wire の形 (`s` は後で埋める)。
pub fn unit(aspect: &str, value: &str) -> Value {
    json!({"a": aspect, "v": value})
}

/// `v[k] = x` にした `v`。
pub fn with(mut v: Value, k: &str, x: &str) -> Value {
    v[k] = json!(x);
    v
}

/// 試験の世界: ターン・台帳・指示。ID は 1 つの時計で増える (ターンと指示は 1 本の列に並ぶ、04 §5.3)。
#[derive(Debug, Clone, Default)]
pub struct World {
    pub turns: BTreeMap<Id, TurnData>,
    pub ledgers: Vec<Ledger>,
    pub directives: Vec<(Id, DirectiveBody)>,
    pub clock: i64,
    pub seq: u16,
}

pub fn session_id(n: u16) -> Id {
    id_at(0, n + 1)
}

impl World {
    pub fn next_id(&mut self) -> Id {
        self.clock += 60_000;
        self.seq += 1;
        id_at(T0 + self.clock, self.seq)
    }

    /// セッション `session` の発話。`long` なら全体を長い一度の入力にする。
    pub fn say(&mut self, session: u16, text: &str, tz: Option<i32>, long: bool) -> Id {
        let id = self.next_id();
        let long = if long {
            std::iter::once(0..text.len()).collect()
        } else {
            Vec::new()
        };
        self.turns.insert(
            id,
            TurnData {
                session: session_id(session),
                speaker: Speaker::User,
                text: text.to_owned(),
                tz,
                long,
            },
        );
        id
    }

    /// セッション `session` の応答。
    pub fn answer(&mut self, session: u16, text: &str) -> Id {
        let id = self.next_id();
        self.turns.insert(
            id,
            TurnData {
                session: session_id(session),
                speaker: Speaker::Assistant,
                text: text.to_owned(),
                tz: Some(540),
                long: Vec::new(),
            },
        );
        id
    }

    /// 区切り 1 つの台帳 (往復の発話 `turns`、候補 `cands`)。文番号は区切りの中で 1 から振り、候補の `s` は根拠の
    /// 位置を含む文にする。訂正の候補の表 (`corr`) は発話の訂正の字句の型から作り、`verdict` で判定を決める
    /// (`None` なら判定を書かない)。
    pub fn ledger(
        &mut self,
        turns: &[Id],
        cands: &[Cand],
        mut verdict: impl FnMut(u32) -> Option<&'static str>,
    ) -> Id {
        let mut sentences = Vec::new();
        let mut corr = Vec::new();
        // 発話ごとの (文番号, 保存した本文の範囲)。
        let mut spans: Vec<Vec<(u32, Range<usize>)>> = Vec::new();
        let mut n = 1u32;
        for t in turns {
            let text = &self.turns[t].text;
            let mut mine = Vec::new();
            if let Some(h) = find_holes(MODE_CHAT, text) {
                corr.push(CorrInput {
                    id: u32::try_from(corr.len() + 1).expect("fits"),
                    turn: *t,
                    rev: 0,
                    pattern: h.pattern.to_owned(),
                    new: byte_range(&h.new),
                    old: h.old.as_ref().map(byte_range),
                    extra: Default::default(),
                });
            }
            if let Screened::Send(f) = prefilter(MODE_CHAT, text) {
                for r in split_v1(f.text()) {
                    let stored = f.to_stored(r).expect("one piece");
                    sentences.push(SentenceSpan::new(
                        n,
                        *t,
                        0,
                        u32::try_from(stored.start).expect("fits"),
                        u32::try_from(stored.end).expect("fits"),
                    ));
                    mine.push((n, stored));
                    n += 1;
                }
            }
            spans.push(mine);
        }
        let units: Vec<Value> = cands
            .iter()
            .map(|c| {
                // 根拠の位置を含む文 (無ければ番号の外の 0。関門 1 で落ちる)。
                let s = spans[c.turn]
                    .iter()
                    .find(|(_, r)| r.start <= c.at && c.at < r.end)
                    .map_or(0, |(s, _)| *s);
                let mut u = c.unit.clone();
                u["s"] = json!(s);
                u
            })
            .collect();
        let verdicts: Vec<Value> = corr
            .iter()
            .filter_map(|c| verdict(c.id).map(|v| json!({"id": c.id, "verdict": v})))
            .collect();
        let mut output = LedgerOutput::default();
        output
            .set_section(LedgerOutput::KEYS, &Vec::<String>::new())
            .expect("keys");
        let mut body = LedgerBody {
            call: CALL_SEGMENT.to_owned(),
            segment: Some(turns[0]),
            schema: 1,
            variant: VARIANT.to_owned(),
            model_key: MODEL.to_owned(),
            sentences,
            output,
            status: "complete".to_owned(),
            ..LedgerBody::default()
        };
        body.inputs.prefilter = PREFILTER_VERSION;
        body.inputs.splitter = SPLITTER_V1;
        body.inputs.correction = CORRECTION_V1;
        body.inputs.corr = corr;
        let id = self.next_id();
        let mut l = Ledger {
            id,
            body,
            units,
            verdicts,
        };
        l.rewrite();
        self.ledgers.push(l);
        id
    }

    pub fn directive(&mut self, b: DirectiveBody) -> Id {
        let id = self.next_id();
        self.directives.push((id, b));
        id
    }

    /// 導出に渡すターン (同じセッションの 1 つ前のターンを指す)。
    pub fn turn_inputs(&self) -> BTreeMap<Id, TurnInput<'_>> {
        let mut last: BTreeMap<Id, Id> = BTreeMap::new();
        self.turns
            .iter()
            .map(|(id, t)| {
                (
                    *id,
                    TurnInput {
                        session: t.session,
                        prev: last.insert(t.session, *id),
                        speaker: t.speaker,
                        rev: 0,
                        text: &t.text,
                        tz_offset: t.tz,
                        long_inputs: &t.long,
                    },
                )
            })
            .collect()
    }

    /// 全件の導出。台帳は `order` の順に渡し、指示は `directives` の順に渡す。
    pub fn derive_in(
        &self,
        rules: &Rules<'_>,
        cache: &mut GateCache,
        order: &[usize],
        directives: &[(Id, DirectiveBody)],
    ) -> Derived {
        let set = DirectiveSet::new(directives.iter().cloned(), "[伏せ字]", |s| {
            rules.resolve_entity(s)
        });
        let turns = self.turn_inputs();
        let selected: Vec<Selected<'_>> = order
            .iter()
            .map(|&i| {
                let l = &self.ledgers[i];
                Selected {
                    ledger: StoredLedger {
                        id: l.id,
                        body: &l.body,
                    },
                    stage: Stage::Current,
                }
            })
            .collect();
        let k = key();
        let inputs = Inputs {
            ledgers: &selected,
            turns: &turns,
            directives: &set,
            env: Env {
                dictionary: Dictionary::bundled(),
                rules,
                key: &k,
            },
            dates: dates::date_rules(),
        };
        derive::derive(&inputs, cache)
    }

    /// 書いた順のままの全件の導出。
    pub fn derive_with(&self, rules: &Rules<'_>, cache: &mut GateCache) -> Derived {
        let order: Vec<usize> = (0..self.ledgers.len()).collect();
        self.derive_in(rules, cache, &order, &self.directives)
    }

    /// 導出の指紋 (台帳の成分を `order` の順に渡す)。
    pub fn fingerprint_in(&self, rules: &Rules<'_>, order: &[usize]) -> Fingerprint {
        let digests: Vec<LedgerDigest> = order
            .iter()
            .map(|&i| {
                let l = &self.ledgers[i];
                LedgerDigest {
                    id: l.id,
                    sha256: Sha256::digest(serde_json::to_vec(&l.body).expect("serializes")).into(),
                }
            })
            .collect();
        let versions = Versions::new(Dictionary::bundled(), rules, dates::date_rules(), 1);
        derive::fingerprint(&Components {
            ledgers: &digests,
            directives: DirectiveStamp {
                count: u64::try_from(self.directives.len()).expect("fits"),
                max: self.directives.iter().map(|(id, _)| *id).max(),
            },
            versions: &versions,
            current: Current::Ready(Selection {
                variant: VARIANT,
                model_key: MODEL,
            }),
            previous: None,
        })
    }
}

pub fn byte_range(r: &Range<usize>) -> ByteRange {
    ByteRange {
        start: u32::try_from(r.start).expect("fits"),
        end: u32::try_from(r.end).expect("fits"),
    }
}

/// 導出の結果のバイト列 (単位の ID・本体の JSON・private の印、数えた理由)。覚えの当たり (`cache`) は入れない。
pub fn units_bytes(d: &Derived) -> Vec<u8> {
    let mut out = Vec::new();
    for u in &d.units {
        out.extend(u.id.as_bytes());
        out.extend(serde_json::to_vec(&u.body).expect("serializes"));
        out.extend(format!("{:?}", u.private_by).into_bytes());
        out.push(b'\n');
    }
    out
}

/// 数えた理由のバイト列 (名前の順の BTreeMap なので決まった形になる)。
pub fn counts_bytes(d: &Derived) -> Vec<u8> {
    format!("{:?}", d.counts).into_bytes()
}

/// 失敗のときに見せる単位の要約。
pub fn summary(d: &Derived) -> Vec<String> {
    d.units
        .iter()
        .map(|u| {
            let b = &u.body;
            format!(
                "{} {}/{}={} {:?} {:?} p={} anchors={}",
                if b.free { "free" } else { "dict" },
                b.entity,
                b.aspect,
                b.value,
                b.state,
                b.tense,
                b.private,
                b.anchors.len()
            )
        })
        .collect()
}
