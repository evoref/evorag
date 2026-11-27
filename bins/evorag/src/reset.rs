//! `evorag reset [--old] [--yes]` (03 §10.2、07 §5、FR-218)。
//!
//! 今のデータ根 (`--old` では、`import-v04` の後の古い根) を丸ごと消す。消す範囲と、元に戻せないことを示して
//! 確かめてから消す (`--yes` で省く)。手順は `evorag_store::reset` (TUI の処分の操作と同じ関数)。
//!
//! 終了コード: 0 消し終えた。1 確認で y 以外を答えた・対象が条件に合わない・消せないファイルが残った。2 ロックが
//! 取れない (改名の隙にほかのプロセスが開いたときを含む)。

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use evorag_memory::{DirectiveBody, DirectiveOp, KIND_DIRECTIVE};
use evorag_store::lock::{DataRootLock, LockError};
use evorag_store::migrate::FormatPlan;
use evorag_store::reset::{
    Done, ImportSource, MarkError, Prepared, ResetError, ResetOptions, Target, deleting_leftovers,
    prepare, read_records,
};
use evorag_store::root_rename::{RenameRootError, read_meta};
use evorag_store::{DB_FILE, OpenOptions, Store, WriteOp, WriteRequest, Writer, meta};
use evorag_tui::catalog::Catalog;
use evorag_types::{Clock, MessageKey, UuidV7Gen};

/// `evorag reset` に要る場所。
pub(crate) struct Places<'a> {
    /// 今のデータ根 (`--data-root`・`storage.data_root`・既定の順)。
    pub data_root: &'a Path,
    /// 設定 `storage.backup_dir`。
    pub backup_dir: Option<&'a Path>,
    /// 消さずに残すもの (設定のファイル・状態のフォルダ)。
    pub keep: &'a [&'a Path],
}

/// `evorag reset [--old] [--yes]` を行う。
pub(crate) fn run(
    catalog: &Catalog,
    places: &Places<'_>,
    old: bool,
    yes: bool,
    clock: Arc<dyn Clock>,
) -> ExitCode {
    if old {
        run_old(catalog, places, yes, clock)
    } else {
        run_current(catalog, places, yes, clock)
    }
}

fn path(p: &Path) -> String {
    p.display().to_string()
}

/// 今のデータ根を消す。
fn run_current(
    catalog: &Catalog,
    places: &Places<'_>,
    yes: bool,
    clock: Arc<dyn Clock>,
) -> ExitCode {
    let opts = ResetOptions {
        target: Target::Current(places.data_root),
        backup_dir: places.backup_dir,
        keep: places.keep,
        now_ms: clock.now().as_millis(),
    };
    let prepared = match prepare(&opts) {
        Ok(p) => p,
        Err(e) => return failed(catalog, &e),
    };
    let mut lines = plan_lines(catalog, &prepared, false);
    // 取り込みの前の古い根が残っていれば、そのパスを示し、先に消すことを勧める (03 §10.2 の「確認」)。
    if prepared.target_exists()
        && let Some(kept) = kept_old_root(prepared.target())
    {
        lines.push(catalog.with(MessageKey::ResetAskKeptOld, &[("path", &path(&kept))]));
    }
    if !confirm(catalog, lines, yes) {
        return declined(catalog);
    }
    let target = prepared.target().to_path_buf();
    match prepared.run(&mut |_| Ok(())) {
        Ok(done) => finished(catalog, &done, "evorag reset", || {
            catalog.with(
                MessageKey::ResetDone,
                &[
                    ("path", &path(&target)),
                    ("backups", &done.backups_removed.to_string()),
                ],
            )
        }),
        Err(e) => failed(catalog, &e),
    }
}

/// 今の根の `import.old_root` が `kept` なら、残っている古い根のパス (あるときだけ)。
fn kept_old_root(root: &Path) -> Option<std::path::PathBuf> {
    let m = read_meta(root).ok()?;
    if m.get(meta::IMPORT_OLD_ROOT).map(String::as_str) != Some(meta::IMPORT_OLD_ROOT_KEPT) {
        return None;
    }
    let old = ImportSource::parse(m.get(meta::IMPORT_SOURCE)?)?.old_root(root);
    std::fs::symlink_metadata(&old).is_ok().then_some(old)
}

/// `import-v04` の後の古い根を消す。今の根のロックを持ったまま行い、消し終えたら今の根の `import.old_root` を
/// `disposed:<時刻>` にする (DB とジャーナル)。
fn run_old(catalog: &Catalog, places: &Places<'_>, yes: bool, clock: Arc<dyn Clock>) -> ExitCode {
    let root = places.data_root;
    let not_imported = || {
        crate::fail(
            catalog,
            1,
            MessageKey::ResetNotImported,
            &[("root", &path(root))],
        )
    };
    // DB の無いフォルダには、ロックのファイルも作らない。
    if !root.join(DB_FILE).is_file() {
        return not_imported();
    }
    let lock = match DataRootLock::acquire(root) {
        Ok(l) => l,
        Err(LockError::InUse { pid }) => return crate::data_root_in_use(catalog, root, pid),
        Err(e) => {
            return crate::fail(
                catalog,
                2,
                MessageKey::CliDataRootUnusable,
                &[("detail", &e.to_string())],
            );
        }
    };
    let Ok(m) = read_meta(root) else {
        return not_imported();
    };
    let Some(source) = m
        .get(meta::IMPORT_SOURCE)
        .and_then(|s| ImportSource::parse(s))
    else {
        return not_imported();
    };
    if m.get(meta::IMPORT_OLD_ROOT)
        .is_some_and(|v| v.starts_with(meta::IMPORT_OLD_ROOT_DISPOSED))
    {
        return crate::fail(catalog, 1, MessageKey::ResetAlreadyDisposed, &[]);
    }
    let excluded = m.get(meta::IMPORT_REPORT).map_or(0, |r| excluded_turns(r));
    let forgets = forgets_after_import(root);
    let now_ms = clock.now().as_millis();
    let opts = ResetOptions {
        target: Target::Old {
            current_root: root,
            source: &source,
        },
        backup_dir: places.backup_dir,
        keep: places.keep,
        now_ms,
    };
    let prepared = match prepare(&opts) {
        Ok(p) => p,
        Err(e) => return failed(catalog, &e),
    };
    let mut lines = plan_lines(catalog, &prepared, true);
    lines.push(catalog.with(
        MessageKey::ResetAskOldForgets,
        &[
            ("forgets", &forgets.to_string()),
            ("excluded", &excluded.to_string()),
        ],
    ));
    if !confirm(catalog, lines, yes) {
        return declined(catalog);
    }
    let target = prepared.target().to_path_buf();
    let mut lock = Some(lock);
    let mut mark = |value: &str| -> Result<(), MarkError> {
        let lock = lock.take().ok_or("the disposal is recorded only once")?;
        mark_disposed(root, lock, value, clock.clone())
    };
    match prepared.run(&mut mark) {
        Ok(done) => finished(catalog, &done, "evorag reset --old", || {
            catalog.with(
                MessageKey::ResetDoneOld,
                &[
                    ("path", &path(&target)),
                    ("backups", &done.backups_removed.to_string()),
                ],
            )
        }),
        Err(e) => failed(catalog, &e),
    }
}

/// 今の根 `root` を、持っているロック `lock` のまま v0.5 の根として開き、`import.old_root` を `value` にする
/// (DB とジャーナルの両方、03 §3.1.1)。
fn mark_disposed(
    root: &Path,
    lock: DataRootLock,
    value: &str,
    clock: Arc<dyn Clock>,
) -> Result<(), MarkError> {
    let opts = OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: env!("CARGO_PKG_VERSION").to_owned(),
        kinds: evorag_memory::kinds_v05(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
        config_file: None,
    };
    let (store, _) = Store::open_locked_with(opts, FormatPlan::v05(), lock)?;
    let (writer, handle) = Writer::new(Arc::new(store))?;
    let thread = std::thread::spawn(move || writer.run());
    let written = handle.submit_wait(WriteRequest::conversation(vec![WriteOp::SetMetaLatest {
        key: meta::IMPORT_OLD_ROOT.into(),
        value: value.into(),
    }]));
    handle.shutdown();
    thread
        .join()
        .map_err(|_| "the writer thread stopped unexpectedly")?;
    written?;
    Ok(())
}

/// 取り込みの後に書いた忘れるの指示の数 (取り込みで写した指示を除く)。読めなければ 0。
fn forgets_after_import(root: &Path) -> usize {
    read_records(root, KIND_DIRECTIVE)
        .unwrap_or_default()
        .iter()
        .filter_map(|env| env.body_as::<DirectiveBody>().ok())
        .filter(|b| !b.imported && b.op() == DirectiveOp::Forget)
        .count()
}

/// `import.report` の、取り込みで除いたターンの数。
fn excluded_turns(report: &str) -> usize {
    serde_json::from_str::<serde_json::Value>(report)
        .ok()
        .and_then(|v| {
            v.get("excluded")?.as_array().map(|a| {
                a.iter()
                    .filter(|e| e.get("kind").and_then(|k| k.as_str()) == Some("turn"))
                    .count()
            })
        })
        .unwrap_or(0)
}

/// 確認に示す、消す範囲の行。
fn plan_lines(catalog: &Catalog, p: &Prepared, old: bool) -> Vec<String> {
    let mut lines = Vec::new();
    let target = path(p.target());
    let backups = p.inside_backups().to_string();
    if p.target_exists() {
        let key = if old {
            MessageKey::ResetAskOldRoot
        } else {
            MessageKey::ResetAskRoot
        };
        lines.push(catalog.with(key, &[("path", &target), ("backups", &backups)]));
    } else if old && p.leftovers().next().is_none() {
        lines.push(catalog.with(MessageKey::ResetAskOldMissing, &[("path", &target)]));
    }
    for l in p.leftovers() {
        lines.push(catalog.with(MessageKey::ResetAskLeftover, &[("path", &path(l))]));
    }
    if let Some(dir) = p.backup_dir()
        && p.outside_backups() > 0
    {
        lines.push(catalog.with(
            MessageKey::ResetAskBackups,
            &[
                ("dir", &path(dir)),
                ("count", &p.outside_backups().to_string()),
            ],
        ));
    }
    lines
}

/// 消す範囲を示して確かめる (`yes` なら尋ねずに示すだけ)。
fn confirm(catalog: &Catalog, mut lines: Vec<String>, yes: bool) -> bool {
    lines.push(catalog.key(MessageKey::ResetAskConfirm));
    let text = lines.join("\n");
    if yes {
        eprintln!("{}", lines[..lines.len() - 1].join("\n"));
        return true;
    }
    crate::ask_yes(&text)
}

fn declined(catalog: &Catalog) -> ExitCode {
    eprintln!("{}", catalog.key(MessageKey::ResetDeclined));
    ExitCode::from(1)
}

/// 消し終えた (0)。消せないものが残れば、残ったパスを示して 1。
fn finished(
    catalog: &Catalog,
    done: &Done,
    command: &str,
    message: impl FnOnce() -> String,
) -> ExitCode {
    if done.left.is_empty() {
        eprintln!("{}", message());
        return ExitCode::SUCCESS;
    }
    crate::fail(
        catalog,
        1,
        MessageKey::ResetLeft,
        &[("paths", &done.left.join("; ")), ("command", command)],
    )
}

/// 断った・止まった理由の文言 (何が起きたか・データは無事か・次に何をするか、NFR-604)。
fn failed(catalog: &Catalog, e: &ResetError) -> ExitCode {
    let code = e.exit_code();
    let say = |key: MessageKey, args: &[(&str, &str)]| crate::fail(catalog, code, key, args);
    match e {
        ResetError::NotDataRoot { path: p } => {
            say(MessageKey::ResetNotDataRoot, &[("path", &path(p))])
        }
        ResetError::IdMismatch { path: p } => {
            say(MessageKey::ResetIdMismatch, &[("path", &path(p))])
        }
        ResetError::Overlaps { path: p, other } => say(
            MessageKey::ResetOverlaps,
            &[("path", &path(p)), ("other", &path(other))],
        ),
        ResetError::Link { path: p } => say(MessageKey::ResetLink, &[("path", &path(p))]),
        ResetError::MountPoint { path: p } => {
            say(MessageKey::ResetMountPoint, &[("path", &path(p))])
        }
        ResetError::OldRootChanged { path: p, lines } => say(
            MessageKey::ResetOldChanged,
            &[("path", &path(p)), ("lines", &lines.to_string())],
        ),
        ResetError::UnknownLeftover { path: p } => {
            say(MessageKey::ResetUnknownLeftover, &[("path", &path(p))])
        }
        ResetError::Locked { path: p, pid } => crate::data_root_in_use(catalog, p, *pid),
        ResetError::Lock { error, .. } => say(
            MessageKey::CliDataRootUnusable,
            &[("detail", &error.to_string())],
        ),
        ResetError::Rename(RenameRootError::NotUndone { left, .. }) => say(
            MessageKey::ResetRenameNotUndone,
            &[("left", &path(left)), ("detail", &e.to_string())],
        ),
        ResetError::Rename(r) if r.is_lock() => {
            say(MessageKey::ResetRenameGap, &[("detail", &r.to_string())])
        }
        ResetError::Rename(r) => say(MessageKey::ResetRenameFailed, &[("detail", &r.to_string())]),
        ResetError::Mark(d) => say(MessageKey::ResetMarkFailed, &[("detail", d)]),
        other => say(MessageKey::CliError, &[("detail", &other.to_string())]),
    }
}

/// `evorag doctor` に示す、データ根の隣に残っている reset の消しかけのフォルダ (03 §10.2)。
pub(crate) fn leftover_lines(catalog: &Catalog, data_root: &Path) -> Vec<String> {
    deleting_leftovers(data_root)
        .iter()
        .map(|p| catalog.with(MessageKey::ResetLeftoverFound, &[("path", &path(p))]))
        .collect()
}
