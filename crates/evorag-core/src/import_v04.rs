//! v0.4 のデータ根から新しいデータ根への取り込み (`import-v04`、03 §12.4、FR-219)。入れ替え (改名) の前までを行う。
//!
//! [`run`] は、(1) 古い根が取り込める根かを確かめて読むだけで開き、(2) 同じ親のフォルダに `<名前>.v05-import` の
//! 新しい根 (形式の版 `0.5`) を作り、(3) 写す記録を通常の書き込みの入口で DB とジャーナルに書き、(4) 新しい根を開き
//! 直して照合し、照合の結果 (`import.report`) を書く。照合が合わなければ新しい根を消す (古い根はそのまま)。
//!
//! 何を写すか・どう変えるかはメモリの層 (`evorag_memory` の `import_v04`)、古い根を読むことと照合の道具は保存の層
//! (`evorag_store::import_v04`) が持つ (02 §4.4)。入れ替えと、途中で止まった取り込みの片付けは、この後の段で足す。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use evorag_memory::{
    COPY_KINDS, Disposition, Exclusion, FactDirectiveBody, ForgetKey, KIND_DIRECTIVE,
    REDACTION_MARKERS, Selector, copy_ops, imported_directive, read_fact_directives, read_facts,
    redaction_marker, v04_ended,
};
use evorag_store::import_v04::{
    DigestSummary, KindDigest, OldRoot, OldRootError, Refusal, import_dir, rebuild_check,
    renamed_old_root,
};
use evorag_store::location::Verdict;
use evorag_store::lock::{DataRootLock, LockError};
use evorag_store::{
    Durability, Envelope, OpenOptions, Read, Store, WriteOp, WriteRequest, Writer, WriterHandle,
    meta,
};
use evorag_types::{Clock, Id, IdGen, UuidV7Gen};

/// 1 つの `Immediate` のコミットにまとめる記録の数 (03 §12.4 の「`Immediate` のコミットを区切りごとにまとめる」)。
/// 大きくすると 1 つのトランザクションが重くなり、小さくするとコミット (約 1〜2 ms、03 §3.4) の数が増える。
const BATCH_RECORDS: usize = 256;

/// 照合の (3) で、ジャーナルから DB を作り直すフォルダ (新しい根の中。照合の後に消す)。
const REBUILD_DIR: &str = "import-check";

/// 照合の結果 (`import.report`) の形の版。
const REPORT_V: u32 = 1;
/// `import.source` の形の版。
const SOURCE_V: u32 = 1;

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
}

/// 取り込みが失敗した・断った理由。
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// 取り込める根でない (何も残していない。終了コード 1)。
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
    /// 書けない・読めない (新しい根は消した)。
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<OldRootError> for ImportError {
    fn from(e: OldRootError) -> Self {
        match e {
            OldRootError::Refused(r) => ImportError::Refused(r),
            OldRootError::Lock(l) => ImportError::Locked(l),
            OldRootError::Io(io) => ImportError::Storage(io.to_string()),
        }
    }
}

fn storage(e: impl std::fmt::Display) -> ImportError {
    ImportError::Storage(e.to_string())
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
    /// 写した指示の数 (元の `fact_directive` の数。照合の (2))。
    pub directives: usize,
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
                "count": self.directives,
                "ended_added": 0,
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
}

/// v0.4 の根を新しい根へ取り込み、照合する (03 §12.4)。入れ替えはしない。
///
/// # Errors
/// [`ImportError`] の通り。取り込める根でないとき・危ない置き場所・ロックが取れないときは何も書かない。書き始めた
/// 後に失敗したとき (照合が合わないときを含む) は、新しい根のフォルダを消してから返す (古い根は変えない)。
pub fn run(opts: ImportOptions) -> Result<Imported, ImportError> {
    let old_path = std::path::absolute(&opts.old_root).map_err(storage)?;
    let not_root = || Refusal::NotDataRoot {
        path: old_path.clone(),
    };
    let new_root = import_dir(&old_path).ok_or_else(not_root)?;
    let started = opts.clock.now().as_millis();
    let renamed = renamed_old_root(&old_path, started).ok_or_else(not_root)?;

    // 危ない置き場所の検査は、古い根と新しい根の両方に当てる (03 §12.4 の (5))。
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

    let mut created = Created::new(&new_root, &opts)?;
    let copied = match created.copy(&old, &source, &opts, &renamed, started) {
        Ok(c) => c,
        Err(e) => {
            created.discard();
            return Err(e);
        }
    };
    created.close();

    match verify(&new_root, &mut old, &source, &copied, &opts) {
        Ok(report) => Ok(Imported {
            new_root,
            renamed_old_root: renamed,
            report,
            location_warnings,
            old_root_lock: old.into_lock(),
        }),
        Err(e) => {
            remove_new_root(&new_root);
            Err(e)
        }
    }
}

/// 古い根から、書き始める前に読むもの (指示・事実・鍵・言語)。読めないもの・断る理由は、ここで見つける。
struct Source {
    directives: Vec<(Envelope, FactDirectiveBody)>,
    facts: BTreeMap<Id, evorag_memory::FactBody>,
    forget_key: Option<String>,
    prompt_language: Option<String>,
    selector: Selector,
}

impl Source {
    fn read(old: &OldRoot) -> Result<Self, ImportError> {
        let (directives, facts) =
            old.read(|view| Ok::<_, Refusal>((read_fact_directives(view)?, read_facts(view)?)))??;
        let forget_key = old.meta(meta::FORGET_KEY).map(str::to_owned);
        // 指示があるのに鍵が無い根は、写した指示の値の鍵を確かめられない (03 §3.1.1 のフェイルクローズ)。
        if forget_key.is_none() && !directives.is_empty() {
            return Err(Refusal::ForgetKeyUnusable.into());
        }
        let prompt_language = old.meta(meta::PROMPT_LANGUAGE).map(str::to_owned);
        let markers: Vec<&'static str> = match prompt_language.as_deref() {
            Some(l @ ("ja" | "en")) => vec![redaction_marker(l)],
            _ => REDACTION_MARKERS.to_vec(),
        };
        let selector = Selector::new(&directives, old.deleted().keys().copied(), &markers);
        // 伏せ字の掛かるターンを先に確かめる (当て直しの前の本文が残っていれば、書く前に断る)。
        old.read(|view| selector.check_redacted_turns(view))??;
        Ok(Self {
            directives,
            facts,
            forget_key,
            prompt_language,
            selector,
        })
    }
}

/// 写した結果 (照合の (1)・(2) で新しい根と比べる、古い根の側の値)。
struct Copied {
    kinds: BTreeMap<String, DigestSummary>,
    directive_ids: BTreeSet<Id>,
    directives_without_at: usize,
    v04_ended: usize,
    excluded: Vec<Excluded>,
}

/// 作った新しい根と、その書き手。
struct Created {
    root: PathBuf,
    store: Option<Arc<Store>>,
    writer: Option<WriterHandle>,
    thread: Option<std::thread::JoinHandle<()>>,
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

impl Created {
    /// 新しい根を作り (形式の版 `0.5`、権限は 03 §2.1)、書き手を動かす。
    fn new(root: &Path, opts: &ImportOptions) -> Result<Self, ImportError> {
        let (_, plan) = evorag_memory::v05_root();
        let store = match Store::open_with(v05_options(root, opts, opts.ids.clone()), plan) {
            Ok((store, _)) => Arc::new(store),
            Err(e) => {
                remove_new_root(root);
                return Err(match e {
                    evorag_store::OpenError::Lock(l) => ImportError::Locked(l),
                    other => storage(other),
                });
            }
        };
        let mut out = Self {
            root: root.to_path_buf(),
            store: Some(store.clone()),
            writer: None,
            thread: None,
        };
        let started = Writer::new(store)
            .map_err(storage)
            .and_then(|(writer, handle)| {
                std::thread::Builder::new()
                    .name("evorag-import-writer".into())
                    .spawn(move || writer.run())
                    .map(|t| (handle, t))
                    .map_err(storage)
            });
        match started {
            Ok((handle, thread)) => {
                out.writer = Some(handle);
                out.thread = Some(thread);
                Ok(out)
            }
            Err(e) => {
                out.discard();
                Err(e)
            }
        }
    }

    fn submit(&self, ops: Vec<WriteOp>) -> Result<(), ImportError> {
        if ops.is_empty() {
            return Ok(());
        }
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| storage("the writer is stopped"))?;
        writer
            .submit_wait(WriteRequest::conversation(ops))
            .map(|_| ())
            .map_err(storage)
    }

    /// 書き手を止めて、DB を閉じる (ロックも外す)。
    fn close(&mut self) {
        if let Some(w) = self.writer.take() {
            w.shutdown();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.store = None;
    }

    /// 閉じてから、新しい根のフォルダを消す。
    fn discard(&mut self) {
        self.close();
        remove_new_root(&self.root);
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
        // 鍵は、DB とジャーナルの両方に、ほかの記録より先に書く (03 §3.1.1)。
        let ended = v04_ended(
            &src.facts,
            src.forget_key
                .as_deref()
                .and_then(ForgetKey::from_hex)
                .as_ref(),
        );
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
        let mut metas = Vec::new();
        if let Some(l) = &src.prompt_language {
            metas.push(WriteOp::SetMetaOnce {
                key: meta::PROMPT_LANGUAGE.into(),
                value: l.clone(),
            });
        }
        if let Some(k) = &src.forget_key {
            metas.push(WriteOp::SetMetaOnce {
                key: meta::FORGET_KEY.into(),
                value: k.clone(),
            });
        }
        metas.push(WriteOp::SetMetaOnce {
            key: meta::IMPORT_SOURCE.into(),
            value: source.to_string(),
        });
        metas.push(WriteOp::SetMeta {
            key: meta::IMPORT_OLD_ROOT.into(),
            value: meta::IMPORT_OLD_ROOT_KEPT.into(),
        });
        metas.push(WriteOp::SetMeta {
            key: meta::IMPORT_V04_ENDED.into(),
            value: serde_json::to_string(&ended).map_err(storage)?,
        });
        self.submit(metas)?;

        let mut selector = src.selector.clone();
        let mut kinds = BTreeMap::new();
        let mut excluded = Vec::new();
        let mut max_id: Option<Id> = None;
        for kind in COPY_KINDS {
            let mut digest = KindDigest::new();
            let mut batch = Batch::default();
            let mut failed: Option<ImportError> = None;
            old.read(|view| {
                view.for_each(kind, |r| {
                    let step = (|| -> Result<(), ImportError> {
                        let env = match r {
                            Read::Ok(e) => e,
                            Read::Newer { kind, id, .. } | Read::Corrupt { kind, id, .. } => {
                                return Err(Refusal::Unreadable {
                                    what: format!("record {kind}/{id}"),
                                }
                                .into());
                            }
                        };
                        match selector.judge(&env)? {
                            Disposition::Copy => {
                                digest.add(&env);
                                max_id = max_id.max(Some(env.id));
                                batch.push(copy_ops(env));
                                if batch.records >= BATCH_RECORDS {
                                    self.submit(batch.take())?;
                                }
                            }
                            Disposition::Exclude(reason) => excluded.push(Excluded {
                                kind: env.kind.clone(),
                                id: env.id,
                                reason,
                            }),
                        }
                        Ok(())
                    })();
                    match step {
                        Ok(()) => true,
                        Err(e) => {
                            failed = Some(e);
                            false
                        }
                    }
                })
                .map_err(storage)
            })??;
            if let Some(e) = failed {
                return Err(e);
            }
            self.submit(batch.take())?;
            kinds.insert(kind.to_owned(), digest.finish());
        }

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
            directive_ids.insert(d.id);
            max_id = max_id.max(Some(d.id));
            batch.push(copy_ops(d));
            if batch.records >= BATCH_RECORDS {
                self.submit(batch.take())?;
            }
        }
        self.submit(batch.take())?;

        // ID の下限: 写した ID と、古い根のジャーナルの削除の行の ID の最大 (03 §3.3)。
        let floor = max_id.max(old.deleted().keys().next_back().copied());
        if let Some(f) = floor {
            opts.ids.raise_floor(f);
        }
        excluded.sort();
        Ok(Copied {
            kinds,
            directive_ids,
            directives_without_at,
            v04_ended: ended.values.len(),
            excluded,
        })
    }
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

/// 新しい根のフォルダを消す (作りかけ・照合の合わないもの)。消せなくても続ける (次の取り込みが作りかけとして扱う)。
fn remove_new_root(root: &Path) {
    let _ = evorag_store::fsutil::retry_transient(|| match std::fs::remove_dir_all(root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    });
}

/// 新しい根を開き直して照合し (03 §12.4 の「照合」の (1)〜(4))、合えば `import.report` を書く。
fn verify(
    new_root: &Path,
    old: &mut OldRoot,
    src: &Source,
    copied: &Copied,
    opts: &ImportOptions,
) -> Result<ImportReport, ImportError> {
    let (_, plan) = evorag_memory::v05_root();
    let (store, _) = Store::open_with(v05_options(new_root, opts, opts.ids.clone()), plan.clone())
        .map_err(storage)?;
    let mut problems = Vec::new();
    if store.format_version() != evorag_store::FORMAT_V05 {
        problems.push(format!("format {}", store.format_version()));
    }

    // (1) 封筒をそのまま写した種類ごとの件数とハッシュ。
    let mut kinds = BTreeMap::new();
    {
        let r = store.read().map_err(storage)?;
        for kind in COPY_KINDS {
            let mut digest = KindDigest::new();
            let mut unreadable = 0usize;
            r.for_each(kind, |x| {
                match x {
                    Read::Ok(e) => digest.add(&e),
                    _ => unreadable += 1,
                }
                true
            })
            .map_err(storage)?;
            let got = digest.finish();
            if unreadable > 0 || copied.kinds.get(kind) != Some(&got) {
                problems.push(format!("kind {kind} differs"));
            }
            kinds.insert(kind.to_owned(), got);
        }
        // (2) 指示の件数と元の ID の集合。
        let mut ids = BTreeSet::new();
        r.for_each(KIND_DIRECTIVE, |x| {
            if let Read::Ok(e) = x {
                ids.insert(e.id);
            }
            true
        })
        .map_err(storage)?;
        let want: BTreeSet<Id> = src.directives.iter().map(|(e, _)| e.id).collect();
        if ids != want || ids != copied.directive_ids {
            problems.push("directive ids differ".to_owned());
        }
    }

    // (3) 新しい根のジャーナルだけから DB を作り直して比べる。
    let scratch_ids: Arc<dyn IdGen> = Arc::new(UuidV7Gen::new(opts.clock.clone()));
    match rebuild_check(
        &store,
        v05_options(&new_root.join(REBUILD_DIR), opts, scratch_ids),
        plan,
    ) {
        Ok(diff) if diff.is_empty() => {}
        Ok(diff) => problems.push(format!(
            "rebuild from the journal differs (records {}, links {}, lookups {}, meta {:?})",
            diff.records, diff.links, diff.lookups, diff.meta
        )),
        Err(e) => problems.push(format!("rebuild from the journal failed: {e}")),
    }

    // (4) 古い根のすべてのファイルが、取り込みの前と同じ。
    let after = old.close_and_snapshot().map_err(storage)?;
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
        directives: copied.directive_ids.len(),
        directives_without_at: copied.directives_without_at,
        v04_ended: copied.v04_ended,
        excluded: copied.excluded.clone(),
    };
    let token = store
        .take_write_token()
        .ok_or_else(|| storage("the write token is taken"))?;
    let mut tx = store
        .write(&token, Durability::Immediate)
        .map_err(storage)?;
    tx.set_meta_once(meta::IMPORT_REPORT, &report.to_json())
        .map_err(storage)?;
    tx.commit().map_err(storage)?;
    Ok(report)
}
