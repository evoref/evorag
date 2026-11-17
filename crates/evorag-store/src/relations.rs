//! 関係 (`links`)・副索引 (`lookup`)・大きなデータ (`blobs`) の表 (03 §3.1)。
//!
//! - `links`: (from, rel, to) → 属性。逆引き用に (to, `~`rel, from) も同じトランザクションで書く。
//! - `lookup`: (kind, field, 値, id) → 空。
//! - `blobs`: (id, part) → バイト列。

use evorag_types::Id;
use redb::ReadableTable;

use crate::key::{self, KeyBuf, OwnedPart, Part};
use crate::store::{BLOBS, LINKS, LOOKUP, ReadTxn, StoreError, WriteTxn, db};

/// 逆向きの関係の名前の前に付ける印。関係の名前はこの文字で始めない。
const REVERSE: char = '~';

/// 関係の名前が正しくない (空、または `~` で始まる)。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid relation name `{0}`")]
pub struct BadRelation(pub String);

fn check_rel(rel: &str) -> Result<(), StoreError> {
    if rel.is_empty() || rel.starts_with(REVERSE) {
        return Err(StoreError::BadRelation(BadRelation(rel.to_owned())));
    }
    Ok(())
}

fn link_key(a: Id, rel: &str, b: Id) -> Vec<u8> {
    let mut k = KeyBuf::new();
    k.id(a).str(rel).id(b);
    k.into_bytes()
}

fn link_prefix(a: Id, rel: &str) -> Vec<u8> {
    let mut k = KeyBuf::new();
    k.id(a).str(rel);
    k.into_bytes()
}

fn reverse(rel: &str) -> String {
    format!("{REVERSE}{rel}")
}

fn lookup_key(kind: &str, field: &str, value: &Part<'_>, id: Id) -> Vec<u8> {
    let mut k = KeyBuf::new();
    k.str(kind).str(field).push(value).id(id);
    k.into_bytes()
}

fn lookup_prefix(kind: &str, field: &str) -> KeyBuf {
    let mut k = KeyBuf::new();
    k.str(kind).str(field);
    k
}

fn blob_key(id: Id, part: u32) -> Vec<u8> {
    let mut k = KeyBuf::new();
    k.id(id).int(i64::from(part));
    k.into_bytes()
}

fn id_prefix(id: Id) -> Vec<u8> {
    let mut k = KeyBuf::new();
    k.id(id);
    k.into_bytes()
}

/// 副索引の走査の範囲。
#[derive(Debug, Clone)]
pub enum LookupRange<'a> {
    /// 値が一致するもの。
    Eq(Part<'a>),
    /// 文字列の値がこの文字列で始まるもの。
    StrPrefix(&'a str),
    /// 値が `from` 以上・`to` 未満のもの (`None` は端なし)。
    Between {
        from: Option<Part<'a>>,
        to: Option<Part<'a>>,
    },
}

/// 副索引の 1 つの項目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupEntry {
    pub value: OwnedPart,
    pub id: Id,
}

fn bounds(kind: &str, field: &str, range: &LookupRange<'_>) -> (Vec<u8>, Option<Vec<u8>>) {
    let base = lookup_prefix(kind, field);
    let field_end = key::prefix_end(base.as_bytes());
    match range {
        LookupRange::Eq(v) => {
            let mut k = base;
            k.push(v);
            let start = k.into_bytes();
            let end = key::prefix_end(&start);
            (start, end)
        }
        LookupRange::StrPrefix(p) => {
            let mut k = base;
            k.str_prefix(p);
            let start = k.into_bytes();
            let end = key::prefix_end(&start);
            (start, end)
        }
        LookupRange::Between { from, to } => {
            let start = match from {
                Some(v) => {
                    let mut k = base.clone();
                    k.push(v);
                    k.into_bytes()
                }
                None => base.as_bytes().to_vec(),
            };
            let end = match to {
                Some(v) => {
                    let mut k = base;
                    k.push(v);
                    Some(k.into_bytes())
                }
                None => field_end,
            };
            (start, end)
        }
    }
}

fn parse_link(k: &[u8]) -> Option<(Id, Vec<u8>)> {
    match key::decode(k).ok()?.as_slice() {
        [OwnedPart::Id(_), OwnedPart::Str(_), OwnedPart::Id(b)] => Some((*b, Vec::new())),
        _ => None,
    }
}

fn parse_lookup(k: &[u8]) -> Option<LookupEntry> {
    let mut parts = key::decode(k).ok()?;
    if parts.len() != 4 {
        return None;
    }
    let OwnedPart::Id(id) = parts.pop()? else {
        return None;
    };
    Some(LookupEntry {
        value: parts.pop()?,
        id,
    })
}

/// 読み取りと書き込みのトランザクションで共通の読み方。
macro_rules! relation_reads {
    () => {
        /// `from` から `rel` でつながる記録の ID と属性 (ID の順)。
        pub fn links_from(&self, from: Id, rel: &str) -> Result<Vec<(Id, Vec<u8>)>, StoreError> {
            check_rel(rel)?;
            self.scan_links(from, rel)
        }

        /// `to` へ `rel` でつながる記録の ID と属性 (ID の順)。
        pub fn links_to(&self, to: Id, rel: &str) -> Result<Vec<(Id, Vec<u8>)>, StoreError> {
            check_rel(rel)?;
            self.scan_links(to, &reverse(rel))
        }

        fn scan_links(&self, a: Id, rel: &str) -> Result<Vec<(Id, Vec<u8>)>, StoreError> {
            let table = self.tx.open_table(LINKS).map_err(db)?;
            let start = link_prefix(a, rel);
            let end = key::prefix_end(&start).expect("an id prefix is never all 0xFF");
            let mut out = Vec::new();
            for item in table
                .range::<&[u8]>(start.as_slice()..end.as_slice())
                .map_err(db)?
            {
                let (k, v) = item.map_err(db)?;
                if let Some((b, _)) = parse_link(k.value()) {
                    out.push((b, v.value().to_vec()));
                }
            }
            Ok(out)
        }

        /// 副索引を走査する。`reverse` で逆の順、`limit` で件数を限る。
        pub fn lookup(
            &self,
            kind: &str,
            field: &str,
            range: &LookupRange<'_>,
            reverse: bool,
            limit: Option<usize>,
        ) -> Result<Vec<LookupEntry>, StoreError> {
            let table = self.tx.open_table(LOOKUP).map_err(db)?;
            let (start, end) = bounds(kind, field, range);
            let iter = match &end {
                Some(e) => table.range::<&[u8]>(start.as_slice()..e.as_slice()),
                None => table.range::<&[u8]>(start.as_slice()..),
            }
            .map_err(db)?;
            let limit = limit.unwrap_or(usize::MAX);
            let mut out = Vec::new();
            let mut take = |item: Result<
                (redb::AccessGuard<'_, &[u8]>, redb::AccessGuard<'_, &[u8]>),
                redb::StorageError,
            >|
             -> Result<bool, StoreError> {
                let (k, _) = item.map_err(db)?;
                if let Some(e) = parse_lookup(k.value()) {
                    out.push(e);
                }
                Ok(out.len() < limit)
            };
            if reverse {
                for item in iter.rev() {
                    if !take(item)? {
                        break;
                    }
                }
            } else {
                for item in iter {
                    if !take(item)? {
                        break;
                    }
                }
            }
            Ok(out)
        }

        /// 大きなデータの 1 つの部分。
        pub fn blob(&self, id: Id, part: u32) -> Result<Option<Vec<u8>>, StoreError> {
            let table = self.tx.open_table(BLOBS).map_err(db)?;
            let k = blob_key(id, part);
            Ok(table
                .get(k.as_slice())
                .map_err(db)?
                .map(|v| v.value().to_vec()))
        }
    };
}

impl ReadTxn<'_> {
    relation_reads!();
}

impl WriteTxn<'_> {
    relation_reads!();

    /// 関係を書く。逆引きの項目も同じトランザクションで書く。
    pub fn put_link(
        &mut self,
        from: Id,
        rel: &str,
        to: Id,
        attrs: &[u8],
    ) -> Result<(), StoreError> {
        check_rel(rel)?;
        let mut t = self.tx.open_table(LINKS).map_err(db)?;
        t.insert(link_key(from, rel, to).as_slice(), attrs)
            .map_err(db)?;
        t.insert(link_key(to, &reverse(rel), from).as_slice(), attrs)
            .map_err(db)?;
        Ok(())
    }

    /// 関係を消す (逆引きの項目も)。
    pub fn delete_link(&mut self, from: Id, rel: &str, to: Id) -> Result<bool, StoreError> {
        check_rel(rel)?;
        let mut t = self.tx.open_table(LINKS).map_err(db)?;
        let a = t
            .remove(link_key(from, rel, to).as_slice())
            .map_err(db)?
            .is_some();
        let b = t
            .remove(link_key(to, &reverse(rel), from).as_slice())
            .map_err(db)?
            .is_some();
        Ok(a || b)
    }

    /// 記録 `id` から出る関係と、`id` へ入る関係をすべて消す (記録を消すとき)。
    pub fn delete_links_of(&mut self, id: Id) -> Result<usize, StoreError> {
        let mut t = self.tx.open_table(LINKS).map_err(db)?;
        let start = id_prefix(id);
        let end = key::prefix_end(&start).expect("an id prefix is never all 0xFF");
        let mut keys = Vec::new();
        for item in t
            .range::<&[u8]>(start.as_slice()..end.as_slice())
            .map_err(db)?
        {
            let (k, _) = item.map_err(db)?;
            keys.push(k.value().to_vec());
        }
        let mut n = 0;
        for k in keys {
            // (id, rel, other) の対の (other, rel⁻¹, id) も消す。
            if let Ok(parts) = key::decode(&k)
                && let [OwnedPart::Id(_), OwnedPart::Str(rel), OwnedPart::Id(other)] =
                    parts.as_slice()
            {
                let partner = match rel.strip_prefix(REVERSE) {
                    Some(fwd) => link_key(*other, fwd, id),
                    None => link_key(*other, &reverse(rel), id),
                };
                t.remove(partner.as_slice()).map_err(db)?;
            }
            t.remove(k.as_slice()).map_err(db)?;
            n += 1;
        }
        Ok(n)
    }

    /// 副索引の項目を書く。
    pub fn put_lookup(
        &mut self,
        kind: &str,
        field: &str,
        value: &Part<'_>,
        id: Id,
    ) -> Result<(), StoreError> {
        let mut t = self.tx.open_table(LOOKUP).map_err(db)?;
        t.insert(lookup_key(kind, field, value, id).as_slice(), [].as_slice())
            .map_err(db)?;
        Ok(())
    }

    /// 副索引の項目を消す。
    pub fn delete_lookup(
        &mut self,
        kind: &str,
        field: &str,
        value: &Part<'_>,
        id: Id,
    ) -> Result<bool, StoreError> {
        let mut t = self.tx.open_table(LOOKUP).map_err(db)?;
        Ok(t.remove(lookup_key(kind, field, value, id).as_slice())
            .map_err(db)?
            .is_some())
    }

    /// 大きなデータの 1 つの部分を書く。
    pub fn put_blob(&mut self, id: Id, part: u32, bytes: &[u8]) -> Result<(), StoreError> {
        let mut t = self.tx.open_table(BLOBS).map_err(db)?;
        t.insert(blob_key(id, part).as_slice(), bytes).map_err(db)?;
        Ok(())
    }

    /// 記録 `id` の大きなデータをすべて消す。
    pub fn delete_blobs(&mut self, id: Id) -> Result<usize, StoreError> {
        let mut t = self.tx.open_table(BLOBS).map_err(db)?;
        let start = id_prefix(id);
        let end = key::prefix_end(&start).expect("an id prefix is never all 0xFF");
        let mut keys = Vec::new();
        for item in t
            .range::<&[u8]>(start.as_slice()..end.as_slice())
            .map_err(db)?
        {
            keys.push(item.map_err(db)?.0.value().to_vec());
        }
        for k in &keys {
            t.remove(k.as_slice()).map_err(db)?;
        }
        Ok(keys.len())
    }
}
