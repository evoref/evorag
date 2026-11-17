//! 忘れるの退避の写し直し (03 §7.3 の `forget_backups` の job)。
//!
//! 退避の論理的な写しの途中で記録を変える: 伏せ字にするターンには、そのターンのすべての忘れるの指示を指示の ID
//! の順に純粋な核 ([`crate::forget::apply_to_turn`]) で当て (合わなければそのターンを写しから除く)、封筒の
//! チェックサムを計算し直す。除く ID の記録と、その関係・副索引・`blobs`・`quarantine` の記録を写さない。
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
}

impl RecopyView<'_> {
    /// 伏せ字にするターン `id` の、この写しでの結果。写しに無いか、伏せ字にするターンでなければ `None`。
    #[must_use]
    pub fn turn(&self, id: Id) -> Option<TurnInCopy> {
        self.turns.get(&id).copied()
    }
}

/// 退避の写し直しの対象 (1 つ以上の `forget_backups` の job をまとめたもの)。
pub struct ForgetRecopy<'a> {
    /// 写しから除く記録の ID (job の `ids`)。
    pub remove: BTreeSet<Id>,
    /// 伏せ字にするターンと、生きている DB から読んだそのターンのすべての忘れるの指示の伏せ字 (指示の ID の順)。
    pub turns: BTreeMap<Id, Vec<Redaction>>,
    /// 伏せ字の印 (プロンプトの言語で選ぶ、04 §7.3)。
    pub marker: String,
    /// [`Self::drop`] が真を返しうる記録は、どれかの値の副索引を持つこと。退避を飛ばすかの判定に使う
    /// (例: 忘れた値の鍵の `vk` の副索引)。
    pub candidates: Vec<LookupProbe>,
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
            .finish_non_exhaustive()
    }
}

impl ForgetRecopy<'_> {
    fn id_targets(&self) -> BTreeSet<Id> {
        self.remove
            .iter()
            .chain(self.turns.keys())
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
    if plan.remove.is_empty() && plan.turns.is_empty() && plan.candidates.is_empty() {
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
    if !targets.is_empty() {
        // 写したときの ID の最大より、対象の ID がすべて大きければ、どの対象も (その隔離も) 含まない。隔離の記録の
        // ID は元の記録より後に作るので、元の記録の ID が最大より大きければ、隔離の記録も無い。
        let below_max = match backup::recorded_max_id(src)? {
            Some(max) => targets.iter().next().is_some_and(|min| *min <= max),
            None => true,
        };
        if below_max && holds_any(src, &targets)? {
            return Ok(true);
        }
    }
    let rtx = src.begin_read().map_err(db)?;
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

/// 記録の表のキーか `quarantine` の記録の元の ID に、`targets` のどれかがあるか。
fn holds_any(src: &Database, targets: &BTreeSet<Id>) -> Result<bool, StoreError> {
    let rtx = src.begin_read().map_err(db)?;
    let t = rtx.open_table(RECORDS).map_err(db)?;
    for item in t.iter().map_err(db)? {
        let (k, v) = item.map_err(db)?;
        let Some((kind, id)) = crate::store::split_record_key(k.value()) else {
            continue;
        };
        if targets.contains(&id) {
            return Ok(true);
        }
        if kind == KIND_QUARANTINE
            && quarantined_id(k.value(), v.value()).is_some_and(|o| targets.contains(&o))
        {
            return Ok(true);
        }
    }
    Ok(false)
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
    /// 写さない記録の ID (その関係・副索引・`blobs` も写さない)。
    drop: BTreeSet<Id>,
    drop_bytes: BTreeSet<[u8; 16]>,
    /// 伏せ字にしたターンの、書き直した値 (チェックサムを計算し直したもの)。
    rewritten: BTreeMap<Id, Vec<u8>>,
}

impl Changes {
    fn scan(src: &Database, plan: &ForgetRecopy<'_>) -> Result<Self, StoreError> {
        let rtx = src.begin_read().map_err(db)?;
        let records = rtx.open_table(RECORDS).map_err(db)?;
        let mut out = Self::default();
        let mut turns: BTreeMap<Id, TurnInCopy> = BTreeMap::new();
        let mut quarantined: Vec<(Id, Id)> = Vec::new();
        // 1 回目: 除く ID、伏せ字にするターン、隔離の記録。
        for item in records.iter().map_err(db)? {
            let (k, v) = item.map_err(db)?;
            let (k, v) = (k.value(), v.value());
            let Some((kind, id)) = crate::store::split_record_key(k) else {
                continue;
            };
            if plan.remove.contains(&id) {
                out.drop.insert(id);
            } else if let Some(steps) = plan.turns.get(&id) {
                let state = match Envelope::decode(k, v) {
                    Ok(env) if env.id == id => out.redact(k, env, steps, &plan.marker),
                    _ => TurnInCopy::Dropped,
                };
                if state == TurnInCopy::Dropped {
                    out.drop.insert(id);
                }
                turns.insert(id, state);
            } else if kind == KIND_QUARANTINE
                && let Some(orig) = quarantined_id(k, v)
            {
                quarantined.push((id, orig));
            }
        }
        // 2 回目: 呼び手の判定 (写しのターンの結果を見て)。
        let view = RecopyView { turns: &turns };
        for item in records.iter().map_err(db)? {
            let (k, v) = item.map_err(db)?;
            let (k, v) = (k.value(), v.value());
            let Some((kind, id)) = crate::store::split_record_key(k) else {
                continue;
            };
            if kind == KIND_QUARANTINE || out.drop.contains(&id) || turns.contains_key(&id) {
                continue;
            }
            if let Ok(env) = Envelope::decode(k, v)
                && (plan.drop)(&env, &view)
            {
                out.drop.insert(id);
            }
        }
        // 除く記録と伏せ字にするターンの隔離の記録 (隔離した原文に伏せる前の本文が残るため、04 §7.2)。
        for (qid, orig) in quarantined {
            if out.drop.contains(&orig) || turns.contains_key(&orig) {
                out.drop.insert(qid);
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
        self.drop.is_empty() && self.rewritten.is_empty()
    }

    fn row(&self, table: Table, k: &[u8]) -> Row {
        let dropped = |id: &Id| self.drop.contains(id);
        let parts = || key::decode(k).ok();
        let gone = match table {
            Table::Meta => false,
            Table::Records => match crate::store::split_record_key(k) {
                Some((_, id)) if dropped(&id) => true,
                Some((_, id)) => {
                    if let Some(bytes) = self.rewritten.get(&id) {
                        return Row::Replace(bytes.clone());
                    }
                    false
                }
                None => false,
            },
            // 関係は (from, rel, to) と逆向きの行の両方に ID を持つ。
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
                Some(p) => matches!(p.first(), Some(OwnedPart::Id(id)) if dropped(id)),
                None => mentions(k, &self.drop_bytes),
            },
        };
        if gone { Row::Drop } else { Row::Keep }
    }
}
