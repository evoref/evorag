//! 書き手 (02 §5)。書き込みの入口は 1 つで、`WriteToken` は書き手だけが持つ。
//!
//! - チャット経路と sleep-time は、書き込みの依頼 (型付きの操作の列) を書き手に送り、結果を待つ。
//! - 会話の追記の依頼 (`Priority::Conversation`) を、裏の処理の結果より先に処理する (2 本の待ち行列)。
//! - 1 つの依頼は 1 つのトランザクションで書く (記録とその関係・副索引を同じトランザクションで書く、03 §3.4)。
//! - `Durability::None` で書いた後は、手が空いたときに区切りの `Immediate` のコミットを 1 回入れる。
//! - 終了の合図を受けたら、新しい依頼を断り、会話の追記を書き切ってから終わる。裏の処理の依頼は捨てる。
//!
//! スレッドを作るのは `evorag-core` (02 §4.3 の R7)。この crate は、そのスレッドで回す `Writer::run` を持つ。

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use evorag_types::Id;

use crate::envelope::Envelope;
use crate::key::{OwnedPart, Part};
use crate::store::{Durability, Store, StoreError, WriteToken, WriteTxn};

/// 依頼の優先度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// 会話の追記と利用者の明示の操作。
    Conversation,
    /// 裏の処理の結果 (job から作り直せる)。
    Background,
}

/// 1 つの書き込みの操作。
#[derive(Debug, Clone)]
pub enum WriteOp {
    Put(Envelope),
    Delete {
        kind: String,
        id: Id,
    },
    PutLink {
        from: Id,
        rel: String,
        to: Id,
        attrs: Vec<u8>,
    },
    DeleteLink {
        from: Id,
        rel: String,
        to: Id,
    },
    DeleteLinksOf(Id),
    PutLookup {
        kind: String,
        field: String,
        value: OwnedPart,
        id: Id,
    },
    DeleteLookup {
        kind: String,
        field: String,
        value: OwnedPart,
        id: Id,
    },
    PutBlob {
        id: Id,
        part: u32,
        bytes: Vec<u8>,
    },
    DeleteBlobs(Id),
    /// 一度だけ書く `meta` の値 (`WriteTxn::set_meta_once`)。
    SetMetaOnce {
        key: String,
        value: String,
    },
}

/// 書き込みの依頼。`ops` は 1 つのトランザクションで、すべて書くか、何も書かない。
#[derive(Debug, Clone)]
pub struct WriteRequest {
    pub ops: Vec<WriteOp>,
    pub durability: Durability,
    pub priority: Priority,
}

impl WriteRequest {
    /// 会話の追記 (`Immediate`)。
    pub fn conversation(ops: Vec<WriteOp>) -> Self {
        Self {
            ops,
            durability: Durability::Immediate,
            priority: Priority::Conversation,
        }
    }

    /// 裏の処理の結果 (`None`)。
    pub fn background(ops: Vec<WriteOp>) -> Self {
        Self {
            ops,
            durability: Durability::None,
            priority: Priority::Background,
        }
    }
}

/// 書いた結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteOutcome {
    /// `Put` で書いた記録の ID (操作の順)。
    pub put: Vec<Id>,
    /// 書き込みの前に quarantine へ移した記録の数 (03 §8)。
    pub quarantined: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error(transparent)]
    Store(#[from] StoreError),
    /// 終了の合図の後の依頼、または終了で捨てた裏の処理の依頼。
    #[error("the writer is shutting down")]
    ShuttingDown,
}

/// 結果を受け取る関数。書き手のスレッドで呼ばれるので、軽い処理 (チャネルへの送信など) だけにする。
pub type Reply = Box<dyn FnOnce(Result<WriteOutcome, WriteError>) + Send>;

struct Pending {
    req: WriteRequest,
    reply: Reply,
}

#[derive(Default)]
struct Queues {
    conversation: VecDeque<Pending>,
    background: VecDeque<Pending>,
    closing: bool,
}

struct Shared {
    queues: Mutex<Queues>,
    ready: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queues> {
        self.queues.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 書き手。`run` を専用のスレッドで回す。
pub struct Writer {
    store: Arc<Store>,
    token: WriteToken,
    shared: Arc<Shared>,
}

/// 依頼を送る口。複製して配ってよい。
#[derive(Clone)]
pub struct WriterHandle {
    shared: Arc<Shared>,
}

impl Writer {
    /// ストアの書き込みの権利を受け取って書き手を作る。権利がもう渡っているとき・読むだけで
    /// 開いているときは作らない。
    pub fn new(store: Arc<Store>) -> Result<(Self, WriterHandle), StoreError> {
        if let Some(r) = store.read_only() {
            return Err(StoreError::ReadOnly(r.clone()));
        }
        let token = store.take_write_token().ok_or(StoreError::TokenTaken)?;
        let shared = Arc::new(Shared {
            queues: Mutex::default(),
            ready: Condvar::new(),
        });
        Ok((
            Self {
                store,
                token,
                shared: shared.clone(),
            },
            WriterHandle { shared },
        ))
    }

    /// 依頼を処理し続ける。終了の合図の後、会話の依頼を書き切ってから戻る。
    pub fn run(self) {
        // `None` で書いた後、まだ `Immediate` のコミットをしていない。
        let mut unflushed = false;
        loop {
            enum Next {
                Apply(Pending),
                Checkpoint,
                Stop(Vec<Pending>),
            }
            let next = {
                let mut q = self.shared.lock();
                while q.conversation.is_empty()
                    && q.background.is_empty()
                    && !q.closing
                    && !unflushed
                {
                    q = self.shared.ready.wait(q).unwrap_or_else(|e| e.into_inner());
                }
                if let Some(p) = q.conversation.pop_front() {
                    Next::Apply(p)
                } else if q.closing {
                    // 裏の処理の依頼は捨てる (job から再開できる、02 §5)。
                    Next::Stop(q.background.drain(..).collect())
                } else if let Some(p) = q.background.pop_front() {
                    Next::Apply(p)
                } else {
                    Next::Checkpoint
                }
            };
            match next {
                Next::Apply(p) => {
                    let result = self.apply(&p.req);
                    if result.is_ok() {
                        // `Immediate` のコミットは、それより前の `None` のコミットも書き切る。
                        unflushed = p.req.durability == Durability::None;
                    }
                    (p.reply)(result);
                }
                Next::Checkpoint => {
                    // 手が空いた。`None` で書いたものを書き切る (03 §3.4)。失敗しても次の依頼で
                    // 書き切られるので、ここでは繰り返さない。
                    let _ = self.checkpoint();
                    unflushed = false;
                }
                Next::Stop(dropped) => {
                    for p in dropped {
                        (p.reply)(Err(WriteError::ShuttingDown));
                    }
                    if unflushed {
                        let _ = self.checkpoint();
                    }
                    return;
                }
            }
        }
    }

    fn checkpoint(&self) -> Result<(), StoreError> {
        self.store
            .write(&self.token, Durability::Immediate)?
            .commit()
    }

    fn apply(&self, req: &WriteRequest) -> Result<WriteOutcome, WriteError> {
        let mut tx = self.store.write(&self.token, req.durability)?;
        let mut out = WriteOutcome::default();
        if self.store.suspect_count() > 0 {
            out.quarantined = tx.quarantine_suspects()?;
        }
        for op in &req.ops {
            apply_op(&mut tx, op, &mut out)?;
        }
        tx.commit()?;
        Ok(out)
    }
}

fn part(p: &OwnedPart) -> Part<'_> {
    match p {
        OwnedPart::Str(s) => Part::Str(s),
        OwnedPart::Bytes(b) => Part::Bytes(b),
        OwnedPart::Int(i) => Part::Int(*i),
        OwnedPart::Id(id) => Part::Id(*id),
    }
}

fn apply_op(tx: &mut WriteTxn<'_>, op: &WriteOp, out: &mut WriteOutcome) -> Result<(), StoreError> {
    match op {
        WriteOp::Put(env) => {
            let mut env = env.clone();
            tx.put(&mut env)?;
            out.put.push(env.id);
        }
        WriteOp::Delete { kind, id } => {
            tx.delete(kind, *id)?;
        }
        WriteOp::PutLink {
            from,
            rel,
            to,
            attrs,
        } => tx.put_link(*from, rel, *to, attrs)?,
        WriteOp::DeleteLink { from, rel, to } => {
            tx.delete_link(*from, rel, *to)?;
        }
        WriteOp::DeleteLinksOf(id) => {
            tx.delete_links_of(*id)?;
        }
        WriteOp::PutLookup {
            kind,
            field,
            value,
            id,
        } => {
            tx.put_lookup(kind, field, &part(value), *id)?;
        }
        WriteOp::DeleteLookup {
            kind,
            field,
            value,
            id,
        } => {
            tx.delete_lookup(kind, field, &part(value), *id)?;
        }
        WriteOp::PutBlob { id, part, bytes } => tx.put_blob(*id, *part, bytes)?,
        WriteOp::DeleteBlobs(id) => {
            tx.delete_blobs(*id)?;
        }
        WriteOp::SetMetaOnce { key, value } => tx.set_meta_once(key, value)?,
    }
    Ok(())
}

impl WriterHandle {
    /// 依頼を送る。終了の合図の後は断る (`reply` は呼ばない)。
    pub fn submit(&self, req: WriteRequest, reply: Reply) -> Result<(), WriteError> {
        let mut q = self.shared.lock();
        if q.closing {
            return Err(WriteError::ShuttingDown);
        }
        let p = Pending { req, reply };
        match p.req.priority {
            Priority::Conversation => q.conversation.push_back(p),
            Priority::Background => q.background.push_back(p),
        }
        drop(q);
        self.shared.ready.notify_one();
        Ok(())
    }

    /// 依頼を送り、結果が出るまで待つ (同期の呼び出し側と試験のため)。
    pub fn submit_wait(&self, req: WriteRequest) -> Result<WriteOutcome, WriteError> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.submit(
            req,
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        )?;
        rx.recv().unwrap_or(Err(WriteError::ShuttingDown))
    }

    /// 終了の合図。新しい依頼を断り、待っている会話の依頼を書き切ったら `run` が戻る。
    pub fn shutdown(&self) {
        self.shared.lock().closing = true;
        self.shared.ready.notify_all();
    }
}
