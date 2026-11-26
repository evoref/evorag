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

use evorag_types::{Id, Timestamp};

use std::collections::{BTreeMap, HashSet};

use crate::envelope::{BodyPatch, Envelope};
use crate::journal::{JournalCut, LineOp, RewriteError, RewriteReport};
use crate::key::{OwnedPart, Part};
use crate::store::{Durability, Read, Store, StoreError, WriteToken, WriteTxn};

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
    /// 記録をこのトランザクションの中で読み直し、本体の項目を差し替えて書く (外で読んだ封筒を丸ごと書き戻さない、
    /// 04 §7.3・§7.7 の 3)。`updated_at` があれば更新時刻も変える。記録が無い・読めなければ、依頼の全体を断る
    /// ([`StoreError::Stale`])。本体が map でなければ [`StoreError::BadBody`]。関係と副索引は、読み直した封筒から
    /// 作り直す (種類の `derive`)。ジャーナルに書く種類なら、差し替えた封筒の `put` の行を書く。
    Patch {
        kind: String,
        id: Id,
        fields: BodyPatch,
        updated_at: Option<Timestamp>,
    },
    /// 依頼の前提: 記録が読める形であり、`field` があれば本体のその整数の項目 (無ければ 0) が値と等しい。
    /// 合わなければ依頼の全体を断る ([`StoreError::Stale`])。派生の記録を書く前に、出典が残っていて改訂の
    /// 番号が読んだときと合うことを、書き手のトランザクションの中で確かめるのに使う (04 §7.7 の 4)。
    Expect {
        kind: String,
        id: Id,
        field: Option<(String, u64)>,
    },
    /// 依頼の前提: 記録 `kind`/`id` の本体が、外で読んだときの本体 (`body` は [`Envelope::body_digest`]) と同じ。
    /// `body` が `None` なら、記録が無いこと。合わなければ依頼の全体を断る ([`StoreError::Stale`])。外で読んだ記録の
    /// 上に書くときに、その間の書き換え (閉じた区切りなど) を古い本体で上書きしないために使う (04 §2)。
    ExpectBody {
        kind: String,
        id: Id,
        body: Option<[u8; 32]>,
    },
    /// 開くときに残した「索引と退避から除く ID」([`Store::pending_deletes`]) を消す。整理の job を書くのと同じ
    /// 依頼に入れる。
    ClearPendingDeletes,
    /// 次の起動で compact が要ることを書く (`meta` の `compact.pending`、03 §3.1.1)。記録を消した・書き換えた
    /// 依頼 (規則 12 の突き合わせなど) に入れる。ジャーナルに書く種類の記録を `Delete` で消した依頼と、`Put`・
    /// `Patch` で単調に増える項目 (ターンの改訂の番号) を上げた依頼 (伏せ字) には、書き手が自動で足す。印は
    /// compact ([`Store::compact_if_pending`]) が消す。
    SetCompactPending,
    /// 起動時の退避の写し直しの job の作り直しを済ませた (`meta` の `forget.recheck` を消す)。job を書くのと
    /// 同じ依頼に入れる。
    ClearForgetRecheck,
    /// コミットの後に、ジャーナルから ID ごとの `put` の行を除く書き直し (03 §6 の「削除と忘れる」)。DB には何も
    /// 書かない。忘れるでは、指示の `put` と伏せ字にしたターンの `Patch` と同じ依頼に入れる (伏せ字の後の行を
    /// 追記してから書き直す)。sleep-time の試し直しと起動時の当て直しは、この操作だけの依頼で送る (そのときは
    /// DB のトランザクションを開かない)。書き直しの成否は [`WriteOutcome::journal_rewrite`] に返し、失敗しても
    /// 依頼は断らない (コミットは済んでいる)。`Delete` で消すジャーナルに書く種類の ID には、
    /// [`JournalCut::All`] を自動で足す。
    RewriteJournal(BTreeMap<Id, JournalCut>),
    /// この依頼のジャーナルの行を、1 つの操作を成す組の行にする (03 §6 の「組の行」、v0.5。v0.5 の根でだけ組にし、
    /// ほかの根では何もしない)。忘れる (`forget` の行
    /// → `end` の行 → 伏せ字にしたターンの行) と削除 (`delete` の行 → `end`・`private` の行) の依頼に入れる。取り込みは、
    /// 組の行がすべてそろったときだけ組を当てる。DB には何も書かない。
    GroupJournalLines,
}

/// 書き込みの依頼。`ops` は 1 つのトランザクションで、すべて書くか、何も書かない。
///
/// ジャーナルに書く行は、`ops` の順に、1 回の追記で、続いた連番で書く。忘れるでは、指示 (`fact_directive`) の
/// `Put` を、伏せ字にするターンの `Patch` より前に置く (指示の行を先に書く、03 §6 の「書く物」)。
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

    /// 裏の処理の結果のうち、作り直せないもの (`Immediate`)。会話の追記より後に回すが、`None` のコミットにまとめず、
    /// 1 つの依頼を書き切る。v0.5 の台帳と閉じた区切り (03 §3.4 の「台帳」)。
    pub fn background_durable(ops: Vec<WriteOp>) -> Self {
        Self {
            ops,
            durability: Durability::Immediate,
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
    /// quarantine へ移せなかった理由 (依頼は続けて書いた)。呼び手が知らせる。
    pub quarantine_failed: Option<String>,
    /// コミットの後のジャーナルの書き直し (削除と [`WriteOp::RewriteJournal`]) の結果。書き直しが無ければ
    /// `None`。失敗は依頼の失敗にしない。呼び手が job に残すか試し直す ([`RewriteError::is_swap`]、03 §6)。
    pub journal_rewrite: Option<Result<RewriteReport, RewriteError>>,
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
    /// ジャーナルに書いた後でコミットに失敗した (以後は書かない)。
    poisoned: std::cell::Cell<bool>,
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
                poisoned: std::cell::Cell::new(false),
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
        if self.poisoned.get() {
            // ジャーナルに書いた後でコミットに失敗した。取り込み直すまで書かない (03 §6)。
            return Err(StoreError::NeedsRestart.into());
        }
        let mut out = WriteOutcome::default();
        let mut cuts = requested_cuts(&req.ops);
        if !req.ops.is_empty()
            && req
                .ops
                .iter()
                .all(|op| matches!(op, WriteOp::RewriteJournal(_)))
        {
            // 書き直しだけの依頼 (試し直し・起動時の当て直し)。DB には書かない。
            out.journal_rewrite = Some(self.rewrite_journal(&cuts));
            return Ok(out);
        }
        // 1. DB のトランザクションで操作を当てて、規則を確かめる (断られたらジャーナルにも書かない)。
        if self.store.suspect_count() > 0 {
            self.quarantine(req.durability, &mut out);
        }
        let mut tx = self.store.write(&self.token, req.durability)?;
        // 前に消した記録を作り直さない (04 §7.7 の 3)。同じ依頼の中で消してから作っても断る。
        let mut gone: HashSet<Id> = HashSet::new();
        let mut patched = Vec::new();
        for op in &req.ops {
            match op {
                WriteOp::Put(Envelope { kind, id, .. }) | WriteOp::Patch { kind, id, .. }
                    if self.store.was_deleted(*id) =>
                {
                    return Err(StoreError::Deleted {
                        kind: kind.clone(),
                        id: *id,
                    }
                    .into());
                }
                WriteOp::Put(e) => {
                    gone.remove(&e.id);
                }
                // 中身から ID を作る種類は、消してから同じ中身で作り直すと同じ ID になる (03 §4.2 の規則 0 の例外)。
                WriteOp::Delete { kind, id } if !self.store.has_content_ids(kind) => {
                    gone.insert(*id);
                }
                _ => {}
            }
            if let Some(e) = apply_op(&mut tx, op, &mut out)? {
                patched.push(e);
            }
        }
        // 2. ジャーナルに書いて書き切る (03 §6)。書けなければ DB にも書かない。
        let (lines, deleted) = self.journal_lines(req, patched);
        for id in deleted {
            cuts.insert(id, JournalCut::All);
        }
        let mut journaled = false;
        if !lines.is_empty() {
            let mut guard = self.store.journal.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(journal) = guard.as_mut() {
                let now = self.store.clock().now();
                // 組の行は v0.5 の根だけで書く (03 §6)。
                let grouped = self.store.is_v05_root()
                    && req
                        .ops
                        .iter()
                        .any(|op| matches!(op, WriteOp::GroupJournalLines));
                let append = |journal: &mut crate::journal::Journal| {
                    if grouped {
                        journal.append_group(now, &lines)
                    } else {
                        journal.append(now, &lines)
                    }
                };
                let seq = match append(journal) {
                    // 空きが尽きたら、予備のファイルを消してもう一度書く (03 §8)。
                    Err(e)
                        if e.kind() == std::io::ErrorKind::StorageFull
                            && self.store.release_reserve() =>
                    {
                        append(journal)
                    }
                    r => r,
                }
                .map_err(|e| StoreError::Journal(e.to_string()))?;
                tx.set_journal_seq(seq)?;
                journaled = true;
            }
        }
        // 3. コミットする。
        if let Err(e) = tx.commit() {
            if journaled {
                self.poisoned.set(true);
            }
            return Err(e.into());
        }
        self.store.remember_deleted(gone);
        // 4. すぐにジャーナルを書き直す (削除した記録の `put` の行と、忘れるの伏せ字の前の行を除く、03 §6)。
        // 失敗しても依頼は断らない (削除の行は次の起動で、忘れるは起動時と sleep-time の当て直しで除く)。
        if !cuts.is_empty() {
            out.journal_rewrite = Some(self.rewrite_journal(&cuts));
        }
        Ok(out)
    }

    /// ジャーナルを書き直す。ジャーナルのロックを持ったまま行うので、書き直しの間に追記は入らない (03 §6 の
    /// 「追記との直列」)。読むだけで開いていてジャーナルが無ければ、何もしない。
    ///
    /// [`JournalCut::All`] は、消した記録の ID ([`Store::was_deleted`]) にだけ当てる。生きている記録の行を
    /// 消さないため。ほかの ID は当てずに [`RewriteReport::skipped`] に入れる。
    fn rewrite_journal(
        &self,
        cuts: &BTreeMap<Id, JournalCut>,
    ) -> Result<RewriteReport, RewriteError> {
        let (allowed, refused): (BTreeMap<Id, JournalCut>, BTreeMap<Id, JournalCut>) = cuts
            .iter()
            .map(|(&id, &cut)| (id, cut))
            .partition(|&(id, cut)| cut != JournalCut::All || self.store.was_deleted(id));
        let mut guard = self.store.journal.lock().unwrap_or_else(|e| e.into_inner());
        let mut report = match guard.as_mut() {
            Some(journal) => journal.rewrite(&allowed)?,
            None => RewriteReport::default(),
        };
        report.skipped.extend(refused.into_keys());
        Ok(report)
    }
}

/// 依頼の [`WriteOp::RewriteJournal`] をまとめる。同じ ID に [`JournalCut::All`] があればそれを、
/// [`JournalCut::Below`] どうしなら大きいほうを採る。
fn requested_cuts(ops: &[WriteOp]) -> BTreeMap<Id, JournalCut> {
    let mut cuts: BTreeMap<Id, JournalCut> = BTreeMap::new();
    for op in ops {
        let WriteOp::RewriteJournal(more) = op else {
            continue;
        };
        for (&id, &cut) in more {
            let merged = match (cuts.get(&id), cut) {
                (Some(JournalCut::All), _) | (_, JournalCut::All) => JournalCut::All,
                (Some(JournalCut::Below(a)), JournalCut::Below(b)) => {
                    JournalCut::Below((*a).max(b))
                }
                (None, c) => c,
            };
            cuts.insert(id, merged);
        }
    }
    cuts
}

impl Writer {
    /// 壊れた記録を、依頼とは別の先のトランザクションで quarantine へ移す (依頼が断られても、移したことが残る
    /// ように)。できるだけ行い、移せなくても依頼は続ける (発話の追記を止めないため、03 §8)。一時の失敗なら、
    /// 次の書き込みでもう一度試す。quarantine の記録を書けない状態 (新しい版の quarantine の記録がある) では
    /// 試し直さない。
    fn quarantine(&self, durability: Durability, out: &mut WriteOutcome) {
        let keys = self.store.take_suspects();
        let moved = self.store.write(&self.token, durability).and_then(|mut q| {
            let n = q.quarantine_keys(&keys)?;
            q.commit()?;
            Ok(n)
        });
        match moved {
            Ok(n) => out.quarantined = n,
            Err(e) => {
                let lasting = matches!(
                    e,
                    StoreError::KindHasNewerRecords(_)
                        | StoreError::WrongVersion { .. }
                        | StoreError::UnknownKind(_)
                );
                if !lasting {
                    self.store.restore_suspects(keys);
                }
                out.quarantine_failed = Some(e.to_string());
            }
        }
    }

    /// 依頼のうちジャーナルに書くもの (会話と明示の操作の種類の put と削除、一度だけ書く meta) と、
    /// 削除する ID。
    /// `patched` は、依頼の `Patch` の順に、差し替えて書いた封筒。
    fn journal_lines(&self, req: &WriteRequest, patched: Vec<Envelope>) -> (Vec<LineOp>, Vec<Id>) {
        let mut lines = Vec::new();
        let mut deleted = Vec::new();
        let mut patched = patched.into_iter();
        for op in &req.ops {
            match op {
                WriteOp::Put(e) if self.store.is_journaled(&e.kind) => {
                    let mut e = e.clone();
                    e.writer = self.store.evorag_version().to_owned();
                    lines.push(LineOp::Put(e));
                }
                WriteOp::Patch { .. } => {
                    if let Some(e) = patched.next()
                        && self.store.is_journaled(&e.kind)
                    {
                        lines.push(LineOp::Put(e));
                    }
                }
                WriteOp::Delete { kind, id } if self.store.is_journaled(kind) => {
                    deleted.push(*id);
                    lines.push(LineOp::Delete {
                        kind: kind.clone(),
                        id: *id,
                    });
                }
                WriteOp::SetMetaOnce { key, value } => lines.push(LineOp::Meta {
                    key: key.clone(),
                    value: value.clone(),
                }),
                _ => {}
            }
        }
        (lines, deleted)
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

/// 操作を当てる。`Patch` なら、差し替えて書いた封筒を返す (ジャーナルの行にするため)。
pub(crate) fn apply_op(
    tx: &mut WriteTxn<'_>,
    op: &WriteOp,
    out: &mut WriteOutcome,
) -> Result<Option<Envelope>, StoreError> {
    match op {
        WriteOp::Patch {
            kind,
            id,
            fields,
            updated_at,
        } => {
            let stale = || StoreError::Stale {
                kind: kind.clone(),
                id: *id,
            };
            let Some(Read::Ok(old)) = tx.get(kind, *id)? else {
                return Err(stale());
            };
            let mut env = old.clone();
            env.apply_patch(fields)
                .map_err(|source| StoreError::BadBody {
                    kind: kind.clone(),
                    id: *id,
                    source,
                })?;
            if let Some(t) = updated_at {
                env.updated_at = *t;
            }
            // 関係と副索引 (セッションの更新時刻の副索引など) は、読み直した封筒から作り直す (外で読んだ古い
            // 値の項目を残さないため)。
            let store = tx.store;
            store.undo_derived(tx, &old)?;
            tx.put(&mut env)?;
            store.redo_derived(tx, &env)?;
            // 伏せ字で改訂の番号を上げた (忘れる): 前の本文の跡を次の起動で compact して除く (03 §3.1.1)。
            if store.raises_monotonic(&old, &env) {
                tx.set_compact_pending()?;
            }
            out.put.push(env.id);
            return Ok(Some(env));
        }
        WriteOp::Expect { kind, id, field } => {
            let ok = match tx.get(kind, *id)? {
                Some(Read::Ok(env)) => field
                    .as_ref()
                    .is_none_or(|(name, want)| env.body_uint(name) == Some(*want)),
                _ => false,
            };
            if !ok {
                return Err(StoreError::Stale {
                    kind: kind.clone(),
                    id: *id,
                });
            }
        }
        WriteOp::ExpectBody { kind, id, body } => {
            let ok = match (tx.get(kind, *id)?, body) {
                (None, None) => true,
                (Some(Read::Ok(env)), Some(want)) => env.body_digest() == *want,
                _ => false,
            };
            if !ok {
                return Err(StoreError::Stale {
                    kind: kind.clone(),
                    id: *id,
                });
            }
        }
        WriteOp::Put(env) => {
            let mut env = env.clone();
            // 改訂の番号を上げる書き換え (伏せ字) は、前の本文の跡を次の起動で compact して除く (03 §3.1.1)。
            let raised = tx.store.has_monotonic(&env.kind)
                && matches!(
                    tx.get(&env.kind, env.id)?,
                    Some(Read::Ok(old)) if tx.store.raises_monotonic(&old, &env)
                );
            tx.put(&mut env)?;
            if raised {
                tx.set_compact_pending()?;
            }
            out.put.push(env.id);
        }
        WriteOp::Delete { kind, id } => {
            // 利用者の削除 (ジャーナルに書く種類) で消した記録の跡は、次の起動で compact して除く (03 §8)。
            // 作り直せる派生の記録と job の削除では立てない (起動のたびの compact を避けるため。消すべき本文を
            // 持つ派生の記録の削除は、呼び手が `SetCompactPending` を足す)。
            if tx.delete(kind, *id)? && tx.store.is_journaled(kind) {
                tx.set_compact_pending()?;
            }
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
        WriteOp::ClearPendingDeletes => tx.clear_pending_deletes()?,
        WriteOp::SetCompactPending => tx.set_compact_pending()?,
        WriteOp::ClearForgetRecheck => tx.clear_forget_recheck()?,
        // コミットの後に書き手が行う・ジャーナルの行の書き方だけを決める (DB には書かない)。
        WriteOp::RewriteJournal(_) | WriteOp::GroupJournalLines => {}
    }
    Ok(None)
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
