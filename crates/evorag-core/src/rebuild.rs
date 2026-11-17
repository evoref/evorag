//! `evorag rebuild [--index] [--facts]` (03 §10、FR-212)。派生データを、会話の本文と明示の操作から作り直す。
//!
//! - 実行の前に退避する (`backups/pre-op/`)。
//! - `--index` は索引だけ: 語彙の索引をチャンク・ノート・事実 (事実の検索の鍵、04 §5.5) の記録から作り直し、
//!   ベクトルの索引は消す (次の起動で埋め込みの記録から作り直す)。
//! - 全体: チャンク・ノート・埋め込み・点検の結果の記録を消し、チャンクと語彙の索引をその場で作る。ノートと
//!   埋め込みと点検は、次に TUI を開いたときの裏の処理が、状態との差から進める (LLM のサーバを使うため。
//!   job は書かない、04 §4)。
//! - `--facts` (v0.3): 全体の作り直しの前に、事実を抽出からやり直す ([`facts`] の手順。llama-server を使う)。
//!   事実を入れ替えてから索引を作るので、語彙の索引の事実の検索の鍵も新しい事実から作る。
//! - 正本 (会話・明示の操作・モデルの記録・しきい値・想起への操作・事実への指示) は変えない。

mod facts;

use std::path::PathBuf;
use std::sync::Arc;

use evorag_llm::LlmError;
use evorag_memory::{KIND_CHUNK, KIND_EMBEDDING, KIND_MODEL_PROBE, KIND_NOTE, Writes};
use evorag_store::{OpenError, OpenOptions, Read, Store, WriteOp, WriteRequest, Writer};
use evorag_types::{Clock, IdGen};

pub use facts::{FactsRebuilt, FactsSource, FactsUnavailable, LOST_REASONS};

/// 作り直しの設定。
pub struct RebuildOptions {
    pub data_root: PathBuf,
    pub evorag_version: String,
    /// 退避に含める `config.toml`。
    pub config_file: Option<PathBuf>,
    /// 索引だけを作り直す (`--index`)。`facts` とは一緒に使えない。
    pub index_only: bool,
    /// 事実も抽出からやり直す (`--facts`)。
    pub facts: Option<FactsSource>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

/// 作り直しの結果。
#[derive(Debug)]
pub struct RebuildReport {
    /// 実行の前に作った退避。
    pub backup: PathBuf,
    /// 消した派生の記録の数 (`--index` では 0。事実は数えない)。
    pub removed: usize,
    /// 語彙の索引の項目の数 (チャンクと鍵)。
    pub docs: usize,
    /// 事実の作り直しの結果 (`--facts` のときだけ)。
    pub facts: Option<FactsRebuilt>,
}

#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    #[error(transparent)]
    Open(#[from] OpenError),
    #[error("the data root is read-only: {0}")]
    ReadOnly(String),
    #[error("backup failed: {0}")]
    Backup(String),
    #[error("read failed: {0}")]
    Read(String),
    #[error("write failed: {0}")]
    Write(String),
    #[error("index failed: {0}")]
    Index(String),
    /// `--index` と `--facts` を一緒に指定した。
    #[error("--index and --facts cannot be used together")]
    IndexWithFacts,
    /// llama-server に繋がらない (事実は変えていない)。
    #[error("llama-server is unreachable: {0}")]
    LlmUnreachable(LlmError),
    /// 応答のモデルが、データ根に記録したモデルと違う (06 §3.1。事実は変えていない)。
    #[error(
        "the chat model differs from the recorded one (recorded: {recorded}, connected: {connected})"
    )]
    ModelMismatch { recorded: String, connected: String },
    /// 事実を作り直せない (事実は変えていない)。
    #[error("facts cannot be rebuilt: {0}")]
    FactsUnavailable(FactsUnavailable),
}

impl From<FactsUnavailable> for RebuildError {
    fn from(e: FactsUnavailable) -> Self {
        Self::FactsUnavailable(e)
    }
}

/// 消す派生の記録の種類 (点検の結果を含む。しきい値は正本として残す、03 §10)。
const DERIVED: [&str; 4] = [KIND_CHUNK, KIND_NOTE, KIND_EMBEDDING, KIND_MODEL_PROBE];
/// 1 回の書き込みに入れる操作の数。
const OPS_PER_WRITE: usize = 2_000;

/// 作り直す。データ根を開けない (ほかの evorag が使っている) ときは `Open` のエラー。
///
/// `--facts` では、退避の前に llama-server と応答のモデルと値の鍵を確かめ、抽出をすべて終えてから事実を
/// 1 つの書き込みで入れ替える。途中で止めたとき (繋がらない・確かめに外れた) は、事実も派生データも変えない。
///
/// # Errors
/// データ根を開けない・読むだけで開いた・退避や書き込みや索引に失敗した・事実を作り直せないとき。
pub async fn run(o: RebuildOptions) -> Result<RebuildReport, RebuildError> {
    if o.index_only && o.facts.is_some() {
        return Err(RebuildError::IndexWithFacts);
    }
    let (store, _) = Store::open(OpenOptions {
        data_root: o.data_root.clone(),
        evorag_version: o.evorag_version.clone(),
        kinds: evorag_memory::kinds(),
        clock: o.clock.clone(),
        ids: o.ids.clone(),
        config_file: o.config_file.clone(),
    })?;
    if let Some(r) = store.read_only() {
        return Err(RebuildError::ReadOnly(format!("{r:?}")));
    }
    // 事実の作り直しに要るものを、退避の前に確かめる (使えない退避で、残す 3 世代を押し出さないため)。
    let checked = match &o.facts {
        Some(src) => Some(facts::check(&store, src, &*o.clock, &*o.ids).await?),
        None => None,
    };
    let extra: Vec<&std::path::Path> = o.config_file.as_deref().into_iter().collect();
    let backup = store
        .pre_op_backup("rebuild", &extra)
        .map_err(|e| RebuildError::Backup(e.to_string()))?;
    let store = Arc::new(store);
    let (writer, handle) =
        Writer::new(store.clone()).map_err(|e| RebuildError::Write(e.to_string()))?;
    let thread = std::thread::Builder::new()
        .name("evorag-writer".into())
        .spawn(move || writer.run())
        .map_err(|e| RebuildError::Write(e.to_string()))?;
    let result = async {
        let facts = match (&o.facts, &checked) {
            (Some(src), Some(checked)) => {
                Some(rebuild_facts(&store, &handle, src, checked, &o).await?)
            }
            _ => None,
        };
        let (removed, docs) = rebuild(&store, &handle, &o)?;
        Ok::<_, RebuildError>((removed, docs, facts))
    }
    .await;
    handle.shutdown();
    let _ = thread.join();
    let (removed, docs, facts) = result?;
    Ok(RebuildReport {
        backup,
        removed,
        docs,
        facts,
    })
}

/// 事実を抽出からやり直し、1 つの書き込みで入れ替える。
async fn rebuild_facts(
    store: &Store,
    handle: &evorag_store::WriterHandle,
    src: &FactsSource,
    checked: &facts::Checked,
    o: &RebuildOptions,
) -> Result<FactsRebuilt, RebuildError> {
    let key = facts::key(store, handle, checked)?;
    let mut out = FactsRebuilt::default();
    let extracted = facts::extract(store, src, checked, &key, &mut out).await?;
    facts::swap(
        store,
        handle,
        &key,
        checked,
        extracted,
        (&*o.clock, &*o.ids),
        &mut out,
    )?;
    Ok(out)
}

fn rebuild(
    store: &Arc<Store>,
    handle: &evorag_store::WriterHandle,
    o: &RebuildOptions,
) -> Result<(usize, usize), RebuildError> {
    let submit = |ops: Vec<WriteOp>| -> Result<(), RebuildError> {
        for chunk in ops.chunks(OPS_PER_WRITE) {
            handle
                .submit_wait(WriteRequest::background(chunk.to_vec()))
                .map_err(|e| RebuildError::Write(e.to_string()))?;
        }
        Ok(())
    };
    let mut removed = 0;
    if !o.index_only {
        let mut ops = Vec::new();
        {
            let r = store
                .read()
                .map_err(|e| RebuildError::Write(e.to_string()))?;
            for kind in DERIVED {
                r.for_each(kind, |x| {
                    let id = match x {
                        Read::Ok(e) => e.id,
                        Read::Corrupt { id, .. } | Read::Newer { id, .. } => id,
                    };
                    removed += 1;
                    ops.push(WriteOp::Delete {
                        kind: kind.into(),
                        id,
                    });
                    if kind == KIND_EMBEDDING {
                        ops.push(WriteOp::DeleteBlobs(id));
                    }
                    ops.push(WriteOp::DeleteLinksOf(id));
                    true
                })
                .map_err(|e| RebuildError::Write(e.to_string()))?;
            }
        }
        submit(ops)?;
    }
    // 索引のフォルダを消してから、語彙の索引を作り直す。ベクトルの索引は次の起動で、埋め込みの記録から
    // 作り直す (全体の作り直しでは、裏の処理が埋め込みから作る)。
    let root = store.data_root().to_path_buf();
    let memory = crate::embedding::memory_dir(&root);
    if let Ok(entries) = std::fs::read_dir(&memory) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("vec-") {
                std::fs::remove_dir_all(e.path())
                    .map_err(|e| RebuildError::Index(e.to_string()))?;
            }
        }
    }
    let lex = crate::recall::index_dir(&root);
    if lex.exists() {
        std::fs::remove_dir_all(&lex).map_err(|e| RebuildError::Index(e.to_string()))?;
    }
    let plan = crate::recall::sync_plan(store);
    let mut docs = plan.keep;
    let writes = Writes {
        clock: &*o.clock,
        ids: &*o.ids,
    };
    if !plan.delete.is_empty() || !plan.create.is_empty() {
        let (made, ops) = writes
            .chunks(&plan.delete, &plan.create)
            .map_err(|e| RebuildError::Write(e.to_string()))?;
        submit(ops)?;
        docs.extend(made.iter().filter_map(|(id, c)| {
            plan.texts
                .get(&c.turn)
                .map(|t| crate::recall::chunk_doc(*id, c, t))
        }));
    }
    let n = docs.len();
    let (ix, _) = crate::recall::open_index(&root);
    let ix =
        ix.ok_or_else(|| RebuildError::Index("the lexical index could not be opened".into()))?;
    ix.replace_all(docs)
        .map_err(|e| RebuildError::Index(e.to_string()))?;
    Ok((removed, n))
}
