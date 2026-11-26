//! `evorag repair` のストアの層 (03 §10、FR-211)。
//!
//! 1 つのトランザクションで、次の順に行う。
//! 1. ジャーナルにあって DB に無い記録を戻す。削除の行のある ID と、`meta` の `cleanup.pending_deletes` (開くときに
//!    消して、まだ整理の job にしていない ID) は戻さない。同じ ID の `put` の行が 2 つ以上あれば、連番の順に、種類の
//!    単調に増える項目 (ターンの改訂の番号) を下げない最後の行を採る (伏せ字の前の行を当てない、03 §6 の
//!    「突き合わせ」)。壊れた記録 (チェックサムが合わない) は quarantine へ移してから戻す (03 §8)。
//! 2. ジャーナルの `meta` の行を、DB にそのキーが無いときだけ書く (一度だけ書くキーの規則。
//!    `memory.forget_key` は開くときの照合だけが扱う、03 §3.1.1)。
//! 3. 関係 (`links`) と副索引 (`lookup`) を、記録から作り直す (種類の [`crate::KindSpec::derive`])。作り直せる記録
//!    (知っている種類で、今の版で読めて、`derive` を持つもの) は、この版の `derive` が作りうる名前
//!    ([`crate::KindSpec::derived_rels`]・[`crate::KindSpec::derived_lookups`]) の行だけを消してから書き直す。
//!    宣言の無い名前の行と、知らない種類・新しい版・読めない記録・`derive` の無い種類の行は残す (03 §4.2 の規則
//!    6・11)。記録の無い ID の行は、どれかの種類が宣言した名前のものだけを消す。
//! 4. データ根に忘れるの指示が 1 つでもあれば `forget.recheck` を立てる (次の起動で、すべての忘れるの指示の退避の
//!    写し直しの job を作り直させる。失った DB にあった job を取り戻すため)。
//!
//! 開くときの取り込み (`journal.last_seq` より後の行) と、削除の行のある記録の削除は、[`Store::open`] が済ませて
//! いる。忘れるの当て直しは、呼び手 (コア) が書き手を通して行う。

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::Ordering;

use evorag_types::Id;
use redb::ReadableTable;

use crate::envelope::Envelope;
use crate::journal::LineOp;
use crate::key::{self, OwnedPart};
use crate::store::{
    Durability, LINKS, LOOKUP, META, RECORDS, Read, Store, StoreError, WriteTxn, classify, db,
    parse_ids, record_key, split_record_key,
};
use crate::writer::{WriteOp, WriteOutcome};

/// `repair` のストアの層がしたこと。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repaired {
    /// ジャーナルから戻した記録の数 (種類ごと)。
    pub restored: BTreeMap<String, usize>,
    /// 戻した記録の `put` の行のうち、単調に増える項目 (ターンの改訂の番号) を下げるので当てなかった行の数。
    pub lower_rev_skipped: usize,
    /// ジャーナルから戻す前に quarantine へ移した、壊れた記録の数。
    pub quarantined: usize,
    /// ジャーナルの `meta` の行から書いた、DB に無かったキーの数。
    pub metas_restored: usize,
    /// ジャーナルの、チェックサムの合わない (読まなかった) 行の数。
    pub journal_bad_lines: usize,
    /// ジャーナルの行の欠けた組の数 (読める行は戻した、03 §6)。
    pub journal_incomplete_groups: usize,
    /// 書き足した関係 (逆向きの行を含む)。
    pub links_added: usize,
    /// 消した関係 (記録から作れない・記録の無いもの。逆向きの行を含む)。
    pub links_removed: usize,
    /// 書き足した副索引の項目。
    pub lookups_added: usize,
    /// 消した副索引の項目。
    pub lookups_removed: usize,
    /// `forget.recheck` を立てた (忘れるの指示がある)。
    pub forget_recheck: bool,
}

impl Repaired {
    /// 戻した記録の合計。
    #[must_use]
    pub fn restored_total(&self) -> usize {
        self.restored.values().sum()
    }
}

/// 記録の扱い。
enum Source {
    /// 記録から関係と副索引を作り直す (知っている種類で、今の版で読めて、`derive` を持つ)。種類の名前を持つ。
    Rederive(String),
    /// 残す (知らない種類・新しい版・読めない記録・`derive` の無い種類。関係と副索引は消さない)。
    Keep,
}

impl Store {
    /// `evorag repair` のストアの層 (03 §10)。ジャーナルにあって DB に無い記録と `meta` のキーを戻し、関係と副索引を
    /// 記録から作り直す。書き手 (`Writer`) を作る前に呼ぶ。
    ///
    /// `forget_directive` は、記録が忘れるの指示かを決める (種類を持つ crate が渡す)。当たる記録が 1 つでもあれば、
    /// 同じトランザクションで `forget.recheck` を立てる。
    ///
    /// # Errors
    /// 読むだけで開いた・書き込みの権利をもう渡した・ジャーナルを読めない・DB に書けないとき (何も書かない)。
    pub fn repair(&self, forget_directive: fn(&Envelope) -> bool) -> Result<Repaired, StoreError> {
        if let Some(r) = self.read_only() {
            return Err(StoreError::ReadOnly(r.clone()));
        }
        if self.token_issued.load(Ordering::SeqCst) {
            return Err(StoreError::TokenTaken);
        }
        let read = crate::journal::read_all(self.data_root())
            .map_err(|e| StoreError::Io(format!("journal: {e}")))?;
        let lines = read.lines;
        let mut out = Repaired {
            journal_bad_lines: read.bad,
            journal_incomplete_groups: read.incomplete_groups,
            ..Repaired::default()
        };
        let mut tx = self.write_internal(Durability::Immediate)?;
        let max_restored = self.restore_missing(&mut tx, &lines, &mut out)?;
        out.metas_restored = restore_metas(&mut tx, &lines)?;
        let has_forgets = self.rebuild_relations(&mut tx, forget_directive, &mut out)?;
        if has_forgets {
            let mut m = tx.tx.open_table(META).map_err(db)?;
            m.insert(
                crate::meta::FORGET_RECHECK.as_bytes(),
                crate::meta::FORGET_RECHECK_SET.as_bytes(),
            )
            .map_err(db)?;
            out.forget_recheck = true;
        }
        // 作り直した DB は、ジャーナルの末尾の組をコミットしたか分からない (次に開くときに切り詰めない、03 §6)。
        // 行の欠けた組の読める行を当てたなら、その記録の ID を `end` の計画し直しの印に残す。
        {
            let mut m = tx.tx.open_table(META).map_err(db)?;
            m.insert(
                crate::meta::JOURNAL_TAIL_UNKNOWN.as_bytes(),
                b"1".as_slice(),
            )
            .map_err(db)?;
        }
        if !read.partial.is_empty() {
            tx.add_ids_meta(crate::meta::JOURNAL_PARTIAL_GROUPS, &read.partial)?;
        }
        tx.commit()?;
        // 戻した記録の ID を、新しい ID の下限に入れる (03 §3.3)。
        if let Some(max) = max_restored {
            self.ids().raise_floor(max);
        }
        Ok(out)
    }

    /// ジャーナルにあって DB に無い記録を戻す。戻した ID の最大を返す。
    fn restore_missing(
        &self,
        tx: &mut WriteTxn<'_>,
        lines: &[crate::journal::Line],
        out: &mut Repaired,
    ) -> Result<Option<Id>, StoreError> {
        // 消したとみなす ID: 削除の行のあるものと、開くときに消してまだ整理の job にしていないもの (03 §7.3)。
        let mut deleted: BTreeSet<Id> = lines
            .iter()
            .filter_map(|l| match &l.op {
                LineOp::Delete { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        if let Some(v) = meta_in(tx, crate::meta::PENDING_DELETES)? {
            deleted.extend(parse_ids(&v).ids);
        }
        // ID ごとに、当てる行と、単調に増える項目を下げるので当てない行の数。
        let mut picks: BTreeMap<Id, (Envelope, usize)> = BTreeMap::new();
        for line in lines {
            let LineOp::Put(env) = &line.op else {
                continue;
            };
            let spec = self.kinds.get(env.kind.as_str());
            // 消したとみなす ID は戻さない (03 §6)。ジャーナルに書かない種類の行は無いはずなので当てない。
            if deleted.contains(&env.id) || spec.is_some_and(|s| !s.journaled) {
                continue;
            }
            match picks.entry(env.id) {
                Entry::Vacant(v) => {
                    v.insert((env.clone(), 0));
                }
                Entry::Occupied(mut o) => {
                    let (picked, lower) = o.get_mut();
                    if spec.is_some_and(|s| s.lowers_monotonic(picked, env)) {
                        *lower += 1;
                    } else {
                        *picked = env.clone();
                    }
                }
            }
        }
        let mut broken = BTreeSet::new();
        let mut missing = Vec::new();
        for (id, (env, lower)) in picks {
            match tx.get(&env.kind, id)? {
                None => missing.push((env, lower)),
                Some(Read::Corrupt { .. }) => {
                    broken.insert(record_key(&env.kind, id));
                    missing.push((env, lower));
                }
                Some(Read::Ok(_) | Read::Newer { .. }) => {}
            }
        }
        if !broken.is_empty() {
            out.quarantined = tx.quarantine_keys(&broken)?;
        }
        let mut max = None;
        for (env, lower) in missing {
            tx.put_raw(&env)?;
            *out.restored.entry(env.kind.clone()).or_default() += 1;
            out.lower_rev_skipped += lower;
            max = max.max(Some(env.id));
        }
        Ok(max)
    }

    /// 関係と副索引を記録から作り直す。忘れるの指示 (`forget_directive` に当たる記録) があるかを返す。
    fn rebuild_relations(
        &self,
        tx: &mut WriteTxn<'_>,
        forget_directive: fn(&Envelope) -> bool,
        out: &mut Repaired,
    ) -> Result<bool, StoreError> {
        let mut sources: BTreeMap<Id, Source> = BTreeMap::new();
        let mut derived: Vec<WriteOp> = Vec::new();
        let mut has_forgets = false;
        {
            let records = tx.tx.open_table(RECORDS).map_err(db)?;
            for item in records.iter().map_err(db)? {
                let (k, v) = item.map_err(db)?;
                let Some((kind, id)) = split_record_key(k.value()) else {
                    continue;
                };
                let spec = self.kinds.get(kind.as_str());
                let source = match classify(&kind, id, k.value(), v.value()) {
                    Read::Ok(e) => {
                        has_forgets |= forget_directive(&e);
                        match spec.and_then(|s| Some((s, s.derive?))) {
                            Some((s, derive)) => {
                                let ops = derive(&e);
                                if e.v > s.current_v {
                                    // 新しい版の記録は、前に作った関係を残したまま、読める分を書き足す。
                                    derived.extend(ops);
                                    Source::Keep
                                } else if ops.is_empty() {
                                    // 関係に要る項目を読めない記録は、前に作った関係を残す (03 §4.2 の規則 11)。
                                    Source::Keep
                                } else {
                                    derived.extend(ops);
                                    Source::Rederive(kind)
                                }
                            }
                            // 知らない種類と、`derive` の無い種類は、関係と副索引に触れない。
                            None => Source::Keep,
                        }
                    }
                    _ => Source::Keep,
                };
                match sources.entry(id) {
                    Entry::Vacant(v) => {
                        v.insert(source);
                    }
                    // 同じ ID の記録が 2 つの種類にあれば、どちらの行も残す側に倒す。
                    Entry::Occupied(mut o) => {
                        o.insert(Source::Keep);
                    }
                }
            }
        }
        // 記録の無い ID の行は、どれかの種類の `derive` が作りうる名前のものだけを消す (新しい版が、記録でない ID
        // から張った関係かもしれないものは残す)。
        let known_rels: BTreeSet<&str> = self
            .kinds
            .values()
            .flat_map(|s| s.derived_rels.iter().copied())
            .collect();
        let drop_link = |k: &[u8]| -> bool {
            let Some((owner, rel)) = link_owner(k) else {
                // キーとして読めない行 (新しい版の形かもしれない) は残す。
                return false;
            };
            match sources.get(&owner) {
                Some(Source::Rederive(kind)) => self
                    .kinds
                    .get(kind.as_str())
                    .is_some_and(|s| s.derived_rels.contains(&rel.as_str())),
                Some(Source::Keep) => false,
                None => known_rels.contains(rel.as_str()),
            }
        };
        let drop_lookup = |k: &[u8]| -> bool {
            let Some((kind, field, id)) = lookup_owner(k) else {
                return false;
            };
            let declared = self
                .kinds
                .get(kind.as_str())
                .is_some_and(|s| s.derived_lookups.contains(&field.as_str()));
            declared
                && match sources.get(&id) {
                    Some(Source::Rederive(owner)) => *owner == kind,
                    Some(Source::Keep) => false,
                    None => true,
                }
        };
        let links_before = keys_of(tx, LINKS)?;
        let lookups_before = keys_of(tx, LOOKUP)?;
        {
            let mut links = tx.tx.open_table(LINKS).map_err(db)?;
            for k in links_before.iter().filter(|k| drop_link(k)) {
                links.remove(k.as_slice()).map_err(db)?;
            }
            let mut lookup = tx.tx.open_table(LOOKUP).map_err(db)?;
            for k in lookups_before.iter().filter(|k| drop_lookup(k)) {
                lookup.remove(k.as_slice()).map_err(db)?;
            }
        }
        let mut outcome = WriteOutcome::default();
        for op in &derived {
            crate::writer::apply_op(tx, op, &mut outcome)?;
        }
        let links_after = keys_of(tx, LINKS)?;
        let lookups_after = keys_of(tx, LOOKUP)?;
        out.links_added = links_after.difference(&links_before).count();
        out.links_removed = links_before.difference(&links_after).count();
        out.lookups_added = lookups_after.difference(&lookups_before).count();
        out.lookups_removed = lookups_before.difference(&lookups_after).count();
        Ok(has_forgets)
    }
}

/// ジャーナルの `meta` の行を、DB にそのキーが無いときだけ書く (一度だけ書くキーの規則、`set_meta_once`)。上書きして
/// よいキーは、連番の最も大きい行の値にする (行は連番の順)。`memory.forget_key` は開くときの照合
/// (`check_forget_key`) だけが扱うので、ここでは書かない (03 §3.1.1)。
/// 書いたキーの数を返す。
fn restore_metas(
    tx: &mut WriteTxn<'_>,
    lines: &[crate::journal::Line],
) -> Result<usize, StoreError> {
    let mut n = 0;
    for line in lines {
        let LineOp::Meta { key, value } = &line.op else {
            continue;
        };
        // 上書きしてよいキー (`import.old_root`・`import.v04_ended`) は、連番の最も大きい行の値を採る (03 §3.1.1)。
        if key == crate::meta::IMPORT_OLD_ROOT || key == crate::meta::IMPORT_V04_ENDED {
            if meta_in(tx, key)?.as_deref() != Some(value.as_str()) {
                match tx.set_meta_latest(key, value) {
                    Ok(()) => n += 1,
                    Err(StoreError::MetaConflict { .. }) => {}
                    Err(e) => return Err(e),
                }
            }
            continue;
        }
        if key == crate::meta::FORGET_KEY || meta_in(tx, key)?.is_some() {
            continue;
        }
        match tx.set_meta_once(key, value) {
            Ok(()) => n += 1,
            // 一度だけ書くキーでない (この版の知らないキー) ものは書かない。
            Err(StoreError::MetaConflict { .. } | StoreError::MetaNotWritable(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// トランザクションの中の `meta` の値。
fn meta_in(tx: &WriteTxn<'_>, key: &str) -> Result<Option<String>, StoreError> {
    let m = tx.tx.open_table(META).map_err(db)?;
    Ok(m.get(key.as_bytes())
        .map_err(db)?
        .map(|v| String::from_utf8_lossy(v.value()).into_owned()))
}

/// 表のすべてのキー。
fn keys_of(tx: &WriteTxn<'_>, table: crate::store::Raw) -> Result<HashSet<Vec<u8>>, StoreError> {
    let t = tx.tx.open_table(table).map_err(db)?;
    let mut out = HashSet::new();
    for item in t.iter().map_err(db)? {
        let (k, _) = item.map_err(db)?;
        out.insert(k.value().to_vec());
    }
    Ok(out)
}

/// 関係の行の持ち主と、順向きの関係の名前 (順向きの行 `(from, rel, to)` は `from`、逆向きの行 `(to, ~rel, from)` は
/// `from`)。読めないキーは `None`。
fn link_owner(k: &[u8]) -> Option<(Id, String)> {
    match key::decode(k).ok()?.as_slice() {
        [OwnedPart::Id(a), OwnedPart::Str(rel), OwnedPart::Id(b)] => {
            Some(match rel.strip_prefix(crate::relations::REVERSE) {
                Some(forward) => (*b, forward.to_owned()),
                None => (*a, rel.clone()),
            })
        }
        _ => None,
    }
}

/// 副索引の項目の種類・項目・ID。読めないキーは `None`。
fn lookup_owner(k: &[u8]) -> Option<(String, String, Id)> {
    match key::decode(k).ok()?.as_slice() {
        [
            OwnedPart::Str(kind),
            OwnedPart::Str(field),
            _,
            OwnedPart::Id(id),
        ] => Some((kind.clone(), field.clone(), *id)),
        _ => None,
    }
}
