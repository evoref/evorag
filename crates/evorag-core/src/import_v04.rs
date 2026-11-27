//! v0.4 のデータ根から新しいデータ根への取り込み (`import-v04`、03 §12.4、FR-219)。入れ替え (改名) の前までを行う。
//!
//! [`run`] は、(1) 古い根が取り込める根かを確かめて読むだけで開き、(2) 同じ親のフォルダに `<名前>.v05-import` の
//! 新しい根 (形式の版 `0.5`) を作り、(3) 写す記録を通常の書き込みの入口で DB とジャーナルに書き、(4) 新しい根を開き
//! 直して照合し、照合の結果 (`import.report`) を書く。照合が合わなければ新しい根を消す (古い根はそのまま)。新しい根の
//! ロックは、作ってから照合の後まで持ち続け、古い根のロックとともに呼び手に渡す (入れ替えの直前まで持つ、03 §9)。
//!
//! 何を写すか・どう変えるかはメモリの層 (`evorag_memory` の `import_v04`)、古い根を読むことと照合の道具は保存の層
//! (`evorag_store::import_v04`) が持つ (02 §4.4)。入れ替えと、途中で止まった取り込みの片付けは、この後の段で足す。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use evorag_memory::{
    COPY_KINDS, Disposition, Exclusion, FactBody, FactDirectiveBody, ForgetKey, KIND_DIRECTIVE,
    KIND_FACT_DIRECTIVE, Selector, copy_ops, imported_directive, read_fact_directives, read_facts,
    v04_ended,
};
use evorag_store::forget::matching_markers;
use evorag_store::import_v04::{
    DigestSummary, KindDigest, OldRoot, OldRootError, RebuildError, Refusal, import_dir,
    rebuild_check, remove_dir_retry, renamed_old_root,
};
use evorag_store::location::Verdict;
use evorag_store::lock::{DataRootLock, LockError};
use evorag_store::{
    Durability, Envelope, OpenError, OpenOptions, Read, Store, StoreError, WriteError, WriteOp,
    WriteRequest, Writer, WriterHandle, fsutil, meta,
};
use evorag_types::{Clock, Id, IdGen, UuidV7Gen};

/// 1 つの `Immediate` のコミットにまとめる記録の数 (03 §12.4 の「`Immediate` のコミットを区切りごとにまとめる」)。
/// 大きくすると 1 つのトランザクションが重くなり、小さくするとコミット (約 1〜2 ms、03 §3.4) の数が増える。
const BATCH_RECORDS: usize = 256;

/// 照合の (3) で、ジャーナルから DB を作り直すフォルダ (新しい根の中。照合の結果を書く前に消す)。
const REBUILD_DIR: &str = "import-check";

/// 照合の結果 (`import.report`) の形の版。
const REPORT_V: u32 = 1;
/// `import.source` の形の版。
const SOURCE_V: u32 = 1;

/// 照合の前に呼ぶ関数 (古い根・新しい根のパス)。試験が照合の失敗を作るためだけに使う。
pub type BeforeVerify = fn(&Path, &Path);

/// 取り込みの指定。
pub struct ImportOptions {
    /// 古い根 (v0.4 のデータ根)。
    pub old_root: PathBuf,
    /// この evorag の版 (semver)。
    pub evorag_version: String,
    pub clock: Arc<dyn Clock>,
    /// 新しい根の ID の生成器。取り込みの後に、写した ID と古い根の削除の行の ID の最大を下限にする (03 §3.3)。
    pub ids: Arc<dyn IdGen>,
    /// 危ない置き場所の検査 (03 §2.2)。古い根と新しい根の両方に当てる。
    pub location: fn(&Path) -> Verdict,
    /// 危ない置き場所を警告だけにする (設定 `storage.allow_unsafe_location`)。
    pub allow_unsafe_location: bool,
    /// 試験だけ: 写し終えてから照合の前に呼ぶ。
    #[doc(hidden)]
    pub before_verify: Option<BeforeVerify>,
}

/// 取り込みが失敗した・断った理由。
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// 取り込める根でない (ロックのファイルのほかは何も残していない。終了コード 1)。
    #[error(transparent)]
    Refused(#[from] Refusal),
    /// 古い根か新しい根のロックが取れない (終了コード 2)。
    #[error(transparent)]
    Locked(#[from] LockError),
    /// 危ない置き場所 (終了コード 2)。
    #[error("unsafe location: {0}")]
    UnsafeLocation(String),
    /// 前の取り込みの新しい根のフォルダが残っている (片付けは入れ替えの段が行う)。
    #[error("{} already exists", .0.display())]
    Leftover(PathBuf),
    /// 照合が合わなかった。新しい根は消した (古い根はそのまま。終了コード 1)。
    #[error("the imported data root did not verify: {}", .0.join("; "))]
    Mismatch(Vec<String>),
    /// 新しい根を開けない (ロックのほか)。
    #[error(transparent)]
    Open(OpenError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Write(#[from] WriteError),
    /// 照合の (3) を行えない。
    #[error(transparent)]
    Rebuild(#[from] RebuildError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// `cause` で失敗し、作りかけの新しい根 `left` を消せなかった (次の取り込みが片付ける)。
    #[error("{cause}; and {} could not be removed: {error}", left.display())]
    NotRemoved {
        cause: Box<ImportError>,
        left: PathBuf,
        #[source]
        error: std::io::Error,
    },
}

impl From<OldRootError> for ImportError {
    fn from(e: OldRootError) -> Self {
        match e {
            OldRootError::Refused(r) => ImportError::Refused(r),
            OldRootError::Lock(l) => ImportError::Locked(l),
            OldRootError::Io(io) => ImportError::Io(io),
        }
    }
}

impl From<OpenError> for ImportError {
    fn from(e: OpenError) -> Self {
        match e {
            OpenError::Lock(l) => ImportError::Locked(l),
            other => ImportError::Open(other),
        }
    }
}

/// 除いた記録 1 件 (03 §12.4 の「除くもの」)。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Excluded {
    pub kind: String,
    pub id: Id,
    pub reason: Exclusion,
}

/// 照合の結果 (`import.report` に書くもの)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// 封筒をそのまま写した種類ごとの件数とハッシュ (照合の (1))。
    pub kinds: BTreeMap<String, DigestSummary>,
    /// 写した指示の件数と (ID ‖ 本体の CBOR) のハッシュ (照合の (2))。
    pub directives: DigestSummary,
    /// 根拠の位置 (`at`) を決められなかった `private`・`accept`・`revert` の数 (元の事実が無い)。
    pub directives_without_at: usize,
    /// `import.v04_ended` の表の値の数。
    pub v04_ended: usize,
    /// 除いた記録 (種類と ID の順)。
    pub excluded: Vec<Excluded>,
}

impl ImportReport {
    /// 理由ごとの除いた数。
    #[must_use]
    pub fn excluded_count(&self, reason: Exclusion) -> usize {
        self.excluded.iter().filter(|e| e.reason == reason).count()
    }

    fn to_json(&self) -> String {
        let kinds: serde_json::Map<String, serde_json::Value> = self
            .kinds
            .iter()
            .map(|(k, d)| {
                (
                    k.clone(),
                    serde_json::json!({ "count": d.count, "sha256": d.sha256 }),
                )
            })
            .collect();
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for e in &self.excluded {
            *counts.entry(e.reason.name()).or_default() += 1;
        }
        let excluded: Vec<serde_json::Value> = self
            .excluded
            .iter()
            .map(|e| {
                serde_json::json!({
                    "kind": e.kind, "id": e.id.to_string(), "reason": e.reason.name(),
                })
            })
            .collect();
        serde_json::json!({
            "v": REPORT_V,
            "kinds": kinds,
            "directive": {
                "count": self.directives.count,
                "sha256": self.directives.sha256,
                "without_at": self.directives_without_at,
            },
            "v04_ended": self.v04_ended,
            "excluded": excluded,
            "excluded_counts": counts,
            "rebuild": "match",
            "old_root": "unchanged",
        })
        .to_string()
    }
}

/// 取り込んだ結果。新しい根は照合を終え、`import.report` を持つ (入れ替えはまだ)。
#[derive(Debug)]
pub struct Imported {
    /// 新しい根 (`<古い根の名前>.v05-import`)。
    pub new_root: PathBuf,
    /// 入れ替えの後の古い根のパス (`import.source` に書いたもの、03 §12.4 の「入れ替え」)。
    pub renamed_old_root: PathBuf,
    pub report: ImportReport,
    /// 危ない置き場所の警告 (警告だけにしたもの)。
    pub location_warnings: Vec<String>,
    /// 古い根のロック (入れ替えの直前まで持つ、03 §9)。
    pub old_root_lock: DataRootLock,
    /// 新しい根のロック (作ってから持ち続けている。入れ替えの直前まで持つ、03 §9)。
    pub new_root_lock: DataRootLock,
}

/// v0.4 の根を新しい根へ取り込み、照合する (03 §12.4)。入れ替えはしない。
///
/// # Errors
/// [`ImportError`] の通り。取り込める根でないとき・危ない置き場所・ロックが取れないときは、ロックのファイルのほかは
/// 何も書かない。書き始めた後に失敗したとき (照合が合わないときを含む) は、新しい根のフォルダを消してから返す
/// (消せなければ [`ImportError::NotRemoved`])。古い根は変えない。
pub fn run(opts: &ImportOptions) -> Result<Imported, ImportError> {
    let old_path = std::path::absolute(&opts.old_root)?;
    let not_root = || Refusal::NotDataRoot {
        path: old_path.clone(),
    };
    let new_root = import_dir(&old_path).ok_or_else(not_root)?;
    let started = opts.clock.now().as_millis();
    let renamed = renamed_old_root(&old_path, started).ok_or_else(not_root)?;

    // 危ない置き場所の検査は、古い根と新しい根の両方に、古い根に触れる前に当てる (03 §12.4 の (5))。
    let mut location_warnings = Vec::new();
    for place in [&old_path, &new_root] {
        match (opts.location)(place) {
            Verdict::Ok => {}
            Verdict::Warn(w) => location_warnings.push(w),
            Verdict::Unsafe(u) if opts.allow_unsafe_location => location_warnings.push(u),
            Verdict::Unsafe(u) => return Err(ImportError::UnsafeLocation(u)),
        }
    }

    let mut old = OldRoot::open(&old_path)?;
    if new_root.exists() {
        return Err(ImportError::Leftover(new_root));
    }
    let source = Source::read(&old)?;

    let mut created = NewRoot::create(&new_root, opts)?;
    let outcome = (|| -> Result<ImportReport, ImportError> {
        let copied = created.copy(&old, &source, opts, &renamed, started)?;
        created.close_writer()?;
        if let Some(hook) = opts.before_verify {
            hook(&old_path, &new_root);
        }
        let store = created.reopen(opts)?;
        let (report, store) = verify(store, &mut old, &copied, opts)?;
        created.keep_lock(store.into_lock());
        Ok(report)
    })();
    match outcome {
        Ok(report) => Ok(Imported {
            new_root,
            renamed_old_root: renamed,
            report,
            location_warnings,
            old_root_lock: old.into_lock(),
            new_root_lock: created.commit(),
        }),
        Err(e) => Err(created.discard(e)),
    }
}

/// 古い根から、書き始める前に読むもの (指示・事実・鍵・言語)。読めないもの・断る理由は、ここで見つける。
struct Source {
    directives: Vec<(Envelope, FactDirectiveBody)>,
    facts: BTreeMap<Id, FactBody>,
    forget_key: Option<String>,
    prompt_language: Option<String>,
    selector: Selector,
    /// 写す前に除いたもの (ジャーナルに削除の行のある `fact_directive`)。
    excluded: Vec<Excluded>,
}

impl Source {
    fn read(old: &OldRoot) -> Result<Self, ImportError> {
        let (all, facts) =
            old.read(|view| Ok::<_, Refusal>((read_fact_directives(view)?, read_facts(view)?)))??;
        // ジャーナルに削除の行のある ID の指示は、v0.4 が開くときに消すものなので写さない (03 §6・§12.4)。
        let (deleted, directives): (Vec<_>, Vec<_>) = all
            .into_iter()
            .partition(|(e, _)| old.deleted().contains_key(&e.id));
        let excluded = deleted
            .into_iter()
            .map(|(e, _)| Excluded {
                kind: KIND_FACT_DIRECTIVE.to_owned(),
                id: e.id,
                reason: Exclusion::Deleted,
            })
            .collect();
        let forget_key = old.meta(meta::FORGET_KEY).map(str::to_owned);
        // 指示か値の鍵を持つ事実があるのに鍵が無い根は、写した値の鍵を確かめられない (03 §3.1.1 のフェイルクローズ)。
        if forget_key.is_none()
            && (!directives.is_empty() || facts.values().any(|f| f.vk.is_some()))
        {
            return Err(Refusal::ForgetKeyUnusable.into());
        }
        let prompt_language = old.meta(meta::PROMPT_LANGUAGE).map(str::to_owned);
        let markers = matching_markers(prompt_language.as_deref());
        let selector = Selector::new(&directives, old.deleted().keys().copied(), &markers);
        // 伏せ字の掛かるターンを先に確かめる (当て直しの前の本文が残っていれば、書く前に断る)。
        old.read(|view| selector.check_redacted_turns(view))??;
        Ok(Self {
            directives,
            facts,
            forget_key,
            prompt_language,
            selector,
            excluded,
        })
    }
}

/// 写した結果 (照合の (1)・(2) で新しい根と比べる、古い根の側の値)。
struct Copied {
    kinds: BTreeMap<String, DigestSummary>,
    directives: DigestSummary,
    directive_ids: BTreeSet<Id>,
    directives_without_at: usize,
    v04_ended: usize,
    excluded: Vec<Excluded>,
}

fn v05_options(root: &Path, opts: &ImportOptions, ids: Arc<dyn IdGen>) -> OpenOptions {
    OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: opts.evorag_version.clone(),
        kinds: evorag_memory::kinds_v05(),
        clock: opts.clock.clone(),
        ids,
        config_file: None,
    }
}

/// 作っている新しい根。[`NewRoot::commit`] するまでは、落とす (失敗・panic) と書き手を止めて、フォルダを消す。
struct NewRoot {
    root: PathBuf,
    /// 書き手を止めた後の新しい根のロック (開き直しの間も持ち続ける)。
    lock: Option<DataRootLock>,
    store: Option<Arc<Store>>,
    writer: Option<WriterHandle>,
    thread: Option<std::thread::JoinHandle<()>>,
    armed: bool,
}

impl NewRoot {
    /// 新しい根を作り (形式の版 `0.5`、権限は 03 §2.1)、ロックを取って開き、書き手を動かす。ロックが取れなければ、
    /// ほかが使っているフォルダなので消さない。
    fn create(root: &Path, opts: &ImportOptions) -> Result<Self, ImportError> {
        fsutil::create_private_dir(root)?;
        let mut out = Self {
            root: root.to_path_buf(),
            lock: None,
            store: None,
            writer: None,
            thread: None,
            armed: true,
        };
        let lock = match DataRootLock::acquire(root) {
            Ok(l) => l,
            Err(e) => {
                out.armed = false;
                return Err(e.into());
            }
        };
        let (_, plan) = evorag_memory::v05_root();
        let started = Store::open_for_import(v05_options(root, opts, opts.ids.clone()), plan, lock)
            .map_err(ImportError::from)
            .and_then(|(store, _)| {
                let store = Arc::new(store);
                let (writer, handle) = Writer::new(store.clone())?;
                let thread = std::thread::Builder::new()
                    .name("evorag-import-writer".into())
                    .spawn(move || writer.run())?;
                Ok((store, handle, thread))
            });
        match started {
            Ok((store, handle, thread)) => {
                out.store = Some(store);
                out.writer = Some(handle);
                out.thread = Some(thread);
                Ok(out)
            }
            Err(e) => Err(out.discard(e)),
        }
    }

    fn submit(&self, ops: Vec<WriteOp>) -> Result<(), ImportError> {
        if ops.is_empty() {
            return Ok(());
        }
        let writer = self.writer.as_ref().ok_or(WriteError::ShuttingDown)?;
        writer.submit_wait(WriteRequest::conversation(ops))?;
        Ok(())
    }

    /// 書き手を止めて DB を閉じ、新しい根のロックを手元に残す。
    fn close_writer(&mut self) -> Result<(), ImportError> {
        if let Some(w) = self.writer.take() {
            w.shutdown();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(store) = self.store.take() {
            let store = Arc::try_unwrap(store)
                .map_err(|_| StoreError::Io("the new data root is still in use".into()))?;
            self.lock = Some(store.into_lock());
        }
        Ok(())
    }

    /// 持っているロックのまま、新しい根を開き直す (03 §12.4 の「照合」)。
    fn reopen(&mut self, opts: &ImportOptions) -> Result<Store, ImportError> {
        let lock = self
            .lock
            .take()
            .ok_or_else(|| StoreError::Io("the new data root lock is not held".into()))?;
        let (_, plan) = evorag_memory::v05_root();
        let (store, _) =
            Store::open_for_import(v05_options(&self.root, opts, opts.ids.clone()), plan, lock)?;
        Ok(store)
    }

    fn keep_lock(&mut self, lock: DataRootLock) {
        self.lock = Some(lock);
    }

    /// 取り込みを終えた: フォルダを残し、新しい根のロックを返す。
    fn commit(mut self) -> DataRootLock {
        self.armed = false;
        self.lock
            .take()
            .expect("the new data root lock is kept after verification")
    }

    /// 書き手を止めて閉じ、フォルダを消す。`cause` に、消せなかったときはそのことを添えて返す。
    fn discard(mut self, cause: ImportError) -> ImportError {
        let _ = self.close_writer();
        self.store = None;
        self.lock = None;
        self.armed = false;
        match remove_dir_retry(&self.root) {
            Ok(()) => cause,
            Err(error) => ImportError::NotRemoved {
                cause: Box::new(cause),
                left: self.root.clone(),
                error,
            },
        }
    }

    /// 写す記録を書く (03 §12.4 の「書き方」)。ジャーナルの行の順は、`meta` の行 → セッション → ターン → 信号・
    /// 想起への操作・設定・モデルの記録 → 指示 (ID の順)。種類ごとに、[`BATCH_RECORDS`] 件ずつ 1 つの `Immediate`
    /// のコミットにまとめる。
    fn copy(
        &self,
        old: &OldRoot,
        src: &Source,
        opts: &ImportOptions,
        renamed: &Path,
        started: i64,
    ) -> Result<Copied, ImportError> {
        let ended = v04_ended(
            &src.facts,
            src.forget_key
                .as_deref()
                .and_then(ForgetKey::from_hex)
                .as_ref(),
        );
        self.submit(meta_ops(old, src, opts, renamed, started, &ended)?)?;

        let mut state = CopyState {
            selector: src.selector.clone(),
            excluded: src.excluded.clone(),
            max_id: None,
        };
        let mut kinds = BTreeMap::new();
        for kind in COPY_KINDS {
            kinds.insert(kind.to_owned(), self.copy_kind(old, kind, &mut state)?);
        }

        let mut digest = KindDigest::new();
        let mut directive_ids = BTreeSet::new();
        let mut directives_without_at = 0;
        let mut batch = Batch::default();
        for (env, body) in &src.directives {
            let d = imported_directive(env, body, &src.facts)?;
            if matches!(body.op.as_str(), "private" | "accept" | "revert")
                && d.body_as::<evorag_memory::DirectiveBody>()
                    .is_ok_and(|b| b.at.is_none())
            {
                directives_without_at += 1;
            }
            digest.add(&d);
            directive_ids.insert(d.id);
            state.max_id = state.max_id.max(Some(d.id));
            batch.push(copy_ops(d));
            if batch.records >= BATCH_RECORDS {
                self.submit(batch.take())?;
            }
        }
        self.submit(batch.take())?;

        // ID の下限: 写した ID と、古い根のジャーナルの削除の行の ID の最大 (03 §3.3)。
        let floor = state.max_id.max(old.deleted().keys().next_back().copied());
        if let Some(f) = floor {
            opts.ids.raise_floor(f);
        }
        let mut excluded = state.excluded;
        excluded.sort();
        Ok(Copied {
            kinds,
            directives: digest.finish(),
            directive_ids,
            directives_without_at,
            v04_ended: ended.values.len(),
            excluded,
        })
    }

    /// 封筒をそのまま写す 1 つの種類を、ID の順に写す。写した記録の (ID ‖ 本体の CBOR) のハッシュを返す。
    fn copy_kind(
        &self,
        old: &OldRoot,
        kind: &str,
        state: &mut CopyState,
    ) -> Result<DigestSummary, ImportError> {
        let mut digest = KindDigest::new();
        let mut batch = Batch::default();
        let mut failed: Option<ImportError> = None;
        let mut step = |r: Read| -> Result<(), ImportError> {
            let env = match r {
                Read::Ok(e) => e,
                Read::Newer { kind, id, .. } | Read::Corrupt { kind, id, .. } => {
                    return Err(Refusal::Unreadable {
                        what: format!("record {kind}/{id}"),
                    }
                    .into());
                }
            };
            match state.selector.judge(&env)? {
                Disposition::Copy => {
                    digest.add(&env);
                    state.max_id = state.max_id.max(Some(env.id));
                    batch.push(copy_ops(env));
                    if batch.records >= BATCH_RECORDS {
                        self.submit(batch.take())?;
                    }
                }
                Disposition::Exclude(reason) => state.excluded.push(Excluded {
                    kind: env.kind.clone(),
                    id: env.id,
                    reason,
                }),
            }
            Ok(())
        };
        old.read(|view| {
            view.for_each(kind, |r| match step(r) {
                Ok(()) => true,
                Err(e) => {
                    failed = Some(e);
                    false
                }
            })
        })?
        .map_err(|e| Refusal::Unreadable {
            what: e.to_string(),
        })?;
        if let Some(e) = failed {
            return Err(e);
        }
        self.submit(batch.take())?;
        Ok(digest.finish())
    }
}

impl Drop for NewRoot {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // 失敗の経路は [`NewRoot::discard`] で消す。ここに来るのは panic などで落としたときだけ。
        let _ = self.close_writer();
        self.store = None;
        self.lock = None;
        if remove_dir_retry(&self.root).is_err() {
            // パスは利用者の名前を含みうるので、ログには出さない (NFR-502)。次の取り込みが片付ける。
            tracing::warn!("an unfinished import-v04 data root could not be removed");
        }
    }
}

/// 種類をまたいで持ち越す、写しの状態。
struct CopyState {
    selector: Selector,
    excluded: Vec<Excluded>,
    max_id: Option<Id>,
}

/// 最初に書く `meta` の行: 鍵は、DB とジャーナルの両方に、ほかの記録より先に書く (03 §3.1.1)。
fn meta_ops(
    old: &OldRoot,
    src: &Source,
    opts: &ImportOptions,
    renamed: &Path,
    started: i64,
    ended: &evorag_memory::V04Ended,
) -> Result<Vec<WriteOp>, ImportError> {
    let source = serde_json::json!({
        "v": SOURCE_V,
        "data_root_id": old.meta(meta::DATA_ROOT_ID),
        "path": renamed.to_string_lossy(),
        "imported_at": started,
        "evorag": opts.evorag_version,
        "journal_last_seq": old.journal_last_seq(),
        "files": old.files_before().len(),
        "files_sha256": old.files_before().digest(),
    });
    let once = |key: &str, value: String| WriteOp::SetMetaOnce {
        key: key.into(),
        value,
    };
    let mut ops = Vec::new();
    if let Some(l) = &src.prompt_language {
        ops.push(once(meta::PROMPT_LANGUAGE, l.clone()));
    }
    if let Some(k) = &src.forget_key {
        ops.push(once(meta::FORGET_KEY, k.clone()));
    }
    ops.push(once(meta::IMPORT_SOURCE, source.to_string()));
    ops.push(WriteOp::SetMetaLatest {
        key: meta::IMPORT_OLD_ROOT.into(),
        value: meta::IMPORT_OLD_ROOT_KEPT.into(),
    });
    ops.push(WriteOp::SetMetaLatest {
        key: meta::IMPORT_V04_ENDED.into(),
        value: serde_json::to_string(ended)?,
    });
    Ok(ops)
}

/// 1 つの依頼にまとめる操作。
#[derive(Default)]
struct Batch {
    ops: Vec<WriteOp>,
    records: usize,
}

impl Batch {
    fn push(&mut self, ops: Vec<WriteOp>) {
        self.ops.extend(ops);
        self.records += 1;
    }

    fn take(&mut self) -> Vec<WriteOp> {
        self.records = 0;
        std::mem::take(&mut self.ops)
    }
}

/// 種類 `kind` の記録の (ID ‖ 本体の CBOR) のハッシュ。読めない記録は数えて返す。
fn digest_of(store: &Store, kind: &str) -> Result<(DigestSummary, usize), ImportError> {
    let mut digest = KindDigest::new();
    let mut unreadable = 0usize;
    store.read()?.for_each(kind, |x| {
        match x {
            Read::Ok(e) => digest.add(&e),
            _ => unreadable += 1,
        }
        true
    })?;
    Ok((digest.finish(), unreadable))
}

/// 開き直した新しい根を照合し (03 §12.4 の「照合」の (1)〜(4))、合えば `import.report` を書く。
fn verify(
    store: Store,
    old: &mut OldRoot,
    copied: &Copied,
    opts: &ImportOptions,
) -> Result<(ImportReport, Store), ImportError> {
    let mut problems = Vec::new();
    if store.format_version() != evorag_store::FORMAT_V05 {
        problems.push(format!("format {}", store.format_version()));
    }

    // (1) 封筒をそのまま写した種類ごとの件数とハッシュ。
    let mut kinds = BTreeMap::new();
    for kind in COPY_KINDS {
        let (got, unreadable) = digest_of(&store, kind)?;
        if unreadable > 0 || copied.kinds.get(kind) != Some(&got) {
            problems.push(format!("kind {kind} differs"));
        }
        kinds.insert(kind.to_owned(), got);
    }
    // (2) 指示の件数と元の ID の集合、古い根から写し方で作った本体のハッシュ。
    let (directives, unreadable) = digest_of(&store, KIND_DIRECTIVE)?;
    let mut ids = BTreeSet::new();
    store.read()?.for_each(KIND_DIRECTIVE, |x| {
        if let Read::Ok(e) = x {
            ids.insert(e.id);
        }
        true
    })?;
    if ids != copied.directive_ids {
        problems.push("directive ids differ".to_owned());
    }
    if unreadable > 0 || directives != copied.directives {
        problems.push("directive bodies differ".to_owned());
    }

    // (3) 新しい根のジャーナルだけから DB を作り直して比べる。
    let scratch_ids: Arc<dyn IdGen> = Arc::new(UuidV7Gen::new(opts.clock.clone()));
    let (_, plan) = evorag_memory::v05_root();
    let diff = rebuild_check(
        &store,
        v05_options(&store.data_root().join(REBUILD_DIR), opts, scratch_ids),
        plan,
    )?;
    if !diff.is_empty() {
        problems.push(format!(
            "rebuild from the journal differs (records {}, links {}, lookups {}, meta {:?})",
            diff.records, diff.links, diff.lookups, diff.meta
        ));
    }

    // (4) 古い根のすべてのファイル (ロックのファイルを除く) が、取り込みの前と同じ。
    let after = old.close_and_snapshot()?;
    let changed = old.files_before().changed(&after);
    if !changed.is_empty() {
        problems.push(format!(
            "{} file(s) of the old data root changed",
            changed.len()
        ));
    }

    if !problems.is_empty() {
        return Err(ImportError::Mismatch(problems));
    }
    let report = ImportReport {
        kinds,
        directives,
        directives_without_at: copied.directives_without_at,
        v04_ended: copied.v04_ended,
        excluded: copied.excluded.clone(),
    };
    let token = store.take_write_token().ok_or(StoreError::TokenTaken)?;
    let mut tx = store.write(&token, Durability::Immediate)?;
    tx.set_meta_once(meta::IMPORT_REPORT, &report.to_json())?;
    tx.commit()?;
    Ok((report, store))
}
