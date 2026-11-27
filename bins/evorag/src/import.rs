//! `evorag import-v04 [--from <古い根>]` (03 §12.4、07 §5) と、通常の起動の前の、止まった取り込みの扱い。
//!
//! 終了コード: 0 取り込んで入れ替えた (止まった入れ替えを続けて終えたときも)。1 取り込める根でない・照合が合わない・
//! 確認で y 以外を答えた・ほかの失敗。2 ロックが取れない・危ない置き場所・改名の隙にほかのプロセスが開いた。
//!
//! v0.4 の根 (形式の版 `0.3`) を開こうとしたとき (03 §12.4 の「きっかけ」): TUI の起動は取り込むかを尋ね
//! ([`import_at_startup`])、y なら取り込んで新しい根で続け、y 以外は何も変えずに 2 で終わる。データ根に書くほかの
//! コマンドは、何も書かずに `import-v04` を案内して 2 で終わる ([`needs_import`]・[`refuse_v04_root`])。

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use evorag_core::import_v04::{
    Finished, ImportError, ImportOptions, Plan, Resumed, import_and_swap, resume_on_startup,
};
use evorag_store::import_v04::{NextStep, Refusal, import_dir};
use evorag_store::lock::{DataRootLock, LockError};
use evorag_store::root_rename::RenameRootError;
use evorag_tui::catalog::Catalog;
use evorag_types::{Clock, MessageKey, UuidV7Gen};

/// `evorag import-v04` を行う。`old_root` は `--from` (省けば今のデータ根)。
pub(crate) fn run(
    catalog: &Catalog,
    old_root: &Path,
    allow_unsafe_location: bool,
    clock: Arc<dyn Clock>,
) -> ExitCode {
    let opts = ImportOptions {
        old_root: old_root.to_path_buf(),
        evorag_version: env!("CARGO_PKG_VERSION").to_owned(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
        location: evorag_store::location::check,
        allow_unsafe_location,
        before_verify: None,
        swap_hooks: Default::default(),
    };
    let mut confirm = |plan: &Plan<'_>| {
        let yes = crate::ask_yes(&question(catalog, plan));
        if yes && matches!(plan, Plan::Import { .. }) {
            eprintln!("{}", catalog.key(MessageKey::ImportV04Started));
        }
        yes
    };
    match import_and_swap(&opts, &mut confirm) {
        Ok(done) => {
            print_done(catalog, &done);
            ExitCode::SUCCESS
        }
        Err(e) => {
            for line in describe(catalog, old_root, &e) {
                eprintln!("{line}");
            }
            ExitCode::from(e.exit_code())
        }
    }
}

/// v0.4 の根 (形式の版 `format`) なので、在り場所では開かずに `import-v04` を案内する (03 §8・§12.4)。何も書いて
/// いない。終了コード 2 (データ根が使えない、07 §5)。
pub(crate) fn needs_import(catalog: &Catalog, data_root: &Path, format: &str) -> ExitCode {
    eprintln!(
        "{}",
        catalog.with(
            MessageKey::CliNeedsImport,
            &[
                ("path", &data_root.display().to_string()),
                ("format", format)
            ]
        )
    );
    ExitCode::from(2)
}

/// 形式の版 `format` が v0.5 より古い (v0.4 以前の根か退避)。読めない版は古いとみなさない (開く側が断る)。
pub(crate) fn older_than_v05(format: &str) -> bool {
    use evorag_store::version::Version;
    let v05: Version = evorag_store::FORMAT_V05
        .parse()
        .expect("FORMAT_V05 is a version");
    format.parse::<Version>().is_ok_and(|v| v < v05)
}

/// データ根の DB が v0.4 以前の形式なら、何も書かずに `import-v04` を案内した終了コード 2 を返す (データ根に書く
/// コマンドのうち、ストアを開かずに書くもの: `restore`・`backups purge`)。DB が無い・読むだけでは開けないときは
/// `None` (各コマンドの扱いに任せる)。
pub(crate) fn refuse_v04_root(catalog: &Catalog, data_root: &Path) -> Option<ExitCode> {
    let format = evorag_store::inspect::format_version(&data_root.join(evorag_store::DB_FILE))?;
    older_than_v05(&format).then(|| needs_import(catalog, data_root, &format))
}

/// `evorag restore <退避>` の前の確かめ (03 §8)。今の根が v0.4 の根なら [`refuse_v04_root`]。今の根が v0.5 の根で、
/// 退避が v0.4 以前の形式なら、何も変えずに終了コード 1 (v0.5 の根を v0.4 の DB で置き換えないため)。
pub(crate) fn check_restore(
    catalog: &Catalog,
    data_root: &Path,
    backup: &Path,
) -> Option<ExitCode> {
    if let Some(code) = refuse_v04_root(catalog, data_root) {
        return Some(code);
    }
    let file = if backup.is_dir() {
        backup.join(evorag_store::DB_FILE)
    } else {
        backup.to_path_buf()
    };
    let root_has_db =
        evorag_store::inspect::format_version(&data_root.join(evorag_store::DB_FILE)).is_some();
    let format = evorag_store::inspect::format_version(&file)?;
    (root_has_db && older_than_v05(&format)).then(|| {
        eprintln!(
            "{}",
            catalog.with(
                MessageKey::CliRestoreBackupV04,
                &[
                    ("backup", &backup.display().to_string()),
                    ("format", &format)
                ]
            )
        );
        ExitCode::from(1)
    })
}

/// TUI の起動で v0.4 の根 (形式の版 `format`) を開こうとしたときに、取り込むかを尋ねる (03 §12.4 の「きっかけ」)。y
/// なら `import-v04` と同じ手順で取り込んで入れ替え、新しい根のロックを返す (呼ぶ側は、そのまま新しい根を開く)。
/// y 以外・取り込めない・失敗したときは、理由を示した終了コード 2 (データ根が使えない) を返す。
pub(crate) fn import_at_startup(
    catalog: &Catalog,
    data_root: &Path,
    format: &str,
    allow_unsafe_location: bool,
    clock: Arc<dyn Clock>,
) -> Result<DataRootLock, ExitCode> {
    eprintln!(
        "{}",
        catalog.with(
            MessageKey::ImportV04AtStartup,
            &[
                ("path", &data_root.display().to_string()),
                ("format", format)
            ]
        )
    );
    let opts = ImportOptions {
        old_root: data_root.to_path_buf(),
        evorag_version: env!("CARGO_PKG_VERSION").to_owned(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
        location: evorag_store::location::check,
        allow_unsafe_location,
        before_verify: None,
        swap_hooks: Default::default(),
    };
    let mut confirm = |plan: &Plan<'_>| {
        let yes = crate::ask_yes(&question(catalog, plan));
        if yes && matches!(plan, Plan::Import { .. }) {
            eprintln!("{}", catalog.key(MessageKey::ImportV04Started));
        }
        yes
    };
    match import_and_swap(&opts, &mut confirm) {
        Ok(done) => {
            print_done(catalog, &done);
            let Finished { new_root_lock, .. } = done;
            Ok(new_root_lock)
        }
        Err(ImportError::Declined) => {
            eprintln!("{}", catalog.key(MessageKey::ImportV04DeclinedAtStartup));
            Err(ExitCode::from(2))
        }
        Err(e) => {
            for line in describe(catalog, data_root, &e) {
                eprintln!("{line}");
            }
            Err(ExitCode::from(2))
        }
    }
}

/// 通常の起動 (TUI) の前に、データ根の止まった取り込みを扱う (03 §12.4 の「入れ替え」)。止まった入れ替えを続け、
/// 作りかけを消し、したことを示す。扱えなければ、理由を示した終了コード 2 (データ根が使えない) を返す。
pub(crate) fn resume_before_start(catalog: &Catalog, data_root: &Path) -> Result<(), ExitCode> {
    let path = |p: &Path| p.display().to_string();
    match resume_on_startup(data_root) {
        Ok(Resumed::Swapped {
            renamed_old_root,
            new_root_lock,
        }) => {
            // ストアは自分でロックを取り直して開く (更新の片付けがロックを取るため)。
            drop(new_root_lock);
            eprintln!(
                "{}",
                catalog.with(
                    MessageKey::ImportV04ResumedAtStartup,
                    &[
                        ("root", &path(data_root)),
                        ("renamed", &path(&renamed_old_root))
                    ]
                )
            );
        }
        Ok(Resumed::RemovedUnfinished) => {
            let new = import_dir(data_root).unwrap_or_default();
            eprintln!(
                "{}",
                catalog.with(
                    MessageKey::ImportV04RemovedUnfinishedAtStartup,
                    &[("new", &path(&new))]
                )
            );
        }
        Ok(Resumed::RemovedStale) => eprintln!(
            "{}",
            catalog.with(
                MessageKey::ImportV04RemovedStaleAtStartup,
                &[("root", &path(data_root))]
            )
        ),
        Ok(_) => {}
        Err(e) => {
            for line in describe(catalog, data_root, &e) {
                eprintln!("{line}");
            }
            return Err(ExitCode::from(2));
        }
    }
    Ok(())
}

/// 確認の問い (07 §5)。
fn question(catalog: &Catalog, plan: &Plan<'_>) -> String {
    let path = |p: &Path| p.display().to_string();
    match *plan {
        Plan::Import {
            old_root,
            root,
            renamed_old_root,
            new_root,
            redo,
        } => {
            let ask = catalog.with(
                MessageKey::ImportV04Ask,
                &[
                    ("old", &path(old_root)),
                    ("renamed", &path(renamed_old_root)),
                    ("root", &path(root)),
                ],
            );
            if redo {
                let redo = catalog.with(MessageKey::ImportV04AskRedo, &[("new", &path(new_root))]);
                format!("{redo}\n{ask}")
            } else {
                ask
            }
        }
        Plan::Continue {
            old_root,
            root,
            renamed_old_root,
            new_root,
        } => catalog.with(
            MessageKey::ImportV04AskContinue,
            &[
                ("new", &path(new_root)),
                ("old", &path(old_root)),
                ("renamed", &path(renamed_old_root)),
                ("root", &path(root)),
            ],
        ),
        _ => String::new(),
    }
}

fn print_done(catalog: &Catalog, done: &Finished) {
    for w in &done.location_warnings {
        eprintln!(
            "{}",
            catalog.with(MessageKey::CliLocationWarning, &[("detail", w)])
        );
    }
    let root = done.root.display().to_string();
    let renamed = done.renamed_old_root.display().to_string();
    let m = match &done.report {
        Some(r) => {
            let directives = r.directives.count;
            let copied = r.kinds.values().map(|d| d.count).sum::<u64>() + directives;
            catalog.with(
                MessageKey::ImportV04Done,
                &[
                    ("root", &root),
                    ("renamed", &renamed),
                    ("copied", &copied.to_string()),
                    ("directives", &directives.to_string()),
                    ("excluded", &r.excluded.len().to_string()),
                ],
            )
        }
        None => catalog.with(
            MessageKey::ImportV04Continued,
            &[("root", &root), ("renamed", &renamed)],
        ),
    };
    eprintln!("{m}");
}

/// 失敗の文言 (何が起きたか・データは無事か・次に何をするか、NFR-604)。`root` は取り込み元 (元の名前)。
fn describe(catalog: &Catalog, root: &Path, e: &ImportError) -> Vec<String> {
    let path = |p: &Path| p.display().to_string();
    let one = |key: MessageKey, args: &[(&str, &str)]| vec![catalog.with(key, args)];
    match e {
        ImportError::Refused(r) => {
            let reason = refusal_reason(catalog, r);
            let next = catalog.key(match r.next_step() {
                NextStep::CheckPath => MessageKey::ImportV04NextCheckPath,
                NextStep::OpenOnceWithV04 => MessageKey::ImportV04NextOpenOnceWithV04,
                NextStep::RepairWithV04 => MessageKey::ImportV04NextRepairWithV04,
                NextStep::PointAtRealFolder => MessageKey::ImportV04NextRealFolder,
                NextStep::MoveIntoFolder => MessageKey::ImportV04NextMoveIntoFolder,
                _ => MessageKey::ImportV04NextNothing,
            });
            one(
                MessageKey::ImportV04Refused,
                &[("reason", &reason), ("next", &next)],
            )
        }
        ImportError::Locked(LockError::InUse { pid }) => {
            let pid = pid.map_or_else(|| "?".to_owned(), |p| p.to_string());
            one(
                MessageKey::CliDataRootInUse,
                &[("path", &path(root)), ("pid", &pid)],
            )
        }
        ImportError::Locked(l) => one(
            MessageKey::CliDataRootUnusable,
            &[("detail", &l.to_string())],
        ),
        ImportError::UnsafeLocation(d) => one(MessageKey::CliUnsafeLocation, &[("detail", d)]),
        ImportError::Declined => one(MessageKey::ImportV04Declined, &[]),
        ImportError::Mismatch(p) => {
            one(MessageKey::ImportV04Mismatch, &[("detail", &p.join("; "))])
        }
        ImportError::Leftover(p) => one(MessageKey::ImportV04Leftover, &[("path", &path(p))]),
        ImportError::OldRootMissing(p) => {
            let new = import_dir(root).unwrap_or_default();
            one(
                MessageKey::ImportV04OldRootMissing,
                &[("new", &path(&new)), ("path", &path(p))],
            )
        }
        ImportError::Rename(RenameRootError::NotUndone { left, .. }) => one(
            MessageKey::ImportV04RenameNotUndone,
            &[("left", &path(left)), ("detail", &e.to_string())],
        ),
        ImportError::Rename(r) if r.is_lock() => one(
            MessageKey::ImportV04RenameGap,
            &[("detail", &r.to_string())],
        ),
        ImportError::OriginalMissing { root, new_root } => one(
            MessageKey::ImportV04OriginalMissing,
            &[("root", &path(root)), ("new", &path(new_root))],
        ),
        ImportError::BothRoots { root, renamed } => one(
            MessageKey::ImportV04BothRoots,
            &[("root", &path(root)), ("renamed", &path(renamed))],
        ),
        ImportError::OtherRootAtOriginal { root, new_root } => one(
            MessageKey::ImportV04OtherRoot,
            &[("root", &path(root)), ("new", &path(new_root))],
        ),
        ImportError::OriginalUnreadable { root } => one(
            MessageKey::ImportV04OriginalUnreadable,
            &[("root", &path(root))],
        ),
        ImportError::DamagedLeftover(p) => {
            one(MessageKey::ImportV04DamagedLeftover, &[("path", &path(p))])
        }
        ImportError::NewRootUsed { new_root } => one(
            MessageKey::ImportV04NewRootUsed,
            &[("new", &path(new_root))],
        ),
        ImportError::ForeignSource(p) => {
            one(MessageKey::ImportV04ForeignSource, &[("new", &path(p))])
        }
        ImportError::Rename(r) => one(
            MessageKey::ImportV04RenameFailed,
            &[("detail", &r.to_string())],
        ),
        ImportError::CannotRemove { left, error } => one(
            MessageKey::ImportV04NotRemoved,
            &[("path", &path(left)), ("detail", &error.to_string())],
        ),
        ImportError::NotRemoved { cause, left, error } => {
            let mut lines = describe(catalog, root, cause);
            lines.push(catalog.with(
                MessageKey::ImportV04NotRemoved,
                &[("path", &path(left)), ("detail", &error.to_string())],
            ));
            lines
        }
        other => one(MessageKey::CliError, &[("detail", &other.to_string())]),
    }
}

fn refusal_reason(catalog: &Catalog, r: &Refusal) -> String {
    match r {
        Refusal::NotDataRoot { path } => catalog.with(
            MessageKey::ImportV04ReasonNotDataRoot,
            &[("path", &path.display().to_string())],
        ),
        Refusal::NotOpenedByV04 { format, max_writer } => catalog.with(
            MessageKey::ImportV04ReasonNotOpenedByV04,
            &[
                ("format", format),
                ("writer", max_writer.as_deref().unwrap_or("?")),
            ],
        ),
        Refusal::NewerFormat { format } => catalog.with(
            MessageKey::ImportV04ReasonNewerFormat,
            &[("format", format)],
        ),
        Refusal::ForgetPending => catalog.key(MessageKey::ImportV04ReasonForgetPending),
        Refusal::JournalPending { lines, .. } => catalog.with(
            MessageKey::ImportV04ReasonJournalPending,
            &[("lines", &lines.to_string())],
        ),
        Refusal::ForgetKeyUnusable => catalog.key(MessageKey::ImportV04ReasonForgetKey),
        Refusal::Unfinished { what } => {
            catalog.with(MessageKey::ImportV04ReasonUnfinished, &[("detail", what)])
        }
        Refusal::Unreadable { what } => {
            catalog.with(MessageKey::ImportV04ReasonUnreadable, &[("detail", what)])
        }
        Refusal::Link { path } => catalog.with(
            MessageKey::ImportV04ReasonLink,
            &[("path", &path.display().to_string())],
        ),
        Refusal::MountPoint { path } => catalog.with(
            MessageKey::ImportV04ReasonMountPoint,
            &[("path", &path.display().to_string())],
        ),
        Refusal::NonUnicodePath { path } => catalog.with(
            MessageKey::ImportV04ReasonNonUnicode,
            &[("path", &path.display().to_string())],
        ),
        other => catalog.with(
            MessageKey::ImportV04ReasonUnreadable,
            &[("detail", &other.to_string())],
        ),
    }
}
