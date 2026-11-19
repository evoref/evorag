//! `evorag repair` (03 §10、FR-211)。副索引と関係を記録から作り直し、ジャーナルにあって DB に無い記録を戻し、
//! 忘れる・削除を当て直す。
//!
//! 順は次のとおり。
//! 1. データ根を書き込みで開く (ロックを取る。開くときに、`journal.last_seq` より後の行を取り込み、削除の行の
//!    ある記録を消す、03 §6)。データ根が無ければ、何も作らずに断る。DB のファイルが壊れていれば、開くときに
//!    `broken/` へ移したことを返す (`evorag restore` で戻す)。
//! 2. 実行の前に退避する (`backups/pre-op/<日時>-repair-v<版>/`、03 §7.2)。退避するのは、開くときの取り込みの
//!    後の状態 (1 の後)。
//! 3. ストアの層 ([`evorag_store::Store::repair`]): ジャーナルにあって DB に無い記録と `meta` のキーを戻し
//!    (ターンの改訂の番号を下げる行は当てない)、関係と副索引を記録から作り直し、忘れるの指示があれば
//!    `forget.recheck` を立てる (1 つのトランザクション)。
//! 4. 忘れるの当て直し (04 §7.2): すべての忘れるの指示を DB の層で当て直し (伏せ字の行を先にジャーナルに書く。
//!    記録を変えたら `purge_indexes`・`forget_backups` の job と `compact.pending` を同じ依頼で書く)、ジャーナルの
//!    伏せ字の前の行を除く。
//!
//! 索引と退避の整理、compact は、残した job と印から、次に TUI を開いたときに行う (CLI は compact をしない、
//! 03 §8)。3 の後に失敗したときは、3 でしたこと (ストアの層はコミット済み) と退避の場所をエラーに持たせる
//! ([`RepairError::AfterStore`])。

use std::path::PathBuf;
use std::sync::Arc;

use evorag_memory::{
    ForgetEnv, ForgetKey, attributes, hidden_marker, journal_cuts, redaction_marker, replay_forgets,
};
use evorag_store::{
    OpenError, OpenOptions, ReadOnlyReason, Repaired, Store, WriteOp, WriteRequest, Writer,
    WriterHandle, meta,
};
use evorag_types::{Clock, IdGen};

use crate::PromptLanguage;

/// `repair` の設定。
pub struct RepairOptions {
    pub data_root: PathBuf,
    pub evorag_version: String,
    /// 退避に含める `config.toml`。
    pub config_file: Option<PathBuf>,
    /// データ根にプロンプトの言語の記録が無いときに使う言語 (伏せ字の印を選ぶ、04 §7.3)。
    pub prompt_language: PromptLanguage,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

/// 忘れるの当て直しの結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForgetsReplayed {
    /// 伏せ字にしたターンの数。
    pub patched: usize,
    /// 取り消した事実の数。
    pub retracted: usize,
    /// 消した派生の記録の数。
    pub deleted: usize,
    /// 忘れるの指示と合わないターンの数 (本文は変えない。想起・書き出しなどから除く、04 §7.2)。
    pub mismatched: usize,
    /// ジャーナルの伏せ字の前の行を除けなかった (次の起動か sleep-time に試し直す)。
    pub journal_pending: bool,
    /// `memory.forget_key` が使えないので、忘れた値に当たる事実の取り消しを当てていない (03 §3.1.1)。
    pub key_missing: bool,
}

/// `repair` の結果。
#[derive(Debug)]
pub struct RepairReport {
    /// 実行の前に作った退避。
    pub backup: PathBuf,
    /// 開くときにジャーナルから取り込んだ行の数。
    pub journal_replayed: usize,
    /// 開くときにジャーナルの削除の行から消した記録の数。
    pub journal_deletes: usize,
    /// ストアの層がしたこと (戻した記録・作り直した関係と副索引)。
    pub store: Repaired,
    /// 忘れるの当て直し。
    pub forgets: ForgetsReplayed,
}

#[derive(Debug, thiserror::Error)]
pub enum RepairError {
    /// データ根のフォルダが無い (何も作らずに断った)。
    #[error("the data root {} does not exist", .0.display())]
    NoDataRoot(PathBuf),
    #[error(transparent)]
    Open(#[from] OpenError),
    /// データ根を読むだけで開いた (新しい形式・移行の失敗)。何も書いていない。
    #[error("the data root is read-only: {0:?}")]
    ReadOnly(ReadOnlyReason),
    #[error("backup failed: {0}")]
    Backup(String),
    #[error("repair failed: {0}")]
    Repair(String),
    #[error("write failed: {0}")]
    Write(String),
    #[error("read failed: {0}")]
    Read(String),
    /// ストアの層 (記録の戻しと関係・副索引の作り直し) はコミットした後に、続く処理 (忘れるの当て直しなど) に
    /// 失敗した。直す前のデータは `backup`。
    #[error("records were restored and relations rebuilt, but a later step failed: {source}")]
    AfterStore {
        store: Box<Repaired>,
        backup: PathBuf,
        source: Box<RepairError>,
    },
}

/// 直す。データ根を開けない (ほかの evorag が使っている) ときは `Open` のエラー。
///
/// CLI (`evorag repair`) の専用。データ根のロックを持ったまま、同期の I/O で行う。
///
/// # Errors
/// データ根が無い・開けない・読むだけで開いた・退避・ジャーナルの読み取り・書き込みに失敗したとき。ストアの層を
/// コミットした後の失敗は [`RepairError::AfterStore`]。
pub fn run(o: RepairOptions) -> Result<RepairReport, RepairError> {
    // データ根が無ければ、開いて作らない (別の場所を指しているかもしれない)。
    if !o.data_root.is_dir() {
        return Err(RepairError::NoDataRoot(o.data_root));
    }
    let (store, opened) = Store::open(OpenOptions {
        data_root: o.data_root.clone(),
        evorag_version: o.evorag_version.clone(),
        kinds: evorag_memory::kinds(),
        clock: o.clock.clone(),
        ids: o.ids.clone(),
        config_file: o.config_file.clone(),
    })?;
    if let Some(r) = store.read_only() {
        return Err(RepairError::ReadOnly(r.clone()));
    }
    let extra: Vec<&std::path::Path> = o.config_file.as_deref().into_iter().collect();
    let backup = store
        .pre_op_backup("repair", &extra)
        .map_err(|e| RepairError::Backup(e.to_string()))?;
    let repaired = store
        .repair(evorag_memory::is_forget_directive)
        .map_err(|e| RepairError::Repair(e.to_string()))?;
    // ここから先の失敗は、ストアの層でしたことと退避の場所を添えて返す。
    let after = |e: RepairError| RepairError::AfterStore {
        store: Box::new(repaired.clone()),
        backup: backup.clone(),
        source: Box::new(e),
    };
    let forgets = replay_with_writer(store, &o).map_err(after)?;
    Ok(RepairReport {
        backup,
        journal_replayed: opened.journal_replayed,
        journal_deletes: opened.journal_deletes.len(),
        store: repaired,
        forgets,
    })
}

/// 書き手を立てて忘れるを当て直し、書き手を止める。書き手のスレッドが異常終了したら失敗にする。
fn replay_with_writer(store: Store, o: &RepairOptions) -> Result<ForgetsReplayed, RepairError> {
    let store = Arc::new(store);
    let (writer, handle) =
        Writer::new(store.clone()).map_err(|e| RepairError::Write(e.to_string()))?;
    let thread = std::thread::Builder::new()
        .name("evorag-writer".into())
        .spawn(move || writer.run())
        .map_err(|e| RepairError::Write(e.to_string()))?;
    let forgets = replay(&store, &handle, o);
    handle.shutdown();
    let joined = thread
        .join()
        .map_err(|_| RepairError::Write("the writer thread panicked".into()));
    let forgets = forgets?;
    joined?;
    Ok(forgets)
}

/// データ根のプロンプトの言語 (記録が無ければ設定の言語)。
fn language(store: &Store, fallback: PromptLanguage) -> PromptLanguage {
    store
        .meta(meta::PROMPT_LANGUAGE)
        .ok()
        .flatten()
        .and_then(|s| PromptLanguage::parse(&s))
        .unwrap_or(fallback)
}

/// 忘れるの当て直し (04 §7.2 の DB の層とジャーナルの層)。起動時の当て直し (`forgets::recheck`) と同じ規則で、
/// 書き手を通して書く。
fn replay(
    store: &Store,
    handle: &WriterHandle,
    o: &RepairOptions,
) -> Result<ForgetsReplayed, RepairError> {
    let lang = language(store, o.prompt_language).as_str();
    let marker = redaction_marker(lang);
    // 鍵が使えなければ (03 §3.1.1)、鍵に頼る層 (印に当たる事実の取り消し) だけを飛ばす。どの印にも当たらない鍵
    // (すべて 0) で当て直すと、取り消しだけが起きない。
    let key = crate::forget_key::ensure(store, None)
        .ok()
        .and_then(|k| k.key().cloned());
    let mut out = ForgetsReplayed {
        key_missing: key.is_none(),
        ..ForgetsReplayed::default()
    };
    let key = key.unwrap_or_else(|| ForgetKey::from_bytes([0; 32]));
    let replayed = {
        let r = store.read().map_err(|e| RepairError::Read(e.to_string()))?;
        let has_forgets = evorag_memory::latest_forget(&r)
            .map_err(|e| RepairError::Read(e.to_string()))?
            .is_some();
        out.key_missing &= has_forgets;
        let env = ForgetEnv {
            key: &key,
            attributes: attributes(),
            marker,
            hidden_marker: hidden_marker(lang),
            ids: &*o.ids,
            now: o.clock.now(),
        };
        replay_forgets(&r, &env).map_err(|e| RepairError::Read(e.to_string()))?
    };
    out.patched = replayed.patched.len();
    out.retracted = replayed.retracted.len();
    out.deleted = replayed.deleted.len();
    out.mismatched = replayed.mismatched.len();
    if replayed.changed() {
        // 伏せ字にするターンの `Patch` は、書き手が伏せ字の後の `put` の行を先にジャーナルに書く。
        handle
            .submit_wait(WriteRequest::conversation(replayed.ops))
            .map_err(|e| RepairError::Write(e.to_string()))?;
    }
    let cuts = {
        let r = store.read().map_err(|e| RepairError::Read(e.to_string()))?;
        journal_cuts(&r, &|id| store.journal_rev_span(id))
            .map_err(|e| RepairError::Read(e.to_string()))?
    };
    if !cuts.is_empty() {
        let done = handle
            .submit_wait(WriteRequest::conversation(vec![WriteOp::RewriteJournal(
                cuts,
            )]))
            .map_err(|e| RepairError::Write(e.to_string()))?;
        out.journal_pending = matches!(done.journal_rewrite, Some(Err(_)));
    }
    Ok(out)
}
