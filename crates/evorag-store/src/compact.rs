//! 消した記録の跡を DB のファイルから除く compact (03 §8) と、その要否の印 (`meta` の `compact.pending`、
//! 03 §3.1.1)。
//!
//! - 印は、削除・忘れる・忘れるの当て直し・開くときのジャーナルの削除の行・規則 12 の突き合わせで記録を消した・
//!   書き換えたコミットで立てる。書き換えてよいキーで、ジャーナルには書かない。
//! - 書き込みで開いた後、忘れるの当て直しの後に、コアが [`Store::compact_if_pending`] を呼ぶ (照会と命令を
//!   受け付ける前)。空きが足りなければ compact せずに印を残し、次の起動でやり直す。
//! - compact は、redb の `Database::compact` ではなく、新しいファイルへの論理的な写しと rename で行う
//!   (03 §8 の「compact で足りないとき」と §12.2 の手順)。redb の compact は解放したページを 0 で埋めると
//!   約束せず、ファイルの末尾の解放した領域しか切り詰めないので、消した記録のバイト列が残っていないことを
//!   確かめられない。新しいファイルには生きている行だけを書くので、消した記録のバイト列は入らない。
//! - 写しは同じフォルダの `evorag.redb.compacting` に書き、行ごとに突き合わせてから、元のファイルの上に
//!   rename で置き換える (1 回の置き換えなので、強制終了しても、元か写しのどちらかが `evorag.redb` にある)。
//!   印は写しに入れないので、置き換えた時点で消えている。開くときに残っている写しは捨てる ([`recover`])。

use std::path::Path;

use redb::backends::InMemoryBackend;
use redb::{Database, ReadableDatabase, ReadableTable, TableHandle};

use crate::fsutil;
use crate::meta;
use crate::store::{
    DB_FILE, META, OpenError, OpenReport, RESERVE_BYTES, Raw, Store, StoreError, TABLES, WriteTxn,
    db, map_db_error,
};

/// 立てたときの値。
pub(crate) const PENDING: &str = "1";

/// compact の写しを書くファイル (データ根の中)。
pub(crate) const COMPACTING_FILE: &str = "evorag.redb.compacting";

/// compact の後にも残しておく空き (会話の追記の予備のファイルと同じ大きさ、03 §8)。
const MARGIN_BYTES: u64 = RESERVE_BYTES;

/// 起動時の compact の結果 (03 §8)。画面と `doctor` が利用者に知らせる。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Compaction {
    /// 印が無かった (または読むだけで開いた)。
    #[default]
    NotPending,
    /// compact した。前後のファイルの大きさ (バイト)。
    Done { before: u64, after: u64 },
    /// 空きが足りないので compact しなかった。印は残り、次の起動でやり直す。
    LowSpace { free: u64, need: u64 },
    /// 写しか置き換えに失敗した。元のファイルのまま動き、印は残る (次の起動でやり直す)。
    Failed { reason: String },
}

/// 試験のための止める点 (09 §3.5)。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactStop {
    /// 止めない。
    Never,
    /// 写しを書いて突き合わせた後、置き換える前に止まる (強制終了で写しが残った状態)。
    BeforeSwap,
}

impl Store {
    /// 次の起動で compact が要るか。無い・知らない値は「要らない」(03 §3.1.1)。
    ///
    /// # Errors
    /// `meta` を読めないとき。
    pub fn compact_pending(&self) -> Result<bool, StoreError> {
        Ok(self.meta(meta::COMPACT_PENDING)?.as_deref() == Some(PENDING))
    }

    /// `compact.pending` が立っていれば compact する (03 §8)。結果は `report.compaction` に書く。
    ///
    /// 起動の順は、ジャーナルの取り込み → 開くときの削除 (どちらも [`Store::open`] の中) → 忘れるの当て直し →
    /// この compact → 照会と命令の受け付け。書き手 (`Writer`) を作る前に呼ぶ (DB のファイルを置き換えるので、
    /// ほかに DB を使っているものがあってはならない。そのため `self` を受け取って返す)。
    ///
    /// `free_bytes` はデータ根のある場所の空き (分からなければ `None`)。DB のファイルの大きさに予備の分を
    /// 足した空きが無ければ compact せずに [`Compaction::LowSpace`] を返す。空きが分からないときは試す
    /// (写しに失敗しても元のファイルは変わらない)。
    ///
    /// # Errors
    /// 置き換えの後に DB のファイルを開き直せないとき ([`OpenError`])。そのときストアは閉じる。写しや置き換え
    /// の失敗は誤りにせず、元のファイルのまま [`Compaction::Failed`] を返す。
    pub fn compact_if_pending(
        self,
        report: &mut OpenReport,
        free_bytes: impl Fn(&Path) -> Option<u64>,
    ) -> Result<Self, OpenError> {
        self.compact_with(report, free_bytes, CompactStop::Never)
    }

    /// [`Self::compact_if_pending`] に、試験のための止める点を足したもの。
    #[doc(hidden)]
    pub fn compact_with(
        mut self,
        report: &mut OpenReport,
        free_bytes: impl Fn(&Path) -> Option<u64>,
        stop: CompactStop,
    ) -> Result<Self, OpenError> {
        report.compaction = Compaction::NotPending;
        if self.read_only().is_some() {
            return Ok(self);
        }
        match self.compact_pending() {
            Ok(true) => {}
            Ok(false) => return Ok(self),
            Err(e) => {
                report.compaction = Compaction::Failed {
                    reason: e.to_string(),
                };
                return Ok(self);
            }
        }
        let main = self.data_root().join(DB_FILE);
        let before = std::fs::metadata(&main).map_or(0, |m| m.len());
        let need = before.saturating_add(MARGIN_BYTES);
        if let Some(free) = free_bytes(self.data_root())
            && free < need
        {
            report.compaction = Compaction::LowSpace { free, need };
            return Ok(self);
        }
        let tmp = self.data_root().join(COMPACTING_FILE);
        let placeholder = copy_verified(&self.db, &tmp).and_then(|()| {
            // 元のファイルを閉じる間だけ持つ空の DB (ストアの DB を空にしないため)。
            Database::builder()
                .create_with_backend(InMemoryBackend::new())
                .map_err(|e| e.to_string())
        });
        let placeholder = match placeholder {
            Ok(p) => p,
            Err(reason) => {
                let _ = std::fs::remove_file(&tmp);
                report.compaction = Compaction::Failed { reason };
                return Ok(self);
            }
        };
        if stop == CompactStop::BeforeSwap {
            return Ok(self);
        }
        // 元のファイルを閉じてから置き換える (Windows では開いているファイルの上に置き換えられない)。
        drop(std::mem::replace(&mut self.db, placeholder));
        let swapped = fsutil::rename_replace(&tmp, &main);
        match &swapped {
            Ok(()) => {
                let _ = fsutil::sync_dir(self.data_root());
            }
            Err(_) => {
                let _ = std::fs::remove_file(&tmp);
            }
        }
        let _ = fsutil::prepare_private_file(&main);
        self.db = Database::open(&main).map_err(map_db_error)?;
        report.compaction = match swapped {
            Ok(()) => Compaction::Done {
                before,
                after: std::fs::metadata(&main).map_or(0, |m| m.len()),
            },
            Err(e) => Compaction::Failed {
                reason: format!("could not replace {DB_FILE}: {e}"),
            },
        };
        Ok(self)
    }
}

impl WriteTxn<'_> {
    /// 次の起動で compact が要ることを、このトランザクションで書く (記録を消した・書き換えたのと同じコミットで)。
    ///
    /// # Errors
    /// `meta` に書けないとき。
    pub fn set_compact_pending(&mut self) -> Result<(), StoreError> {
        let mut m = self.tx.open_table(META).map_err(db)?;
        m.insert(meta::COMPACT_PENDING.as_bytes(), PENDING.as_bytes())
            .map_err(db)?;
        Ok(())
    }

    /// compact を終えた後に、印を消す。
    ///
    /// # Errors
    /// `meta` に書けないとき。
    pub fn clear_compact_pending(&mut self) -> Result<(), StoreError> {
        let mut m = self.tx.open_table(META).map_err(db)?;
        m.remove(meta::COMPACT_PENDING.as_bytes()).map_err(db)?;
        Ok(())
    }
}

/// 写さない行 (`meta` の `compact.pending`。写しを終えた時点で compact は済むため)。
fn skipped(table: Raw, key: &[u8]) -> bool {
    table.name() == META.name() && key == meta::COMPACT_PENDING.as_bytes()
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// `src` の生きている行を、すべての表について `path` の新しいファイルへ写し、書き切ってから行ごとに突き合わせる
/// (03 §12.2 の「件数とチェックサムを突き合わせて」。写しは行のバイト列が同じなので、すべての行を比べる)。
fn copy_verified(src: &Database, path: &Path) -> Result<(), String> {
    if path.exists() {
        std::fs::remove_file(path).map_err(err)?;
    }
    fsutil::prepare_private_file(path).map_err(err)?;
    let dst = Database::create(path).map_err(err)?;
    let rtx = src.begin_read().map_err(err)?;
    let mut wtx = dst.begin_write().map_err(err)?;
    wtx.set_durability(redb::Durability::Immediate)
        .map_err(err)?;
    for t in TABLES {
        let mut to = wtx.open_table(t).map_err(err)?;
        let from = match rtx.open_table(t) {
            Ok(from) => from,
            Err(redb::TableError::TableDoesNotExist(_)) => continue,
            Err(e) => return Err(err(e)),
        };
        for item in from.iter().map_err(err)? {
            let (k, v) = item.map_err(err)?;
            if !skipped(t, k.value()) {
                to.insert(k.value(), v.value()).map_err(err)?;
            }
        }
    }
    wtx.commit().map_err(err)?;
    verify(&rtx, &dst)
}

/// 写しが、元の生きている行 (写さない行を除く) とキーも値も同じで、行の数も同じことを確かめる。
fn verify(src: &redb::ReadTransaction, dst: &Database) -> Result<(), String> {
    let rtx = dst.begin_read().map_err(err)?;
    for t in TABLES {
        let to = rtx.open_table(t).map_err(err)?;
        let mut copied = to.iter().map_err(err)?;
        let from = match src.open_table(t) {
            Ok(from) => Some(from),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(err(e)),
        };
        if let Some(from) = from {
            for item in from.iter().map_err(err)? {
                let (k, v) = item.map_err(err)?;
                if skipped(t, k.value()) {
                    continue;
                }
                let Some(c) = copied.next() else {
                    return Err(format!("table `{}` lost rows in the copy", t.name()));
                };
                let (ck, cv) = c.map_err(err)?;
                if ck.value() != k.value() || cv.value() != v.value() {
                    return Err(format!("table `{}` differs in the copy", t.name()));
                }
            }
        }
        if copied.next().is_some() {
            return Err(format!("table `{}` gained rows in the copy", t.name()));
        }
    }
    Ok(())
}

/// 前の compact が置き換えの前に止まった跡 (写し) を捨てる。データ根を開く前 (ロックを取った後) に呼ぶ。
///
/// 写しを捨てるのは、`evorag.redb` があるときだけにする (置き換えは 1 回の rename なので、`evorag.redb` が
/// あれば、写しは置き換えに使われなかったもの)。写しには、その後に消した記録が入っているかもしれないので残さない。
/// 消せなくても開くのは止めない (次の compact が写しを書く前に消し直す)。
pub(crate) fn recover(root: &Path) {
    let tmp = root.join(COMPACTING_FILE);
    if tmp.exists() && root.join(DB_FILE).is_file() {
        let removed = fsutil::retry_transient(|| std::fs::remove_file(&tmp));
        if removed.is_ok() {
            let _ = fsutil::sync_dir(root);
        }
    }
}
