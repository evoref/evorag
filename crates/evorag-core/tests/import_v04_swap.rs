//! `import-v04` の入れ替えと、止まった取り込みの続き・片付け (03 §12.4 の「入れ替え」・「改名の前の比べ」、FR-219)。
//!
//! 見本 `tests/fixtures/db/0.4.0/root` を一時のフォルダの `evorag` に写して使う。強制終了は、止まった後の状態
//! (照合の後・2 つの改名の間・書いている途中) をファイルの操作で作って確かめる (どの段で止まっても、残るのは
//! これらの状態のどれかなので)。
//!
//! verifies: 09 §3.6 (`import-v04` の途中の強制終了)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use evorag_core::import_v04::{
    Finished, ImportError, ImportOptions, Imported, Plan, Resumed, import_and_swap,
    resume_on_startup, run,
};
use evorag_memory::kinds_v05;
use evorag_store::import_v04::{Refusal, import_dir};
use evorag_store::inspect::inspect_collect;
use evorag_store::location::Verdict;
use evorag_store::lock::DataRootLock;
use evorag_store::testutil::{TempDir, raw_remove_meta};
use evorag_store::{DB_FILE, meta};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};

const NOW: i64 = 1_795_000_000_000;

fn fixture_root() -> PathBuf {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    dir.join("../../tests/fixtures/db/0.4.0/root")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the copy");
    for e in std::fs::read_dir(from).expect("read the fixture") {
        let e = e.expect("an entry");
        let target = to.join(e.file_name());
        if e.file_type().expect("a file type").is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), &target).expect("copy a file");
        }
    }
}

fn copied_fixture(label: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new(label);
    let root = tmp.path().join("evorag");
    copy_dir(&fixture_root(), &root);
    (tmp, root)
}

fn options_at(root: &Path, now: i64) -> ImportOptions {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(now)));
    ImportOptions {
        old_root: root.to_path_buf(),
        evorag_version: "0.5.0".into(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
        location: |_| Verdict::Ok,
        allow_unsafe_location: false,
        before_verify: None,
    }
}

fn options(root: &Path) -> ImportOptions {
    options_at(root, NOW)
}

/// データ根のファイルの (大きさ・更新時刻・中身)。ロックのファイルは除く (03 §12.4 の「読み方」)。
fn files(root: &Path) -> BTreeMap<String, (u64, std::time::SystemTime, Vec<u8>)> {
    fn walk(
        base: &Path,
        dir: &Path,
        out: &mut BTreeMap<String, (u64, std::time::SystemTime, Vec<u8>)>,
    ) {
        for e in std::fs::read_dir(dir).expect("read a directory") {
            let path = e.expect("an entry").path();
            if path.is_dir() {
                walk(base, &path, out);
                continue;
            }
            let rel = path
                .strip_prefix(base)
                .expect("under the base")
                .to_string_lossy()
                .replace('\\', "/");
            if ["lock", "lock.pid", "lock.pid.tmp"].contains(&rel.as_str()) {
                continue;
            }
            let m = std::fs::metadata(&path).expect("metadata");
            out.insert(
                rel,
                (
                    m.len(),
                    m.modified().expect("a modified time"),
                    std::fs::read(&path).expect("read"),
                ),
            );
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn meta_of(root: &Path) -> BTreeMap<String, String> {
    inspect_collect(root, kinds_v05(), None, &[])
        .expect("inspect")
        .meta
}

/// 照合まで済ませて止まった (1 つ目の改名の前に強制終了した) 状態を作る。改名の後の古い根のパスを返す。
fn stop_after_verification(old: &Path) -> PathBuf {
    let Imported {
        renamed_old_root,
        old_root_lock,
        new_root_lock,
        ..
    } = run(&options(old)).unwrap_or_else(|e| panic!("import failed: {e}"));
    drop((old_root_lock, new_root_lock));
    renamed_old_root
}

/// 何でも y と答え、問いを記録する。
fn yes(asked: &mut Vec<String>) -> impl FnMut(&Plan<'_>) -> bool + '_ {
    |p| {
        asked.push(match p {
            Plan::Import { redo, .. } => format!("import redo={redo}"),
            Plan::Continue { .. } => "continue".to_owned(),
            _ => "other".to_owned(),
        });
        true
    }
}

fn import_yes(old: &Path) -> (Finished, Vec<String>) {
    let mut asked = Vec::new();
    let done = import_and_swap(&options(old), &mut yes(&mut asked))
        .unwrap_or_else(|e| panic!("import-v04 failed: {e}"));
    (done, asked)
}

/// 入れ替えた後の形: 元の名前に新しい根 (形式の版 0.5、照合の結果あり)、改名した古い根は無傷、`.v05-import` は無い。
fn assert_swapped(
    root: &Path,
    renamed: &Path,
    before: &BTreeMap<String, (u64, std::time::SystemTime, Vec<u8>)>,
) {
    let m = meta_of(root);
    assert_eq!(m.get(meta::FORMAT_VERSION).map(String::as_str), Some("0.5"));
    assert!(m.contains_key(meta::IMPORT_REPORT));
    let source: serde_json::Value =
        serde_json::from_str(&m[meta::IMPORT_SOURCE]).expect("import.source");
    assert_eq!(
        source["path"].as_str().map(PathBuf::from).as_deref(),
        Some(renamed),
        "import.source names the renamed old root"
    );
    assert_eq!(&files(renamed), before, "the old root is intact");
    assert!(!import_dir(root).expect("a named root").exists());
}

/// 取り込みが通ると、古い根を `<名前>-v0.4-<UTC>` に、新しい根を元の名前に改名する。問いは 1 度だけ。
///
/// verifies: AC-FR-219-3
#[test]
fn the_import_swaps_the_new_root_into_the_original_name() {
    let (_tmp, old) = copied_fixture("swap-ok");
    let before = files(&old);
    let (done, asked) = import_yes(&old);
    assert_eq!(asked, vec!["import redo=false"]);
    assert_eq!(done.root, std::path::absolute(&old).expect("absolute"));
    assert!(
        done.renamed_old_root
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("evorag-v0.4-")),
        "{}",
        done.renamed_old_root.display()
    );
    assert!(done.report.is_some());
    assert!(!done.redone);
    // 新しい根のロックを返す (TUI がそのまま続けるため)。
    assert!(matches!(
        DataRootLock::acquire(&old),
        Err(evorag_store::lock::LockError::InUse { .. })
    ));
    let renamed = done.renamed_old_root.clone();
    drop(done);
    assert_swapped(&old, &renamed, &before);
    // 入れ替えた後の根は、もう取り込まない (取り込みは要らないと示して 1)。
    let err =
        import_and_swap(&options(&old), &mut |_: &Plan<'_>| true).expect_err("already imported");
    assert!(
        matches!(err, ImportError::Refused(Refusal::NewerFormat { .. })),
        "{err}"
    );
    assert_eq!(err.exit_code(), 1);
    assert!(matches!(resume_on_startup(&old), Ok(Resumed::Nothing)));
}

/// 確認で y 以外を答えると、何も変えずに 1 で終わる。
#[test]
fn declining_changes_nothing() {
    let (_tmp, old) = copied_fixture("swap-declined");
    let before = files(&old);
    let err = import_and_swap(&options(&old), &mut |_: &Plan<'_>| false).expect_err("declined");
    assert!(matches!(err, ImportError::Declined));
    assert_eq!(err.exit_code(), 1);
    assert_eq!(files(&old), before);
    assert!(!import_dir(&old).expect("named").exists());
}

/// 照合の後 (1 つ目の改名の前) に止まったら、次の `import-v04` が比べてから入れ替えを続ける。
///
/// verifies: AC-FR-219-3
#[test]
fn a_swap_stopped_before_the_first_rename_is_continued_by_the_next_import() {
    let (_tmp, old) = copied_fixture("swap-before-first");
    let before = files(&old);
    let renamed = stop_after_verification(&old);
    assert_eq!(files(&old), before);
    let mut asked = Vec::new();
    let done = import_and_swap(&options_at(&old, NOW + 60_000), &mut yes(&mut asked))
        .expect("continue the swap");
    assert_eq!(asked, vec!["continue"]);
    assert!(done.report.is_none());
    // 前の取り込みが決めた名前に改名する (後の時刻の名前にしない)。
    assert_eq!(done.renamed_old_root, renamed);
    drop(done);
    assert_swapped(&old, &renamed, &before);
}

/// 照合の後に止まったら、次の起動も入れ替えを続ける (尋ねない)。
///
/// verifies: AC-FR-219-3
#[test]
fn a_swap_stopped_before_the_first_rename_is_continued_at_startup() {
    let (_tmp, old) = copied_fixture("swap-startup-first");
    let before = files(&old);
    let renamed = stop_after_verification(&old);
    match resume_on_startup(&old).expect("resume") {
        Resumed::Swapped {
            renamed_old_root,
            new_root_lock,
        } => {
            assert_eq!(renamed_old_root, renamed);
            drop(new_root_lock);
        }
        other => panic!("expected a swap, got {other:?}"),
    }
    assert_swapped(&old, &renamed, &before);
}

/// 2 つの改名の間に止まったら (元の名前のフォルダが無く、`.v05-import` に照合の結果がある)、次の起動と次の
/// `import-v04` が 2 つ目の改名を続ける。
///
/// verifies: AC-FR-219-3
#[test]
fn a_swap_stopped_between_the_renames_is_continued() {
    for by_import in [false, true] {
        let (_tmp, old) = copied_fixture("swap-between");
        let before = files(&old);
        let renamed = stop_after_verification(&old);
        std::fs::rename(&old, &renamed).expect("the first rename");
        assert!(!old.exists());
        if by_import {
            let mut asked = Vec::new();
            let done =
                import_and_swap(&options(&old), &mut yes(&mut asked)).expect("continue the swap");
            assert_eq!(asked, vec!["continue"]);
            assert_eq!(done.renamed_old_root, renamed);
        } else {
            assert!(matches!(
                resume_on_startup(&old).expect("resume"),
                Resumed::Swapped { .. }
            ));
        }
        assert_swapped(&old, &renamed, &before);
    }
}

/// 書いている途中で止まった (照合の結果の無い・`import-check` の残る) `.v05-import` は、作りかけとして消す。次の
/// `import-v04` は消してからやり直し、次の起動は消すだけ (古い根はそのまま)。
///
/// verifies: AC-FR-219-3
#[test]
fn an_unfinished_new_root_is_removed_and_the_import_is_redone() {
    let unfinished = [
        |new: &Path| raw_remove_meta(new, meta::IMPORT_REPORT),
        |new: &Path| std::fs::create_dir(new.join("import-check")).expect("a leftover check"),
        |new: &Path| std::fs::remove_file(new.join(DB_FILE)).expect("no database yet"),
    ];
    for make in unfinished {
        let (_tmp, old) = copied_fixture("swap-unfinished");
        let before = files(&old);
        let new = import_dir(&old).expect("named");
        stop_after_verification(&old);
        make(&new);
        assert!(matches!(
            resume_on_startup(&old).expect("clean up"),
            Resumed::RemovedUnfinished
        ));
        assert!(!new.exists());
        assert_eq!(files(&old), before, "the old root is untouched");

        stop_after_verification(&old);
        make(&new);
        let mut asked = Vec::new();
        let done = import_and_swap(&options_at(&old, NOW + 1_000), &mut yes(&mut asked))
            .expect("redo the import");
        assert_eq!(asked, vec!["import redo=true"]);
        assert!(done.redone);
        let renamed = done.renamed_old_root.clone();
        drop(done);
        assert_swapped(&old, &renamed, &before);
    }
}

/// 確認で断ったときは、作りかけも消さない (何も変えない)。
#[test]
fn declining_keeps_an_unfinished_new_root() {
    let (_tmp, old) = copied_fixture("swap-unfinished-declined");
    let new = import_dir(&old).expect("named");
    stop_after_verification(&old);
    raw_remove_meta(&new, meta::IMPORT_REPORT);
    let left = files(&new);
    let err = import_and_swap(&options(&old), &mut |_: &Plan<'_>| false).expect_err("declined");
    assert!(matches!(err, ImportError::Declined));
    assert_eq!(files(&new), left);
}

/// 照合の後に古い根を v0.4 で使った (改名の前の比べが合わない) ときは、`.v05-import` を消して作り直す。
///
/// verifies: AC-FR-219-3
#[test]
fn a_changed_old_root_makes_the_import_start_over() {
    let (_tmp, old) = copied_fixture("swap-stale-first");
    let stale = stop_after_verification(&old);
    let f = old.join("index/memory/chunk-lex/CURRENT");
    let mut bytes = std::fs::read(&f).expect("read");
    bytes.push(b'\n');
    std::fs::write(&f, bytes).expect("v0.4 wrote to the old root");
    let before = files(&old);
    let mut asked = Vec::new();
    let done = import_and_swap(&options_at(&old, NOW + 120_000), &mut yes(&mut asked))
        .expect("start over");
    assert_eq!(asked, vec!["import redo=true"]);
    assert!(done.redone);
    assert_ne!(
        done.renamed_old_root, stale,
        "a new import names a new time"
    );
    let renamed = done.renamed_old_root.clone();
    drop(done);
    assert_swapped(&old, &renamed, &before);
}

/// 2 つの改名の間に古い根を使ったときは、次の起動が古い根を元の名前に戻して `.v05-import` を消し、次の
/// `import-v04` は戻してから取り込み直す。
///
/// verifies: AC-FR-219-3
#[test]
fn a_changed_old_root_between_the_renames_is_put_back() {
    for by_import in [false, true] {
        let (_tmp, old) = copied_fixture("swap-stale-between");
        let renamed = stop_after_verification(&old);
        std::fs::rename(&old, &renamed).expect("the first rename");
        let f = renamed.join("index/memory/chunk-lex/CURRENT");
        let mut bytes = std::fs::read(&f).expect("read");
        bytes.push(b'\n');
        std::fs::write(&f, bytes).expect("v0.4 wrote to the renamed old root");
        let before = files(&renamed);
        if by_import {
            let mut asked = Vec::new();
            let done = import_and_swap(&options_at(&old, NOW + 120_000), &mut yes(&mut asked))
                .expect("start over");
            assert_eq!(asked, vec!["import redo=true"]);
            let again = done.renamed_old_root.clone();
            drop(done);
            assert!(!renamed.exists());
            assert_swapped(&old, &again, &before);
        } else {
            assert!(matches!(
                resume_on_startup(&old).expect("clean up"),
                Resumed::RemovedStale
            ));
            assert!(!renamed.exists());
            assert_eq!(files(&old), before, "the old root is back at its name");
            assert!(!import_dir(&old).expect("named").exists());
        }
    }
}

/// どちらかの根をほかのプロセスが使っていれば、触れずに 2 で終わる。
#[test]
fn a_locked_root_is_left_alone_with_exit_code_2() {
    let (_tmp, old) = copied_fixture("swap-locked");
    let new = import_dir(&old).expect("named");
    stop_after_verification(&old);
    let before = (files(&old), files(&new));
    for held_root in [&new, &old] {
        let held = DataRootLock::acquire(held_root).expect("another evorag");
        let err = import_and_swap(&options(&old), &mut |_: &Plan<'_>| true).expect_err("locked");
        assert!(matches!(err, ImportError::Locked(_)), "{err}");
        assert_eq!(err.exit_code(), 2);
        assert!(matches!(
            resume_on_startup(&old),
            Err(ImportError::Locked(_))
        ));
        drop(held);
        assert_eq!((files(&old), files(&new)), before);
    }
}

/// evorag の作らないもののある `.v05-import` には触れない (`import-v04` は 1、起動は無視する)。
#[test]
fn a_foreign_import_folder_is_never_removed() {
    let (_tmp, old) = copied_fixture("swap-foreign");
    let new = import_dir(&old).expect("named");
    std::fs::create_dir(&new).expect("a folder");
    std::fs::write(new.join("notes.txt"), b"mine").expect("write");
    let err = import_and_swap(&options(&old), &mut |_: &Plan<'_>| true).expect_err("foreign");
    assert!(matches!(err, ImportError::Leftover(_)), "{err}");
    assert_eq!(err.exit_code(), 1);
    assert!(matches!(resume_on_startup(&old), Ok(Resumed::Nothing)));
    assert_eq!(std::fs::read(new.join("notes.txt")).expect("kept"), b"mine");
    assert!(!new.join("lock").exists(), "not even locked");
}

/// 照合を終えた `.v05-import` の古い根がどこにも無ければ、何も変えずに断る。
#[test]
fn a_missing_old_root_stops_the_swap() {
    let (tmp, old) = copied_fixture("swap-missing");
    stop_after_verification(&old);
    std::fs::rename(&old, tmp.path().join("moved-away")).expect("move the old root away");
    let err = import_and_swap(&options(&old), &mut |_: &Plan<'_>| true).expect_err("missing");
    assert!(matches!(err, ImportError::OldRootMissing(_)), "{err}");
    assert!(matches!(
        resume_on_startup(&old),
        Err(ImportError::OldRootMissing(_))
    ));
    assert!(import_dir(&old).expect("named").exists());
}
