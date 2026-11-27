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

/// 閉じた DB の `meta` の値を直接消す (試験で「古い版が取り込まなかった `meta` の行」を作るため)。
pub fn raw_remove_meta(data_root: &Path, key: &str) {
    let db = redb::Database::create(data_root.join(crate::DB_FILE)).expect("open db");
    let tx = db.begin_write().expect("begin");
    {
        let mut t = tx.open_table(crate::store::META).expect("meta");
        t.remove(key.as_bytes()).expect("remove");
    }
    tx.commit().expect("commit");
}

/// 閉じた DB に封筒を直接書く (チェックサムは正しく付ける。ジャーナルにも関係にも書かない。試験で、書き手を
/// 通さずに記録を変えた根を作るため)。
pub fn raw_put_envelope(data_root: &Path, env: &crate::Envelope) {
    let db = redb::Database::create(data_root.join(crate::DB_FILE)).expect("open db");
    let tx = db.begin_write().expect("begin");
    {
        let mut t = tx.open_table(crate::store::RECORDS).expect("records");
        let k = crate::store::record_key(&env.kind, env.id);
        t.insert(k.as_slice(), env.encode(&k).as_slice())
            .expect("insert");
    }
    tx.commit().expect("commit");
}

/// 閉じた DB に関係 (順向きと逆向きの行) を直接書く (試験だけ)。
pub fn raw_put_link(data_root: &Path, from: evorag_types::Id, rel: &str, to: evorag_types::Id) {
    let key = |a, r: &str, b| {
        let mut k = crate::key::KeyBuf::new();
        k.id(a).str(r).id(b);
        k.into_bytes()
    };
    let db = redb::Database::create(data_root.join(crate::DB_FILE)).expect("open db");
    let tx = db.begin_write().expect("begin");
    {
        let mut t = tx.open_table(crate::store::LINKS).expect("links");
        let reverse = format!("{}{rel}", crate::relations::REVERSE);
        t.insert(key(from, rel, to).as_slice(), [].as_slice())
            .expect("insert");
        t.insert(key(to, &reverse, from).as_slice(), [].as_slice())
            .expect("insert");
    }
    tx.commit().expect("commit");
}

/// 閉じた DB に副索引の項目 (文字列の値) を直接書く (試験だけ)。
pub fn raw_put_lookup(
    data_root: &Path,
    kind: &str,
    field: &str,
    value: &str,
    id: evorag_types::Id,
) {
    let mut k = crate::key::KeyBuf::new();
    k.str(kind).str(field).str(value).id(id);
    let db = redb::Database::create(data_root.join(crate::DB_FILE)).expect("open db");
    let tx = db.begin_write().expect("begin");
    {
        let mut t = tx.open_table(crate::store::LOOKUP).expect("lookup");
        t.insert(k.as_bytes(), [].as_slice()).expect("insert");
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

/// 閉じた DB の、種類 `kind` の副索引の項目をすべて直接消す (試験で「失った副索引」を作るため)。消した数を返す。
pub fn raw_remove_lookups(data_root: &Path, kind: &str) -> usize {
    use redb::ReadableTable;
    let mut prefix = crate::key::KeyBuf::new();
    prefix.str(kind);
    let prefix = prefix.into_bytes();
    let db = redb::Database::create(data_root.join(crate::DB_FILE)).expect("open db");
    let tx = db.begin_write().expect("begin");
    let n = {
        let mut t = tx.open_table(crate::store::LOOKUP).expect("lookup");
        let keys: Vec<Vec<u8>> = t
            .iter()
            .expect("iter")
            .map(|item| item.expect("row").0.value().to_vec())
            .filter(|k| k.starts_with(&prefix))
            .collect();
        for k in &keys {
            t.remove(k.as_slice()).expect("remove");
        }
        keys.len()
    };
    tx.commit().expect("commit");
    n
}

/// 閉じた DB の、`from` から関係 `rel` の行 (順向きと逆向き) を直接消す (試験で「失った関係」を作るため)。消した数を
/// 返す。
pub fn raw_remove_links(data_root: &Path, from: evorag_types::Id, rel: &str) -> usize {
    use redb::ReadableTable;

    use crate::key::OwnedPart;
    let reverse = format!("{}{rel}", crate::relations::REVERSE);
    let db = redb::Database::create(data_root.join(crate::DB_FILE)).expect("open db");
    let tx = db.begin_write().expect("begin");
    let n = {
        let mut t = tx.open_table(crate::store::LINKS).expect("links");
        let keys: Vec<Vec<u8>> = t
            .iter()
            .expect("iter")
            .map(|item| item.expect("row").0.value().to_vec())
            .filter(|k| {
                matches!(
                    crate::key::decode(k).ok().as_deref(),
                    Some([OwnedPart::Id(a), OwnedPart::Str(r), OwnedPart::Id(b)])
                        if (*a == from && r == rel) || (*b == from && *r == reverse)
                )
            })
            .collect();
        for k in &keys {
            t.remove(k.as_slice()).expect("remove");
        }
        keys.len()
    };
    tx.commit().expect("commit");
    n
}
