//! DB の形式の移行 (03 §12.1、FR-214)。
//!
//! 1. 移行の前に退避を作る (03 §7.2)。作れなければ移行しない。
//! 2. 移行先の新しいファイル (`evorag.redb.migrating`) に、`migration.state` を `in_progress:…` にして書く。
//! 3. 移行の関数が表を写し、必要なら記録を書き換える。
//! 4. 件数とチェックサムを突き合わせ、`migration.state` を `none` にしてから、ファイルを入れ替える。
//! 5. どこかで失敗したら、移行先を捨て、元の DB のまま (読むだけで) 開く。
//!
//! 起動時には、前の移行が途中で止まった跡を片付ける (`recover`)。
//!
//! v0.1〜v0.4 の間は、移行の関数を直前の版の分だけ持てばよい (03 §12.1)。

use std::path::{Path, PathBuf};

use redb::{Database, ReadableDatabase, ReadableTable};

use crate::backup;
use crate::envelope::{DecodeError, Envelope};
use crate::fsutil;
use crate::meta;
use crate::store::{BLOBS, DB_FILE, FORMAT_VERSION, LINKS, LOOKUP, META, RECORDS, Raw, TABLES};
use crate::version::Version;

/// 移行の関数。`ctx` を通して、元の DB を読み、移行先に書く。
pub type MigrateFn = fn(&mut MigrationCtx<'_>) -> Result<(), String>;

/// 1 つの版から次の版への移行。
#[derive(Clone, Copy)]
pub struct Migration {
    pub from: &'static str,
    pub to: &'static str,
    pub run: MigrateFn,
    /// 移行で行の数が変わる表。それ以外の表は、移行の前後で行の数が同じことを確かめる。
    pub counts_change: &'static [Table],
}

impl std::fmt::Debug for Migration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Migration({} -> {})", self.from, self.to)
    }
}

/// この版の evorag が持つ移行 (v0.1 は最初の形式なので無い)。
pub const MIGRATIONS: &[Migration] = &[];

/// この evorag が書く形式と、持っている移行。
#[derive(Debug, Clone)]
pub struct FormatPlan {
    pub version: String,
    pub min_read: String,
    pub min_write: String,
    pub migrations: Vec<Migration>,
}

impl FormatPlan {
    pub fn current() -> Self {
        Self {
            version: FORMAT_VERSION.to_owned(),
            min_read: FORMAT_VERSION.to_owned(),
            min_write: FORMAT_VERSION.to_owned(),
            migrations: MIGRATIONS.to_vec(),
        }
    }
}

/// 表の名前 (移行の関数から見える形。redb の型を出さない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Meta,
    Records,
    Links,
    Lookup,
    Blobs,
}

impl Table {
    pub const ALL: [Table; 5] = [
        Table::Meta,
        Table::Records,
        Table::Links,
        Table::Lookup,
        Table::Blobs,
    ];

    fn def(self) -> Raw {
        match self {
            Table::Meta => META,
            Table::Records => RECORDS,
            Table::Links => LINKS,
            Table::Lookup => LOOKUP,
            Table::Blobs => BLOBS,
        }
    }
}

/// 移行の関数に渡す口。
pub struct MigrationCtx<'a> {
    src: &'a redb::ReadTransaction,
    dst: &'a redb::WriteTransaction,
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

impl MigrationCtx<'_> {
    /// 元の表を、そのまま移行先へ写す。写した行の数を返す。
    pub fn copy_table(&mut self, t: Table) -> Result<u64, String> {
        let src = self.src.open_table(t.def()).map_err(err)?;
        let mut dst = self.dst.open_table(t.def()).map_err(err)?;
        let mut n = 0;
        for item in src.iter().map_err(err)? {
            let (k, v) = item.map_err(err)?;
            dst.insert(k.value(), v.value()).map_err(err)?;
            n += 1;
        }
        Ok(n)
    }

    /// 元の表の行を順に読む。
    pub fn for_each(
        &self,
        t: Table,
        mut f: impl FnMut(&[u8], &[u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        let src = self.src.open_table(t.def()).map_err(err)?;
        for item in src.iter().map_err(err)? {
            let (k, v) = item.map_err(err)?;
            f(k.value(), v.value())?;
        }
        Ok(())
    }

    /// 移行先に 1 行を書く (同じキーがあれば置き換える)。
    pub fn put(&mut self, t: Table, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.dst
            .open_table(t.def())
            .map_err(err)?
            .insert(key, value)
            .map_err(err)?;
        Ok(())
    }

    /// 移行先から 1 行を消す。
    pub fn remove(&mut self, t: Table, key: &[u8]) -> Result<(), String> {
        self.dst
            .open_table(t.def())
            .map_err(err)?
            .remove(key)
            .map_err(err)?;
        Ok(())
    }
}

pub(crate) const MIGRATING_FILE: &str = "evorag.redb.migrating";
pub(crate) const PRE_MIGRATION_FILE: &str = "evorag.redb.pre-migration";

fn meta_get(db: &Database, key: &str) -> Option<String> {
    let rtx = db.begin_read().ok()?;
    let t = rtx.open_table(META).ok()?;
    let v = t.get(key.as_bytes()).ok()??;
    Some(String::from_utf8_lossy(v.value()).into_owned())
}

/// 移行を書き終えた (`migration.state == none` の) redb のファイルか。
fn is_complete_db(path: &Path) -> bool {
    path.exists()
        && Database::open(path)
            .ok()
            .and_then(|db| meta_get(&db, meta::MIGRATION_STATE))
            .is_some_and(|s| s == meta::MIGRATION_NONE)
}

/// 移行の跡のファイルで、残っているもの (無ければ `None`)。
pub(crate) fn leftover(root: &Path) -> Option<&'static str> {
    [MIGRATING_FILE, PRE_MIGRATION_FILE]
        .into_iter()
        .find(|f| root.join(f).exists())
}

/// 前の移行が途中で止まった跡を片付ける。データ根を開く前 (ロックを取った後) に呼ぶ。
///
/// 跡のファイルを消すのは、本体が redb として開け、移行を終えた状態 (`none`) と確かめられたとき
/// だけにする。確かめられなければ何も消さない (開くときに止まり、利用者に知らせる)。
pub(crate) fn recover(root: &Path) -> std::io::Result<()> {
    let main = root.join(DB_FILE);
    let migrating = root.join(MIGRATING_FILE);
    let pre = root.join(PRE_MIGRATION_FILE);
    if !main.exists() {
        // 入れ替えの途中 (元を退けた後、移行先を置く前) に止まった。
        if is_complete_db(&migrating) {
            fsutil::rename_durable(&migrating, &main)?;
        } else if pre.exists() {
            fsutil::rename_durable(&pre, &main)?;
        }
    }
    if (migrating.exists() || pre.exists()) && is_complete_db(&main) {
        // 入れ替えの前に止まった移行先と、入れ替えの後に消し損ねた元は要らない (退避はある)。
        for f in [&migrating, &pre] {
            if f.exists() {
                std::fs::remove_file(f)?;
            }
        }
        fsutil::sync_dir(root)?;
    }
    Ok(())
}

/// 移行の結果。
pub(crate) enum Outcome {
    /// 移行した。新しい DB と、移行の前の退避の場所。
    Migrated { db: Database, backup: PathBuf },
    /// 移行しなかった (元の DB のまま)。理由を示して読むだけで開く。
    Failed { db: Database, reason: String },
    /// 入れ替えに失敗し、元の DB も開き直せない。ファイルは消さずに残してある
    /// (03 §8 の「DB のファイルの破損」として扱う)。
    Fatal(String),
}

enum StepError {
    /// 元の DB のまま。
    Kept(Database, String),
    Fatal(String),
}

fn chain<'p>(plan: &'p FormatPlan, from: &Version) -> Option<Vec<&'p Migration>> {
    let target: Version = plan.version.parse().ok()?;
    let mut at = from.clone();
    let mut steps = Vec::new();
    while at < target {
        let m = plan
            .migrations
            .iter()
            .find(|m| m.from.parse::<Version>().is_ok_and(|f| f == at))?;
        let next: Version = m.to.parse().ok()?;
        if next <= at {
            // 版が進まない移行 (定義の誤り)。
            return None;
        }
        at = next;
        steps.push(m);
    }
    (at == target).then_some(steps)
}

/// `db` (形式の版 `found`) を `plan` の版まで移行する。`extra_files` (`config.toml`) は退避に含める。
pub(crate) fn run(
    db: Database,
    root: &Path,
    found: &Version,
    plan: &FormatPlan,
    now_ms: i64,
    extra_files: &[&Path],
) -> Outcome {
    let Some(steps) = chain(plan, found) else {
        return Outcome::Failed {
            db,
            reason: format!("no migration from format {found} to {}", plan.version),
        };
    };
    let backup = match backup::pre_op(
        &db,
        root,
        now_ms,
        "migrate",
        &found.to_string(),
        extra_files,
    ) {
        Ok(dir) => dir,
        Err(e) => {
            return Outcome::Failed {
                db,
                reason: format!("could not back up before migrating: {e}"),
            };
        }
    };
    let mut db = db;
    let last = steps.len() - 1;
    for (i, step) in steps.into_iter().enumerate() {
        let (min_read, min_write) = if i == last {
            (plan.min_read.as_str(), plan.min_write.as_str())
        } else {
            (step.to, step.to)
        };
        match migrate_step(db, root, step, i, min_read, min_write) {
            Ok(next) => db = next,
            Err(StepError::Kept(old, reason)) => {
                let partial = if i > 0 {
                    format!(" (stopped at the intermediate format {})", step.from)
                } else {
                    String::new()
                };
                return Outcome::Failed {
                    db: old,
                    reason: format!(
                        "migration {} -> {} failed{partial}: {reason}",
                        step.from, step.to
                    ),
                };
            }
            Err(StepError::Fatal(reason)) => return Outcome::Fatal(reason),
        }
    }
    Outcome::Migrated { db, backup }
}

fn migrate_step(
    src: Database,
    root: &Path,
    step: &Migration,
    index: usize,
    min_read: &str,
    min_write: &str,
) -> Result<Database, StepError> {
    let path = root.join(MIGRATING_FILE);
    let written = (|| -> Result<(), String> {
        if path.exists() {
            std::fs::remove_file(&path).map_err(err)?;
        }
        fsutil::prepare_private_file(&path).map_err(err)?;
        let dst = Database::create(&path).map_err(err)?;
        let rtx = src.begin_read().map_err(err)?;
        let mut wtx = dst.begin_write().map_err(err)?;
        wtx.set_durability(redb::Durability::Immediate)
            .map_err(err)?;
        for t in TABLES {
            wtx.open_table(t).map_err(err)?;
        }
        let state = format!("in_progress:{}:{}:{index}", step.from, step.to);
        wtx.open_table(META)
            .map_err(err)?
            .insert(meta::MIGRATION_STATE.as_bytes(), state.as_bytes())
            .map_err(err)?;
        let mut ctx = MigrationCtx {
            src: &rtx,
            dst: &wtx,
        };
        (step.run)(&mut ctx)?;
        {
            // 形式の版は移行の関数ではなく、ここで書く。`none` は突き合わせの後に書く。
            let mut m = wtx.open_table(META).map_err(err)?;
            for (k, v) in [
                (meta::FORMAT_VERSION, step.to),
                (meta::FORMAT_MIN_READ, min_read),
                (meta::FORMAT_MIN_WRITE, min_write),
                (meta::MIGRATION_STATE, state.as_str()),
            ] {
                m.insert(k.as_bytes(), v.as_bytes()).map_err(err)?;
            }
        }
        wtx.commit().map_err(err)?;
        verify(&rtx, &dst, step.counts_change)?;
        let mut done = dst.begin_write().map_err(err)?;
        done.set_durability(redb::Durability::Immediate)
            .map_err(err)?;
        done.open_table(META)
            .map_err(err)?
            .insert(
                meta::MIGRATION_STATE.as_bytes(),
                meta::MIGRATION_NONE.as_bytes(),
            )
            .map_err(err)?;
        done.commit().map_err(err)?;
        Ok(())
    })();
    if let Err(reason) = written {
        let _ = std::fs::remove_file(&path);
        return Err(StepError::Kept(src, reason));
    }

    // 入れ替える。元を退けてから移行先を置く (途中で止まっても `recover` が直す)。
    let main = root.join(DB_FILE);
    let pre = root.join(PRE_MIGRATION_FILE);
    drop(src);
    let first = fsutil::rename_durable(&main, &pre);
    let second = first
        .as_ref()
        .map_err(|_| ())
        .ok()
        .map(|()| fsutil::rename_durable(&path, &main));
    // rename は済んだが書き切りで失敗を返したときのため、結果ではなくファイルの状態で判断する。
    let swapped = main.exists() && !path.exists();
    if !swapped {
        // 元に戻す。戻せないときは何も消さずに止める (次の起動の `recover` が直す)。
        if !main.exists() && pre.exists() && fsutil::rename_durable(&pre, &main).is_err() {
            return Err(StepError::Fatal(format!(
                "could not swap the migrated file ({}) nor put the original back; \
                 {PRE_MIGRATION_FILE} and {MIGRATING_FILE} are kept",
                describe(&first, second.as_ref())
            )));
        }
        let _ = std::fs::remove_file(&path);
        return match Database::open(&main) {
            Ok(db) => Err(StepError::Kept(
                db,
                format!(
                    "could not swap the migrated file: {}",
                    describe(&first, second.as_ref())
                ),
            )),
            Err(open) => Err(StepError::Fatal(format!(
                "could not reopen the original database after a failed swap: {open}"
            ))),
        };
    }
    if pre.exists() {
        let _ = std::fs::remove_file(&pre);
        let _ = fsutil::sync_dir(root);
    }
    let _ = fsutil::prepare_private_file(&main);
    Database::open(&main)
        .map_err(|e| StepError::Fatal(format!("the migrated database could not be opened: {e}")))
}

fn describe(first: &std::io::Result<()>, second: Option<&std::io::Result<()>>) -> String {
    match (first, second) {
        (Err(e), _) => format!("moving the original aside failed: {e}"),
        (Ok(()), Some(Err(e))) => format!("putting the migrated file in place failed: {e}"),
        _ => "unknown".into(),
    }
}

/// 表ごとの件数と、記録のチェックサムを突き合わせる (03 §12.1)。
///
/// - `meta` 以外の表は、移行が件数を変えると宣言した表 (`counts_change`) を除き、件数が同じこと。
/// - 移行先で壊れた記録が、元より増えていないこと (元から壊れていた記録は、原文のまま写って隔離を待つ)。
fn verify(
    src: &redb::ReadTransaction,
    dst: &Database,
    counts_change: &[Table],
) -> Result<(), String> {
    fn census(
        t: &impl ReadableTable<&'static [u8], &'static [u8]>,
        check: bool,
    ) -> Result<(u64, u64), String> {
        let (mut n, mut corrupt) = (0, 0);
        for item in t.iter().map_err(err)? {
            let (k, v) = item.map_err(err)?;
            n += 1;
            if check
                && matches!(
                    Envelope::decode(k.value(), v.value()),
                    Err(DecodeError::Corrupt(_))
                )
            {
                corrupt += 1;
            }
        }
        Ok((n, corrupt))
    }
    let rtx = dst.begin_read().map_err(err)?;
    for t in Table::ALL {
        if t == Table::Meta {
            continue;
        }
        let records = t == Table::Records;
        let (n_src, c_src) = census(&src.open_table(t.def()).map_err(err)?, records)?;
        let (n_dst, c_dst) = census(&rtx.open_table(t.def()).map_err(err)?, records)?;
        if n_src != n_dst && !counts_change.contains(&t) {
            return Err(format!("{t:?} row count changed from {n_src} to {n_dst}"));
        }
        if c_dst > c_src {
            return Err(format!("{} migrated record(s) are corrupt", c_dst - c_src));
        }
    }
    Ok(())
}
