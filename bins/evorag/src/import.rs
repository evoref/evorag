//! `evorag import-v04 [--from <古い根>]` (03 §12.4、07 §5) と、通常の起動の前の、止まった取り込みの扱い。
//!
//! 終了コード: 0 取り込んで入れ替えた (止まった入れ替えを続けて終えたときも)。1 取り込める根でない・照合が合わない・
//! 確認で y 以外を答えた・ほかの失敗。2 ロックが取れない・危ない置き場所・改名の隙にほかのプロセスが開いた。

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use evorag_core::import_v04::{
    Finished, ImportError, ImportOptions, Plan, Resumed, import_and_swap, resume_on_startup,
};
use evorag_store::import_v04::{NextStep, Refusal, import_dir};
use evorag_store::lock::LockError;
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
        ImportError::Rename(r) if r.is_lock() => one(
            MessageKey::ImportV04RenameGap,
            &[("detail", &r.to_string())],
        ),
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
        other => catalog.with(
            MessageKey::ImportV04ReasonUnreadable,
            &[("detail", &other.to_string())],
        ),
    }
}
