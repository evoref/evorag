//! 試験の道具。外部の crate (tempfile) を足さずに、一時フォルダを作って片付ける。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 落とすと消える一時フォルダ。
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "evorag-test-{label}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).expect("create temp dir");
        Self(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 閉じた DB の記録を 1 件、直接消す (試験で「ジャーナルにだけある状態」を作るため)。
pub fn raw_remove_record(data_root: &Path, kind: &str, id: evorag_types::Id) {
    let db = redb::Database::create(data_root.join(crate::DB_FILE)).expect("open db");
    let tx = db.begin_write().expect("begin");
    {
        let mut t = tx.open_table(crate::store::RECORDS).expect("records");
        t.remove(crate::store::record_key(kind, id).as_slice())
            .expect("remove");
    }
    tx.commit().expect("commit");
}

/// 閉じた DB の `meta` の値を直接書く (試験だけ)。
pub fn raw_set_meta(data_root: &Path, key: &str, value: &str) {
    let db = redb::Database::create(data_root.join(crate::DB_FILE)).expect("open db");
    let tx = db.begin_write().expect("begin");
    {
        let mut t = tx.open_table(crate::store::META).expect("meta");
        t.insert(key.as_bytes(), value.as_bytes()).expect("insert");
    }
    tx.commit().expect("commit");
}

/// 閉じた DB の記録 1 件の本体のバイトを書き換える (試験で「壊れた記録」を作るため)。
pub fn raw_corrupt_record(data_root: &Path, kind: &str, id: evorag_types::Id) {
    use redb::ReadableTable;
    let db = redb::Database::create(data_root.join(crate::DB_FILE)).expect("open db");
    let tx = db.begin_write().expect("begin");
    {
        let mut t = tx.open_table(crate::store::RECORDS).expect("records");
        let k = crate::store::record_key(kind, id);
        let mut v = t
            .get(k.as_slice())
            .expect("get")
            .expect("record")
            .value()
            .to_vec();
        let last = v.len() - 2;
        v[last] ^= 0x20;
        t.insert(k.as_slice(), v.as_slice()).expect("insert");
    }
    tx.commit().expect("commit");
}
