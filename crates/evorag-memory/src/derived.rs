//! v0.5 の種類の関係と副索引を、操作の列にする道具 (03 §3.3・§3.4)。
//!
//! 関係と副索引は本体から決まる ([`evorag_store::KindSpec::derive`])。書き換えるときは、前の封筒から作った関係と
//! 副索引を消してから、新しい本体から作り直す (書き手の `Put` は前の関係と副索引を消さないため)。

use evorag_store::key::OwnedPart;
use evorag_store::{BodyError, DeriveFn, Envelope, WriteOp};
use evorag_types::{Id, Timestamp};
use serde::Serialize;

pub(crate) fn link(from: Id, rel: &str, to: Id) -> WriteOp {
    WriteOp::PutLink {
        from,
        rel: rel.into(),
        to,
        attrs: Vec::new(),
    }
}

pub(crate) fn lookup(kind: &str, field: &str, value: OwnedPart, id: Id) -> WriteOp {
    WriteOp::PutLookup {
        kind: kind.into(),
        field: field.into(),
        value,
        id,
    }
}

/// `derive` が作った関係と副索引を消す操作。
pub(crate) fn undo(ops: Vec<WriteOp>) -> impl Iterator<Item = WriteOp> {
    ops.into_iter().filter_map(|op| match op {
        WriteOp::PutLink { from, rel, to, .. } => Some(WriteOp::DeleteLink { from, rel, to }),
        WriteOp::PutLookup {
            kind,
            field,
            value,
            id,
        } => Some(WriteOp::DeleteLookup {
            kind,
            field,
            value,
            id,
        }),
        _ => None,
    })
}

/// 記録 1 件を書く操作 (封筒と、その関係・副索引)。`previous` (読み直した前の封筒) があれば、その上に書く: 作成の
/// 時刻と知らない項目 (03 §4.2 の規則 7) を残し、前の関係と副索引を消してから作り直す。
pub(crate) struct Record<'a, T> {
    pub kind: &'a str,
    pub v: u32,
    pub id: Id,
    pub now: Timestamp,
    pub body: &'a T,
    pub derive: DeriveFn,
}

impl<T: Serialize> Record<'_, T> {
    pub(crate) fn ops(&self, previous: Option<&Envelope>) -> Result<Vec<WriteOp>, BodyError> {
        let mut ops = Vec::new();
        let mut env = match previous {
            Some(old) => {
                ops.extend(undo((self.derive)(old)));
                let mut e = old.clone();
                e.updated_at = self.now;
                e
            }
            None => Envelope::new(self.id, self.kind, self.v, self.now, ""),
        };
        env.set_body(self.body)?;
        let derived = (self.derive)(&env);
        ops.push(WriteOp::Put(env));
        ops.extend(derived);
        Ok(ops)
    }
}

/// 記録を消す操作 (封筒と、その関係・副索引)。
pub(crate) fn delete_ops(env: &Envelope, derive: DeriveFn) -> Vec<WriteOp> {
    let mut ops: Vec<WriteOp> = undo(derive(env)).collect();
    ops.push(WriteOp::Delete {
        kind: env.kind.clone(),
        id: env.id,
    });
    ops.push(WriteOp::DeleteLinksOf(env.id));
    ops
}
