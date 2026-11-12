//! `evorag rebuild [--index]` (03 §10、FR-212)。派生データを、会話の本文と明示の操作から作り直す。
//!
//! - 実行の前に退避する (`backups/pre-op/`)。
//! - `--index` は索引だけ: 語彙の索引をチャンクとノートの記録から作り直し、ベクトルの索引は消す (次の起動で
//!   埋め込みの記録から作り直す)。
//! - 全体: チャンク・ノート・埋め込み・点検の結果の記録を消し、チャンクと語彙の索引をその場で作る。ノートと
//!   埋め込みと点検は、次に TUI を開いたときの裏の処理が、状態との差から進める (LLM のサーバを使うため。
//!   job は書かない、04 §4)。
//! - 正本 (会話・明示の操作・モデルの記録・しきい値・想起への操作) は変えない。

use std::path::PathBuf;
use std::sync::Arc;

use evorag_memory::{KIND_CHUNK, KIND_EMBEDDING, KIND_MODEL_PROBE, KIND_NOTE, Writes};
use evorag_store::{OpenError, OpenOptions, Read, Store, WriteOp, WriteRequest, Writer};
use evorag_types::{Clock, IdGen};

/// 作り直しの設定。
pub struct RebuildOptions {
    pub data_root: PathBuf,
    pub evorag_version: String,
    /// 退避に含める `config.toml`。
    pub config_file: Option<PathBuf>,
    /// 索引だけを作り直す (`--index`)。
    pub index_only: bool,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

/// 作り直しの結果。
#[derive(Debug)]
pub struct RebuildReport {
    /// 実行の前に作った退避。
    pub backup: PathBuf,
    /// 消した派生の記録の数 (`--index` では 0)。
    pub removed: usize,
    /// 語彙の索引の項目の数 (チャンクと鍵)。
    pub docs: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    #[error(transparent)]
    Open(#[from] OpenError),
    #[error("the data root is read-only: {0}")]
    ReadOnly(String),
    #[error("backup failed: {0}")]
    Backup(String),
    #[error("write failed: {0}")]
    Write(String),
    #[error("index failed: {0}")]
    Index(String),
}

/// 消す派生の記録の種類 (点検の結果を含む。しきい値は正本として残す、03 §10)。
const DERIVED: [&str; 4] = [KIND_CHUNK, KIND_NOTE, KIND_EMBEDDING, KIND_MODEL_PROBE];
/// 1 回の書き込みに入れる操作の数。
const OPS_PER_WRITE: usize = 2_000;

/// 作り直す。データ根を開けない (ほかの evorag が使っている) ときは `Open` のエラー。
pub fn run(o: RebuildOptions) -> Result<RebuildReport, RebuildError> {
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
    let result = rebuild(&store, &handle, &o);
    handle.shutdown();
    let _ = thread.join();
    let (removed, docs) = result?;
    Ok(RebuildReport {
        backup,
        removed,
        docs,
    })
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
