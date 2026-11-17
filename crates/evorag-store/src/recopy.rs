//! 忘れるの退避の写し直し (03 §7.3 の `forget_backups` の job)。
//!
//! 退避の論理的な写しの途中で記録を変える: 伏せ字にするターンには、そのターンのすべての忘れるの指示を指示の ID
//! の順に純粋な核 ([`crate::forget::apply_to_turn`]) で当て (合わなければそのターンを写しから除く)、封筒の
//! チェックサムを計算し直す。除く ID (`remove` と呼び手の判定で除く記録) の記録と、その関係・副索引・`blobs`・
//! `quarantine` の記録を写さない。合わないターンは記録と `blobs`・`quarantine` だけを除き、そのターンを指す関係と
//! 副索引は残す (ID だけで本文を持たず、restore はジャーナルからターンを戻すため)。読めない記録は、対象の ID を
//! 文字列で含むものを除く。
//! 値の鍵を使う判定 (印に当たる事実・`vks` に当たる訂正) と、出典の改訂の番号の判定は、呼び手が
//! [`ForgetRecopy::drop`] で渡す。写し終えたら元の退避の DB を閉じてから入れ替える。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Bound;
use std::path::Path;

use evorag_types::Id;
use redb::{Database, ReadableDatabase, ReadableTable};

use crate::backup::{self, Row, Table};
use crate::envelope::Envelope;
use crate::forget::{self, Redaction, TurnOutcome};
use crate::fsutil;
use crate::key::{self, KeyBuf, OwnedPart};
use crate::restore::{list_backups, mentions};
use crate::store::{DB_FILE, KIND_QUARANTINE, LOOKUP, RECORDS, Store, StoreError, db};

/// 副索引の 1 つの値 (種類, 項目, 値) の前方一致。[`ForgetRecopy::drop`] が除きうる記録の在りかを示す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupProbe {
    pub kind: String,
    pub field: String,
    pub value: OwnedPart,
}

impl LookupProbe {
    fn prefix(&self) -> Vec<u8> {
        let mut k = KeyBuf::new();
        k.str(&self.kind).str(&self.field);
        match &self.value {
            OwnedPart::Str(s) => k.str(s),
            OwnedPart::Bytes(b) => k.bytes(b),
            OwnedPart::Int(i) => k.int(*i),
            OwnedPart::Id(id) => k.id(*id),
        };
        k.into_bytes()
    }
}

/// 写しの中の、伏せ字にするターンの結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnInCopy {
    /// 写しに残る (伏せ字にしたか、すでに当たっていた)。`rev` は写しの中の改訂の番号。
    Kept { rev: u32 },
    /// 写しから除く (合わない・先の指示が無い・読めない)。
    Dropped,
}

/// [`ForgetRecopy::drop`] に渡す、1 つの退避の写しの様子。
#[derive(Debug)]
pub struct RecopyView<'a> {
    turns: &'a BTreeMap<Id, TurnInCopy>,
    dropped: &'a BTreeSet<Id>,
}

impl RecopyView<'_> {
    /// 伏せ字にするターン `id` の、この写しでの結果。写しに無いか、伏せ字にするターンでなければ `None`。
    #[must_use]
    pub fn turn(&self, id: Id) -> Option<TurnInCopy> {
        self.turns.get(&id).copied()
    }

    /// 記録 `id` を、この写しから除くと決めたか (除く ID と、前の回の判定で除いた記録)。判定は、除く記録が
    /// 増えなくなるまで繰り返すので、派生の記録の派生の記録 (チャンクの埋め込みなど) も、元の記録に続けて除ける。
    #[must_use]
    pub fn is_dropped(&self, id: Id) -> bool {
        self.dropped.contains(&id)
    }
}

/// 呼び手の判定を繰り返す上限 (派生の記録の段の数より多ければよい)。
const DROP_ROUNDS: usize = 4;

/// 退避の写し直しの対象 (1 つ以上の `forget_backups` の job をまとめたもの)。
pub struct ForgetRecopy<'a> {
    /// 写しから除く記録の ID (job の `ids`)。
    pub remove: BTreeSet<Id>,
    /// 伏せ字にするターンと、生きている DB から読んだそのターンのすべての忘れるの指示の伏せ字 (指示の ID の順)。
    pub turns: BTreeMap<Id, Vec<Redaction>>,
    /// 伏せ字にするはずだが、生きている DB の指示の記録が形として正しくないターン。写しから記録 (と `blobs`・
    /// `quarantine`) だけを除き、関係と副索引は残す (合わないターンと同じ)。[`RecopyView::turn`] は
    /// [`TurnInCopy::Dropped`] を返すので、その派生の記録も古いものとして除ける。
    pub drop_turns: BTreeSet<Id>,
    /// 伏せ字の印 (プロンプトの言語で選ぶ、04 §7.3)。
    pub marker: String,
    /// [`Self::drop`] が真を返しうる記録は、どれかの値の副索引を持つこと。退避を飛ばすかの判定に使う
    /// (例: 忘れた値の鍵の `vk` の副索引)。
    pub candidates: Vec<LookupProbe>,
    /// 副索引で引けない記録 (訂正・ノート) を [`Self::drop`] が除きうるとき: 記録の ID の最大がこの ID 以上の
    /// 退避は飛ばさない (対象を含まないことを確かめられないため。指示の後に作った記録がありうる)。
    pub scan_from: Option<Id>,
    /// 写しの記録 (読めるもの。除く ID・伏せ字にするターン・`quarantine` を除く) ごとに呼ぶ。真なら除く。
    /// 印に当たる事実、`vks` に当たる訂正、伏せ字にしたターンを出典とし出典の改訂の番号が写しのターンと合わない
    /// 派生の記録などを、ここで判定する (03 §7.3)。
    pub drop: &'a dyn Fn(&Envelope, &RecopyView<'_>) -> bool,
}

impl fmt::Debug for ForgetRecopy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgetRecopy")
            .field("remove", &self.remove)
            .field("turns", &self.turns)
            .field("marker", &self.marker)
            .field("candidates", &self.candidates)
            .field("drop_turns", &self.drop_turns)
            .field("scan_from", &self.scan_from)
            .finish_non_exhaustive()
    }
}

impl ForgetRecopy<'_> {
    fn id_targets(&self) -> BTreeSet<Id> {
        self.remove
            .iter()
            .chain(self.turns.keys())
            .chain(&self.drop_turns)
            .copied()
            .collect()
    }
}

/// 写し直した退避と、対象を含まないと確かめて飛ばした退避の数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecopyCount {
    pub rewritten: usize,
    pub skipped: usize,
}

impl Store {
    /// 忘れるの対象を、保持しているすべての退避から除いて写し直す (03 §7.3 の `forget_backups`)。
    ///
    /// 退避を作る処理と退避の整理 ([`Self::purge_backups`]) と同じ順序で通し、作りかけの退避を先に片付ける。
    /// `stop` が真を返したら、次の退避に移る前に止めて失敗を返す。続けてした操作をまとめるのは呼び手が行う
    /// (5 秒の分の job を 1 つの [`ForgetRecopy`] にまとめる)。
    ///
    /// # Errors
    /// 写し直せない退避、または消せない作りかけの退避があるとき (ほかの退避は写し終えてから返す。呼び手は job を
    /// 残して再試行する)。
    pub fn forget_backups(
        &self,
        backups: Option<&Path>,
        plan: &ForgetRecopy<'_>,
        stop: &dyn Fn() -> bool,
    ) -> Result<RecopyCount, StoreError> {
        let _order = self.backup_order();
        let leftovers = crate::restore::sweep_leftovers(self.data_root(), backups);
        let count = forget_backups_until(self.data_root(), backups, plan, stop)?;
        if !leftovers.is_empty() {
            return Err(StoreError::Io(format!(
                "partial backups could not be removed: {}",
                leftovers.join("; ")
            )));
        }
        Ok(count)
    }
}

/// [`Store::forget_backups`] の本体 (順序の鍵を取らない)。データ根を開いていないときの道具と試験で使う。
///
/// # Errors
/// 写し直せなかった退避があるとき、または止めたとき。
pub fn forget_backups_until(
    root: &Path,
    backups: Option<&Path>,
    plan: &ForgetRecopy<'_>,
    stop: &dyn Fn() -> bool,
) -> Result<RecopyCount, StoreError> {
    let mut count = RecopyCount::default();
    if plan.remove.is_empty()
        && plan.turns.is_empty()
        && plan.drop_turns.is_empty()
        && plan.candidates.is_empty()
        && plan.scan_from.is_none()
    {
        return Ok(count);
    }
    let mut failed = Vec::new();
    for b in list_backups(root, backups) {
        if stop() {
            return Err(StoreError::Io(
                "the backup rewrite was stopped before it finished".into(),
            ));
        }
        let file = b.dir.join(DB_FILE);
        match recopy_one(&file, plan) {
            Ok(true) => count.rewritten += 1,
            Ok(false) => count.skipped += 1,
            Err(e) => failed.push(format!("{}: {e}", file.display())),
        }
    }
    if !failed.is_empty() {
        return Err(StoreError::Io(format!(
            "backups could not be rewritten: {}",
            failed.join("; ")
        )));
    }
    Ok(count)
}

/// 1 つの退避を写し直す。写し直したら `true`、対象を含まない (変えるものが無い) と確かめたら `false`。
fn recopy_one(file: &Path, plan: &ForgetRecopy<'_>) -> Result<bool, StoreError> {
    let src = Database::open(file).map_err(db)?;
    if !may_hold_targets(&src, plan)? {
        return Ok(false);
    }
    let changes = Changes::scan(&src, plan)?;
    if changes.is_empty() {
        return Ok(false);
    }
    let tmp = file.with_file_name(format!("{DB_FILE}.purging"));
    backup::logical_copy_rows(&src, &tmp, &mut |table, k, _| Ok(changes.row(table, k)))?;
    // 元の退避の DB を閉じてから入れ替える (Windows では開いたままのファイルを置き換えられない)。
    drop(src);
    fsutil::rename_durable(&tmp, file).map_err(|e| StoreError::Io(e.to_string()))?;
    Ok(true)
}

/// 対象を含みうるか。含まないことを確かめられたときだけ `false` (03 §7.3 の「飛ばす退避」)。
fn may_hold_targets(src: &Database, plan: &ForgetRecopy<'_>) -> Result<bool, StoreError> {
    let targets = plan.id_targets();
    let rtx = src.begin_read().map_err(db)?;
    if let Some(from) = plan.scan_from {
        let after = match max_record_id(&rtx)? {
            MaxId::Known(max) => max >= from,
            MaxId::Empty => false,
            MaxId::Unknown => true,
        };
        if after {
            return Ok(true);
        }
    }
    if let Some(min) = targets.first() {
        // 退避の記録の ID の最大より、対象の ID がすべて大きければ、どの対象も (その隔離・派生の記録も) 含まない。
        // 隔離・派生の記録の ID は元の記録より後に作るので、元の記録の ID が最大より大きければ、それらも無い。
        // 最大は退避の記録の表から求める (`meta` の値は信じない)。
        let below_max = match max_record_id(&rtx)? {
            MaxId::Known(max) => *min <= max,
            MaxId::Empty => false,
            MaxId::Unknown => true,
        };
        if below_max && holds_any(&rtx, &targets)? {
            return Ok(true);
        }
    }
    let lookup = rtx.open_table(LOOKUP).map_err(db)?;
    for probe in &plan.candidates {
        let start = probe.prefix();
        let end = key::prefix_end(&start);
        let end = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
        let mut range = lookup
            .range::<&[u8]>((Bound::Included(start.as_slice()), end))
            .map_err(db)?;
        if range.next().transpose().map_err(db)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 退避の記録の ID の最大。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaxId {
    /// 記録が無い。
    Empty,
    Known(Id),
    /// 種類と ID の形でないキーがあり、求められない。
    Unknown,
}

/// 記録の表の ID の最大を、種類ごとの最後のキーから求める (種類の前方一致の終わりへ飛びながら、種類の数 × log n)。
fn max_record_id(rtx: &redb::ReadTransaction) -> Result<MaxId, StoreError> {
    let t = rtx.open_table(RECORDS).map_err(db)?;
    let mut from: Vec<u8> = Vec::new();
    let mut max: Option<Id> = None;
    loop {
        let Some(first) = t
            .range::<&[u8]>((Bound::Included(from.as_slice()), Bound::Unbounded))
            .map_err(db)?
            .next()
        else {
            break;
        };
        let (k, _) = first.map_err(db)?;
        let Some((kind, _)) = crate::store::split_record_key(k.value()) else {
            return Ok(MaxId::Unknown);
        };
        let mut prefix = KeyBuf::new();
        prefix.str(&kind);
        let prefix = prefix.into_bytes();
        let end = key::prefix_end(&prefix);
        let upper = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
        let last = t
            .range::<&[u8]>((Bound::Included(prefix.as_slice()), upper))
            .map_err(db)?
            .next_back();
        let Some(last) = last else {
            return Ok(MaxId::Unknown);
        };
        let (k, _) = last.map_err(db)?;
        let Some((_, id)) = crate::store::split_record_key(k.value()) else {
            return Ok(MaxId::Unknown);
        };
        max = max.max(Some(id));
        match end {
            Some(e) => from = e,
            None => break,
        }
    }
    Ok(max.map_or(MaxId::Empty, MaxId::Known))
}

/// 記録の表のキー、`quarantine` の記録の元の ID、チェックサムの合わない記録の値 (ID の文字列) に、`targets` の
/// どれかがあるか。
fn holds_any(rtx: &redb::ReadTransaction, targets: &BTreeSet<Id>) -> Result<bool, StoreError> {
    let needles = id_strings(targets.iter());
    let t = rtx.open_table(RECORDS).map_err(db)?;
    for item in t.iter().map_err(db)? {
        let (k, v) = item.map_err(db)?;
        let (k, v) = (k.value(), v.value());
        let Some((kind, id)) = crate::store::split_record_key(k) else {
            continue;
        };
        if targets.contains(&id)
            || (kind == KIND_QUARANTINE
                && quarantined_id(k, v).is_some_and(|o| targets.contains(&o)))
            || (!Envelope::is_intact(k, v) && contains_any(v, &needles))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// ID の文字列の形 (封筒と本体が ID を書く形)。
fn id_strings<'a>(ids: impl Iterator<Item = &'a Id>) -> Vec<Vec<u8>> {
    ids.map(|id| id.to_string().into_bytes()).collect()
}

fn contains_any(haystack: &[u8], needles: &[Vec<u8>]) -> bool {
    needles
        .iter()
        .any(|n| !n.is_empty() && haystack.windows(n.len()).any(|w| w == n.as_slice()))
}

/// `quarantine` の記録が持つ、元の記録の ID (本体の `id`)。
fn quarantined_id(k: &[u8], v: &[u8]) -> Option<Id> {
    let env = Envelope::decode(k, v).ok()?;
    let ciborium::Value::Map(map) = &env.body else {
        return None;
    };
    map.iter()
        .find(|(k, _)| k.as_text() == Some("id"))
        .and_then(|(_, v)| v.as_text())
        .and_then(|s| s.parse().ok())
}

/// 1 つの退避の写しで変えるもの。
#[derive(Debug, Default)]
struct Changes {
    /// 記録と、その関係・副索引・`blobs` を写さない ID (`remove` と、呼び手の判定で除く記録)。
    drop: BTreeSet<Id>,
    drop_bytes: BTreeSet<[u8; 16]>,
    /// 記録と `blobs` だけを写さない ID (合わないターン、読めずに対象の ID を含む記録、隔離の記録)。関係と副索引は
    /// ID だけなので残す (restore はジャーナルからターンを戻す)。
    drop_record: BTreeSet<Id>,
    /// 伏せ字にしたターンの、書き直した値 (チェックサムを計算し直したもの)。
    rewritten: BTreeMap<Id, Vec<u8>>,
}

impl Changes {
    fn scan(src: &Database, plan: &ForgetRecopy<'_>) -> Result<Self, StoreError> {
        let rtx = src.begin_read().map_err(db)?;
        let records = rtx.open_table(RECORDS).map_err(db)?;
        let needles = id_strings(plan.id_targets().iter());
        let mut out = Self::default();
        let mut turns: BTreeMap<Id, TurnInCopy> = BTreeMap::new();
        let mut quarantined: Vec<(Id, Id)> = Vec::new();
        // 1 回目: 除く ID、伏せ字にするターン、隔離の記録、読めない記録。
        for item in records.iter().map_err(db)? {
            let (k, v) = item.map_err(db)?;
            let (k, v) = (k.value(), v.value());
            let Some((kind, id)) = crate::store::split_record_key(k) else {
                continue;
            };
            if plan.remove.contains(&id) {
                out.drop.insert(id);
            } else if plan.drop_turns.contains(&id) {
                out.drop_record.insert(id);
                turns.insert(id, TurnInCopy::Dropped);
            } else if let Some(steps) = plan.turns.get(&id) {
                let state = match Envelope::decode(k, v) {
                    Ok(env) if env.id == id => out.redact(k, env, steps, &plan.marker),
                    _ => TurnInCopy::Dropped,
                };
                if state == TurnInCopy::Dropped {
                    out.drop_record.insert(id);
                }
                turns.insert(id, state);
            } else if Envelope::decode(k, v).is_err() {
                // 読めない記録は判定にかけられないので、対象の ID を文字列で含むものを除く (安全の側)。
                if contains_any(v, &needles) {
                    out.drop_record.insert(id);
                }
            } else if kind == KIND_QUARANTINE
                && let Some(orig) = quarantined_id(k, v)
            {
                quarantined.push((id, orig));
            }
        }
        // 2 回目から: 呼び手の判定 (写しのターンの結果と、前の回までに除いた記録を見て)。除く記録が増えなく
        // なるまで繰り返す。
        for _ in 0..DROP_ROUNDS {
            let mut more = Vec::new();
            let view = RecopyView {
                turns: &turns,
                dropped: &out.drop,
            };
            for item in records.iter().map_err(db)? {
                let (k, v) = item.map_err(db)?;
                let (k, v) = (k.value(), v.value());
                let Some((kind, id)) = crate::store::split_record_key(k) else {
                    continue;
                };
                if kind == KIND_QUARANTINE
                    || out.drop.contains(&id)
                    || out.drop_record.contains(&id)
                    || turns.contains_key(&id)
                {
                    continue;
                }
                if let Ok(env) = Envelope::decode(k, v)
                    && (plan.drop)(&env, &view)
                {
                    more.push(id);
                }
            }
            if more.is_empty() {
                break;
            }
            out.drop.extend(more);
        }
        // 除く記録・除く ID・伏せ字にするターンの隔離の記録 (隔離した原文に伏せる前の本文が残るため、04 §7.2)。
        // 写しに元の記録が無くても、計画の ID なら除く。
        for (qid, orig) in quarantined {
            if out.drop.contains(&orig)
                || out.drop_record.contains(&orig)
                || plan.remove.contains(&orig)
                || plan.turns.contains_key(&orig)
                || plan.drop_turns.contains(&orig)
            {
                out.drop_record.insert(qid);
            }
        }
        out.drop_bytes = out.drop.iter().map(|i| *i.as_bytes()).collect();
        Ok(out)
    }

    /// ターン `env` に伏せ字を当て、書き直した値を覚える。
    fn redact(
        &mut self,
        k: &[u8],
        mut env: Envelope,
        steps: &[Redaction],
        marker: &str,
    ) -> TurnInCopy {
        match forget::apply_to_turn(&env, steps, marker) {
            TurnOutcome::Unchanged => env
                .body_uint(forget::TURN_REV)
                .and_then(|r| u32::try_from(r).ok())
                .map_or(TurnInCopy::Dropped, |rev| TurnInCopy::Kept { rev }),
            TurnOutcome::Changed { text, rev } => {
                let patched = forget::turn_patch(&text, rev)
                    .ok()
                    .and_then(|p| env.apply_patch(&p).ok());
                if patched.is_none() {
                    return TurnInCopy::Dropped;
                }
                self.rewritten.insert(env.id, env.encode(k));
                TurnInCopy::Kept { rev }
            }
            TurnOutcome::NeedsEarlier | TurnOutcome::Mismatch => TurnInCopy::Dropped,
        }
    }

    fn is_empty(&self) -> bool {
        self.drop.is_empty() && self.drop_record.is_empty() && self.rewritten.is_empty()
    }

    fn row(&self, table: Table, k: &[u8]) -> Row {
        let dropped = |id: &Id| self.drop.contains(id);
        let record_gone = |id: &Id| dropped(id) || self.drop_record.contains(id);
        let parts = || key::decode(k).ok();
        let gone = match table {
            Table::Meta => false,
            Table::Records => match crate::store::split_record_key(k) {
                Some((_, id)) if record_gone(&id) => true,
                Some((_, id)) => {
                    if let Some(bytes) = self.rewritten.get(&id) {
                        return Row::Replace(bytes.clone());
                    }
                    false
                }
                None => false,
            },
            // 関係は (from, rel, to) と逆向きの行の両方に ID を持つ。除くのは `drop` の ID の関係だけ。
            Table::Links => match parts() {
                Some(p) => p
                    .iter()
                    .any(|x| matches!(x, OwnedPart::Id(id) if dropped(id))),
                None => mentions(k, &self.drop_bytes),
            },
            // 副索引は最後の要素が記録の ID (値に ID を持つ項目は、持ち主の記録が残る限り残す)。
            Table::Lookup => match parts() {
                Some(p) => matches!(p.last(), Some(OwnedPart::Id(id)) if dropped(id)),
                None => mentions(k, &self.drop_bytes),
            },
            Table::Blobs => match parts() {
                Some(p) => matches!(p.first(), Some(OwnedPart::Id(id)) if record_gone(id)),
                None => mentions(k, &self.drop_bytes),
            },
        };
        if gone { Row::Drop } else { Row::Keep }
    }
}
