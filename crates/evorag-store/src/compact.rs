//! 次の起動で compact が要ることの印 (`meta` の `compact.pending`、03 §3.1.1・§8)。
//!
//! 削除・忘れる・忘れるの当て直し・開くときのジャーナルの削除の行・規則 12 の突き合わせで記録を消した・書き換えた
//! コミットで立て、compact を終えた後に消す。書き換えてよいキーで、ジャーナルには書かない。

use crate::meta;
use crate::store::{META, Store, StoreError, WriteTxn, db};

/// 立てたときの値。
const PENDING: &str = "1";

impl Store {
    /// 次の起動で compact が要るか。無い・知らない値は「要らない」(03 §3.1.1)。
    ///
    /// # Errors
    /// `meta` を読めないとき。
    pub fn compact_pending(&self) -> Result<bool, StoreError> {
        Ok(self.meta(meta::COMPACT_PENDING)?.as_deref() == Some(PENDING))
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
