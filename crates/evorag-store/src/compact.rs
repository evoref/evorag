//! 消した記録の跡を DB のファイルから除く compact (03 §8) と、その要否の印 (`meta` の `compact.pending`、
//! 03 §3.1.1)。
//!
//! - 印は、ジャーナルに書く種類の記録を消した・伏せ字で書き換えたコミットで立てる (利用者の操作・開くときの削除・
//!   取り込み・忘れるの当て直し・規則 12 の突き合わせ)。作り直せる派生の記録だけの削除では立てない。書き換えて
//!   よいキーで、ジャーナルには書かない。
//! - 起動の順は、ジャーナルの取り込み → 開くときの削除 (どちらも [`Store::open`] の中) →
//!   [`Store::compact_if_pending`] → 書き手を作る → 忘れるの当て直し → 照会と命令の受け付け。空きが足りなければ
//!   compact せずに印を残し、次の起動でやり直す。
//! - compact は、redb の `Database::compact` ではなく、つねに新しいファイルへの論理的な写しと rename で行う
//!   (03 §8、§12.2 の手順)。redb の compact は解放したページを 0 で埋めると約束せず、ファイルの末尾の解放した
//!   領域しか切り詰めないので、消した記録のバイト列が残っていないことを確かめられない。新しいファイルには
//!   生きている行だけを書くので、消した記録のバイト列は入らない。
//! - 写しは同じフォルダの `evorag.redb.compacting` に書き、閉じて開き直し、redb の整合の検査と行ごとの突き合わせを
//!   してから、元のファイルの上に rename で置き換える。印は写しに入れないので、置き換えた時点で消えている。
//! - rename が原子的でないファイルシステム (FAT・exFAT・SMB) では、置き換えの途中で `evorag.redb` が無い・空に
//!   なりうる。そのため写しは、`evorag.redb` が完全 ([`holds_data`]) と確かめたときだけ消し、そうでなければ
//!   残して開くときに戻す ([`recover`])。

use std::path::Path;

use redb::backends::InMemoryBackend;
use redb::{Database, MultimapTableHandle, ReadableDatabase, ReadableTable, TableHandle};

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
    /// compact しなかった (知らない表がある)、または写しか置き換えに失敗した。元のファイルのまま動き、印は残る
    /// (次の起動でやり直す)。
    Failed { reason: String },
}

/// 試験のための止める点と失敗 (09 §3.5)。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactStop {
    /// 止めない。
    Never,
    /// 写しを書いて確かめた後、置き換える前に止まる (強制終了で写しが残った状態)。
    BeforeSwap,
    /// 置き換えの rename が失敗し、元のファイルはそのまま残る。
    SwapFails,
    /// 置き換えの rename が、元のファイルを消した後で失敗する (原子的でないファイルシステム)。
    SwapFailsAfterRemovingMain,
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
    /// [`Store::open`] (ジャーナルの取り込みと開くときの削除) の後、書き手 (`Writer`) を作る前に呼ぶ (DB の
    /// ファイルを置き換えるので、ほかに DB を使っているものがあってはならない。そのため `self` を受け取って返す)。
    ///
    /// `free_bytes` はデータ根のある場所の空き (分からなければ `None`)。DB のファイルの大きさに予備の 16 MiB を
    /// 足した空きが無ければ compact せずに [`Compaction::LowSpace`] を返す。空きが分からないときは試す
    /// (写しに失敗しても元のファイルは変わらない)。DB に知らない表があれば、写すと失うので compact しない
    /// ([`Compaction::Failed`]、03 §4.2 の規則 6)。
    ///
    /// # Errors
    /// 置き換えの後に DB のファイルを開き直せないとき、または置き換えに失敗して `evorag.redb` が完全と確かめられ
    /// ないとき ([`OpenError`]。写しは残し、次に開くときに戻す)。そのときストアは閉じる。写しの失敗と、元の
    /// ファイルが残っている置き換えの失敗は誤りにせず、元のファイルのまま [`Compaction::Failed`] を返す。
    pub fn compact_if_pending(
        self,
        report: &mut OpenReport,
        free_bytes: impl Fn(&Path) -> Option<u64>,
    ) -> Result<Self, OpenError> {
        self.compact_with(report, free_bytes, CompactStop::Never)
    }

    /// [`Self::compact_if_pending`] に、試験のための止める点と失敗を足したもの。
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
        let _ = std::mem::replace(&mut self.db, placeholder).close();
        let swapped = match stop {
            CompactStop::SwapFails => Err(std::io::Error::other("injected swap failure")),
            CompactStop::SwapFailsAfterRemovingMain => {
                std::fs::remove_file(&main)?;
                Err(std::io::Error::other("injected swap failure"))
            }
            CompactStop::Never | CompactStop::BeforeSwap => fsutil::rename_replace(&tmp, &main),
        };
        report.compaction = match swapped {
            Ok(()) => {
                let _ = fsutil::sync_dir(self.data_root());
                let _ = fsutil::prepare_private_file(&main);
                self.db = reopen(&main)?;
                Compaction::Done {
                    before,
                    after: std::fs::metadata(&main).map_or(0, |m| m.len()),
                }
            }
            // 元のファイルが完全に残っているときだけ写しを捨てて元のまま動く。
            Err(e) if holds_data(&main) => {
                let _ = std::fs::remove_file(&tmp);
                self.db = reopen(&main)?;
                Compaction::Failed {
                    reason: format!("could not replace {DB_FILE}: {e}"),
                }
            }
            // 原子的でない rename の途中で止まった。写しを残し、次に開くときに戻す (空の DB を作らない)。
            Err(e) => {
                return Err(OpenError::Unreadable(format!(
                    "could not replace {DB_FILE} ({e}) and it is no longer complete; \
                     {COMPACTING_FILE} is kept and is put back at the next start"
                )));
            }
        };
        Ok(self)
    }
}

/// 置き換えた・残ったファイルを開き直す。ほかのプログラムが一時的に掴んでいるときは試し直す。
fn reopen(main: &Path) -> Result<Database, OpenError> {
    let mut other = None;
    let opened = fsutil::retry_transient(|| {
        Database::open(main).map_err(|e| match e {
            redb::DatabaseError::Storage(redb::StorageError::Io(io)) => io,
            e => {
                let io = std::io::Error::other(e.to_string());
                other = Some(e);
                io
            }
        })
    });
    opened.map_err(|io| {
        map_db_error(other.unwrap_or(redb::DatabaseError::Storage(redb::StorageError::Io(io))))
    })
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

/// `src` の生きている行を、すべての表について `path` の新しいファイルへ写す。写しを書き切って閉じ、開き直して
/// redb の整合の検査をしてから、元の同じ時点の写し (読み取りのトランザクション) と行ごとに突き合わせる (03 §12.2 の
/// 「件数とチェックサムを突き合わせて」。写しは行のバイト列が同じなので、すべての行を比べる)。
///
/// 知らない表 (新しい版が足した表、multimap の表) があれば写さずに断る (写すと失うため、03 §4.2 の規則 6)。
fn copy_verified(src: &Database, path: &Path) -> Result<(), String> {
    let rtx = src.begin_read().map_err(err)?;
    let known: Vec<&str> = TABLES.iter().map(|t| t.name()).collect();
    let unknown: Vec<String> = rtx
        .list_tables()
        .map_err(err)?
        .map(|t| t.name().to_owned())
        .filter(|n| !known.contains(&n.as_str()))
        .chain(
            rtx.list_multimap_tables()
                .map_err(err)?
                .map(|t| t.name().to_owned()),
        )
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "unknown tables are not compacted: {}",
            unknown.join(", ")
        ));
    }
    if path.exists() {
        std::fs::remove_file(path).map_err(err)?;
    }
    fsutil::prepare_private_file(path).map_err(err)?;
    let dst = Database::create(path).map_err(err)?;
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
    dst.close().map_err(err)?;
    // ディスクにあるものを確かめる (書いた手元の状態ではなく)。
    let mut on_disk = Database::open(path).map_err(err)?;
    if !on_disk.check_integrity().map_err(err)? {
        return Err("the compacted copy failed the integrity check".into());
    }
    verify(&rtx, &on_disk)?;
    on_disk.close().map_err(err)
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

/// redb のファイルとして開け、`meta` の `data_root.id` を読める (中身のある完全な DB) か。
pub(crate) fn holds_data(path: &Path) -> bool {
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0) {
        return false;
    }
    let Ok(database) = Database::open(path) else {
        return false;
    };
    database
        .begin_read()
        .ok()
        .and_then(|rtx| {
            let t = rtx.open_table(META).ok()?;
            let v = t.get(meta::DATA_ROOT_ID.as_bytes()).ok()??;
            Some(!v.value().is_empty())
        })
        .unwrap_or(false)
}

/// 前の compact が残した写しを片付ける。データ根を開く前 (ロックを取った後) に呼ぶ。
///
/// - `evorag.redb` が完全なら ([`holds_data`])、写しは置き換えに使われなかったものなので捨てる (その後に消した
///   記録が入っているかもしれないので残さない)。消せなくても開くのは止めない (次の compact が消し直す)。
/// - `evorag.redb` が無い・空・開けないときに、写しが完全なら、写しを `evorag.redb` に戻す (原子的でない rename の
///   途中で止まった)。空でない開けない `evorag.redb` は、消さずに `broken/` へ移してから戻す。
/// - どちらも完全でなければ何もしない (`evorag.redb` が無ければ、開くときに新しい DB を作らずに止まる)。
///
/// # Errors
/// 写しを戻せないとき。
pub(crate) fn recover(root: &Path, now_ms: i64) -> std::io::Result<()> {
    let tmp = root.join(COMPACTING_FILE);
    if !tmp.exists() {
        return Ok(());
    }
    let main = root.join(DB_FILE);
    if holds_data(&main) {
        if fsutil::retry_transient(|| std::fs::remove_file(&tmp)).is_ok() {
            let _ = fsutil::sync_dir(root);
        }
        return Ok(());
    }
    if !holds_data(&tmp) {
        return Ok(());
    }
    if std::fs::metadata(&main).is_ok_and(|m| m.len() > 0) {
        crate::restore::move_broken(root, now_ms)?;
    }
    fsutil::rename_durable(&tmp, &main)
}
