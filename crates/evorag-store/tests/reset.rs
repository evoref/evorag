//! `evorag reset [--old]` の消し方 (03 §10.2、FR-218): 対象の根とその退避だけを消し、ほかの根の退避・利用者の
//! ファイル・設定には触れない。リンクはたどらない。途中で止まっても、次の `reset` が続きを消す。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use evorag_store::import_v04::FileSnapshot;
use evorag_store::lock::{DataRootLock, LOCK_FILE};
use evorag_store::reset::{
    Done, ImportSource, Prepared, ResetError, ResetOptions, Target, deleting_leftovers, prepare,
};
use evorag_store::root_rename::read_meta;
use evorag_store::testutil::{TempDir, raw_set_meta};
use evorag_store::{DB_FILE, JOURNAL_DIR, OpenOptions, Store, meta};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};

/// 2026-11-27 00:00 UTC。
const NOW: i64 = 1_795_737_600_000;

/// データ根 `root` を作り (閉じる)、`backups` に日次の退避を 1 つ作る (`backups` が `None` なら根の中)。
fn make_root(root: &Path, seed: u64, backups: Option<&Path>) -> String {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(NOW)));
    let (store, _) = Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.5.0".into(),
        kinds: Vec::new(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
        config_file: None,
    })
    .expect("create a data root");
    store
        .daily_backup(backups)
        .expect("a daily backup")
        .expect("made today");
    drop(store);
    root_id(root)
}

fn root_id(root: &Path) -> String {
    read_meta(root).expect("meta")[meta::DATA_ROOT_ID].clone()
}

/// `dir` の日次の退避のフォルダ (名前の順)。
fn daily(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir.join("daily"))
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn ids_of(backups: &[PathBuf]) -> Vec<Option<String>> {
    backups
        .iter()
        .map(|b| evorag_store::backup::backup_root_id(b))
        .collect()
}

fn current<'a>(
    root: &'a Path,
    backups: Option<&'a Path>,
    keep: &'a [&'a Path],
) -> ResetOptions<'a> {
    ResetOptions {
        target: Target::Current(root),
        backup_dir: backups,
        keep,
        now_ms: NOW,
    }
}

/// 処分の印を書かない (`--old` でない)。
fn run_current(p: Prepared) -> Done {
    p.run(&mut |v| panic!("no mark for the current root, got {v}"))
        .expect("reset")
}

fn siblings(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .expect("read")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
        .collect();
    v.sort();
    v
}

/// 今の根を消す。DB・ジャーナル・中の退避と、外の退避の置き場所のこの根の退避が消え、ほかの根の退避と、利用者の
/// 置いたもの・設定は残る。消した後に開くと、新しいデータ根として始まる。
///
/// verifies: AC-FR-218-1 (設定・ほかの根の退避は残る)
#[test]
fn reset_removes_the_root_and_only_its_own_backups() {
    let tmp = TempDir::new("reset-current");
    let shared = tmp.path().join("shared");
    let root = tmp.path().join("data");
    let other = tmp.path().join("other");
    let id = make_root(&root, 1, Some(&shared));
    let other_id = make_root(&other, 2, Some(&shared));
    assert_ne!(id, other_id);
    // 根の中にも退避を 1 つ (別の日)。
    {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(NOW + 86_400_000)));
        let (store, _) = Store::open(OpenOptions {
            data_root: root.clone(),
            evorag_version: "0.5.0".into(),
            kinds: Vec::new(),
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock, 3)),
            config_file: None,
        })
        .expect("open");
        store.daily_backup(None).expect("backup").expect("made");
    }
    let notes = shared.join("daily").join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(notes.join("memo.txt"), b"mine").unwrap();
    let config = tmp.path().join("config").join("config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, b"").unwrap();

    let keep = [config.as_path()];
    let p = prepare(&current(&root, Some(&shared), &keep)).expect("prepare");
    assert!(p.target_exists());
    assert_eq!(p.inside_backups(), 2, "daily and weekly");
    assert_eq!(p.outside_backups(), 2, "daily and weekly");
    assert_eq!(p.leftovers().count(), 0);
    let done = run_current(p);
    assert!(done.left.is_empty(), "{done:?}");
    assert_eq!(done.backups_removed, 2);
    assert_eq!(done.disposed, None);
    assert!(!root.exists());
    assert_eq!(siblings(tmp.path()), ["config", "other", "shared"]);
    assert_eq!(ids_of(&daily(&shared)), [Some(other_id.clone()), None]);
    assert!(notes.join("memo.txt").is_file(), "a user's folder is left");
    assert!(config.is_file(), "the config is kept");
    assert_eq!(root_id(&other), other_id);

    // 次に開くと、新しいデータ根 (別の ID) として始まる。
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(NOW)));
    let (store, _) = Store::open(OpenOptions {
        data_root: root.clone(),
        evorag_version: "0.5.0".into(),
        kinds: Vec::new(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 9)),
        config_file: None,
    })
    .expect("a new data root");
    assert_eq!(store.meta(meta::PROMPT_LANGUAGE).unwrap(), None);
    drop(store);
    assert_ne!(root_id(&root), id);
}

/// ほかの evorag がロックを持っていれば、何も消さずに 2 で断る。
#[test]
fn a_locked_root_is_refused_with_exit_code_2() {
    let tmp = TempDir::new("reset-locked");
    let root = tmp.path().join("data");
    make_root(&root, 1, None);
    let held = DataRootLock::acquire(&root).unwrap();
    let err = prepare(&current(&root, None, &[])).expect_err("locked");
    assert!(
        matches!(&err, ResetError::Locked { pid: Some(p), .. } if *p == std::process::id()),
        "{err}"
    );
    assert_eq!(err.exit_code(), 2);
    drop(held);
    assert!(root.join(DB_FILE).is_file());
}

/// DB の無いフォルダ・無いパスは、データ根でないとして 1 で断り、ロックのファイルも作らない。
///
/// verifies: AC-FR-218-1 (データ根でないものを消さない)
#[test]
fn a_folder_without_a_database_is_not_a_data_root() {
    let tmp = TempDir::new("reset-not-root");
    let dir = tmp.path().join("mine");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("memo.txt"), b"mine").unwrap();
    for p in [dir.clone(), tmp.path().join("missing")] {
        let err = prepare(&current(&p, None, &[])).expect_err("not a data root");
        assert!(matches!(err, ResetError::NotDataRoot { .. }), "{err}");
        assert_eq!(err.exit_code(), 1);
    }
    assert_eq!(siblings(&dir), ["memo.txt"]);
}

/// 設定のファイル (や状態のフォルダ) を中に持つ根は消さない (設定は残す)。
#[test]
fn a_root_holding_the_config_is_refused() {
    let tmp = TempDir::new("reset-keep");
    let root = tmp.path().join("data");
    make_root(&root, 1, None);
    let config = root.join("config").join("config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, b"").unwrap();
    let keep = [config.as_path()];
    let err = prepare(&current(&root, None, &keep)).expect_err("holds the config");
    assert!(matches!(err, ResetError::Overlaps { .. }), "{err}");
    assert!(root.join(DB_FILE).is_file());
    // 根の祖先を指す設定のフォルダは、根の中に無いので妨げない。
    let keep = [tmp.path()];
    prepare(&current(&root, None, &keep)).expect("the config is outside the root");
}

/// 根の中のリンク (シンボリックリンク・ジャンクション) は、その先をたどらずにリンクだけを消す。根そのものが
/// リンクなら、何も消さずに断る。
///
/// verifies: AC-FR-218-1 (リンクの先を消さない)
#[test]
fn links_are_not_followed() {
    let tmp = TempDir::new("reset-links");
    let root = tmp.path().join("data");
    make_root(&root, 1, None);
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), b"keep").unwrap();
    if !make_dir_link(&outside, &root.join("link")) {
        eprintln!("skipped: this system cannot make a directory link here");
        return;
    }
    make_dir_link(&outside, &root.join(JOURNAL_DIR).join("link"));
    let linked_root = tmp.path().join("linked-root");
    assert!(make_dir_link(&root, &linked_root));
    let err = prepare(&current(&linked_root, None, &[])).expect_err("a link");
    assert!(matches!(err, ResetError::Link { .. }), "{err}");
    std::fs::remove_dir(&linked_root)
        .or_else(|_| std::fs::remove_file(&linked_root))
        .unwrap();

    let done = run_current(prepare(&current(&root, None, &[])).expect("prepare"));
    assert!(done.left.is_empty(), "{done:?}");
    assert!(!root.exists());
    assert!(
        outside.join("keep.txt").is_file(),
        "the link target is left"
    );
}

/// (1) の改名の後に止まった・(3) の途中で止まったフォルダは、次の `reset` が続きを消す (今の根がもう無くても)。
/// evorag の作らないものを持つ消しかけのフォルダには触れない。
///
/// verifies: AC-FR-218-1 (途中で強制終了しても、次の reset で続きが終わる)
#[test]
fn an_interrupted_reset_is_finished_by_the_next_one() {
    let tmp = TempDir::new("reset-resume");
    let shared = tmp.path().join("shared");
    let root = tmp.path().join("data");
    let other = tmp.path().join("other");
    make_root(&root, 1, Some(&shared));
    let other_id = make_root(&other, 2, Some(&shared));
    // (1) の後: 改名しただけ。
    let renamed = tmp.path().join("data.deleting-20261127T000000000Z");
    std::fs::rename(&root, &renamed).unwrap();
    // (3) の途中: DB は消え、ジャーナルと索引が残る。
    let half = tmp.path().join("data.deleting-20261126T000000000Z");
    std::fs::create_dir_all(half.join(JOURNAL_DIR)).unwrap();
    std::fs::write(half.join(JOURNAL_DIR).join("0001.jsonl"), b"{}\n").unwrap();
    std::fs::create_dir_all(half.join("index")).unwrap();
    std::fs::write(half.join(LOCK_FILE), b"").unwrap();
    assert_eq!(deleting_leftovers(&root), [half.clone(), renamed.clone()]);

    let p = prepare(&current(&root, Some(&shared), &[])).expect("prepare");
    assert!(!p.target_exists());
    assert_eq!(
        p.leftovers().collect::<Vec<_>>(),
        [half.as_path(), renamed.as_path()]
    );
    assert_eq!(p.outside_backups(), 2, "daily and weekly");
    let done = run_current(p);
    assert!(done.left.is_empty(), "{done:?}");
    assert_eq!(done.removed, [half.clone(), renamed.clone()]);
    assert_eq!(ids_of(&daily(&shared)), [Some(other_id)]);
    assert_eq!(siblings(tmp.path()), ["other", "shared"]);

    // 2 回目: もう何も無い。
    let err = prepare(&current(&root, Some(&shared), &[])).expect_err("nothing left");
    assert!(matches!(err, ResetError::NotDataRoot { .. }), "{err}");

    // evorag の作らないものを持つ消しかけのフォルダには触れない。
    let foreign = tmp.path().join("data.deleting-mine");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("memo.txt"), b"mine").unwrap();
    let err = prepare(&current(&root, None, &[])).expect_err("foreign");
    assert!(matches!(err, ResetError::UnknownLeftover { .. }), "{err}");
    assert!(foreign.join("memo.txt").is_file());
}

/// 古い根の試験の場所: `<一時>/data` (今の根) と、`<一時>/data-v0.4-<日時>` (取り込み元の古い根)。外の退避は
/// `<一時>/shared` に共有する。
struct Imported {
    tmp: TempDir,
    current: PathBuf,
    old: PathBuf,
    shared: PathBuf,
    source: ImportSource,
    current_id: String,
}

fn imported(label: &str) -> Imported {
    let tmp = TempDir::new(label);
    let shared = tmp.path().join("shared");
    let current = tmp.path().join("data");
    let old = tmp.path().join("data-v0.4-20261126T000000000Z");
    let old_id = make_root(&old, 1, Some(&shared));
    let current_id = make_root(&current, 2, Some(&shared));
    let files = FileSnapshot::take(&old).unwrap();
    let seq = read_meta(&old).unwrap()[meta::JOURNAL_LAST_SEQ]
        .parse()
        .unwrap();
    let source = ImportSource {
        data_root_id: old_id,
        // 記録したパスはドライブの文字が変わったなどで古くても、今の根の親と名前で見つかる。
        path: PathBuf::from("Z:/moved").join(old.file_name().unwrap()),
        journal_last_seq: seq,
        files: files.len(),
        files_sha256: files.digest(),
    };
    Imported {
        tmp,
        current,
        old,
        shared,
        source,
        current_id,
    }
}

impl Imported {
    fn opts(&self) -> ResetOptions<'_> {
        ResetOptions {
            target: Target::Old {
                current_root: &self.current,
                source: &self.source,
            },
            backup_dir: Some(&self.shared),
            keep: &[],
            now_ms: NOW,
        }
    }

    /// 消して、処分の印の値を返す。
    fn run(&self, p: Prepared) -> (Done, Vec<String>) {
        let marks = RefCell::new(Vec::new());
        let done = p
            .run(&mut |v| {
                marks.borrow_mut().push(v.to_owned());
                Ok(())
            })
            .expect("reset --old");
        (done, marks.into_inner())
    }
}

/// `--old`: 古い根と、共有した外の退避のうち古い根の退避を消し、最後に処分の印を書かせる。今の根とその退避は
/// 残る。
///
/// verifies: AC-FR-218-1 (`--old` は import.source の ID の根を消す), AC-FR-220-1 (ストアの層: 古い根と外の
/// 退避の置き場所の古い根の退避が消え、処分の印を書く)
#[test]
fn reset_old_removes_the_old_root_and_its_backups_then_marks_it_disposed() {
    let t = imported("reset-old");
    let p = prepare(&t.opts()).expect("prepare");
    assert_eq!(p.target(), t.old);
    assert_eq!(p.outside_backups(), 2, "daily and weekly");
    let (done, marks) = t.run(p);
    assert!(done.left.is_empty(), "{done:?}");
    let disposed = format!("{}{NOW}", meta::IMPORT_OLD_ROOT_DISPOSED);
    assert_eq!(marks, std::slice::from_ref(&disposed));
    assert_eq!(done.disposed.as_deref(), Some(disposed.as_str()));
    assert!(!t.old.exists());
    assert_eq!(siblings(t.tmp.path()), ["data", "shared"]);
    assert_eq!(ids_of(&daily(&t.shared)), [Some(t.current_id.clone())]);
    assert_eq!(root_id(&t.current), t.current_id);
}

/// `--old`: 取り込んだときから古い根が変わっていれば (取り込みの後に v0.4 で使った)、消さずに、増えたジャーナルの
/// 行の数を示して断る (手順の (0))。
#[test]
fn reset_old_refuses_an_old_root_used_after_the_import() {
    let t = imported("reset-old-changed");
    raw_set_meta(
        &t.old,
        meta::JOURNAL_LAST_SEQ,
        &(t.source.journal_last_seq + 3).to_string(),
    );
    let err = prepare(&t.opts()).expect_err("changed");
    assert!(
        matches!(err, ResetError::OldRootChanged { lines: 3, .. }),
        "{err}"
    );
    assert_eq!(err.exit_code(), 1);

    // 連番が同じでも、ファイルが変わっていれば断る。
    let t = imported("reset-old-changed-files");
    std::fs::write(t.old.join(JOURNAL_DIR).join("extra.jsonl"), b"{}\n").unwrap();
    let err = prepare(&t.opts()).expect_err("changed");
    assert!(
        matches!(err, ResetError::OldRootChanged { lines: 0, .. }),
        "{err}"
    );
    assert!(t.old.join(DB_FILE).is_file());
    assert_eq!(daily(&t.shared).len(), 2);
}

/// `--old`: `import.source` の ID と合わない根、今の根そのもの・その祖先は消さない。古い根のロックが取れなければ 2。
///
/// verifies: AC-FR-218-1 (今の根の祖先・子孫を消さない。import.source の ID と合わない根を消さない)
#[test]
fn reset_old_refuses_other_roots_the_current_root_and_its_ancestors() {
    let t = imported("reset-old-refused");
    let mut source = t.source.clone();
    source.data_root_id = "another".into();
    let opts = |source: &ImportSource| {
        prepare(&ResetOptions {
            target: Target::Old {
                current_root: &t.current,
                source,
            },
            backup_dir: Some(&t.shared),
            keep: &[],
            now_ms: NOW,
        })
    };
    let err = opts(&source).expect_err("another root");
    assert!(matches!(err, ResetError::IdMismatch { .. }), "{err}");

    // 記録したパスが今の根・その親 (名前が `data-v0.4-` で始まらないので、記録のパスをそのまま使う)。
    for path in [t.current.clone(), t.tmp.path().to_path_buf()] {
        let mut source = t.source.clone();
        source.path = path;
        let err = opts(&source).expect_err("overlaps the current root");
        assert!(matches!(err, ResetError::Overlaps { .. }), "{err}");
    }
    // 今の根の中の根 (子孫)。
    let inner = t.current.join("inner");
    make_root(&inner, 5, None);
    let mut source = t.source.clone();
    source.path = inner.clone();
    source.data_root_id = root_id(&inner);
    let err = opts(&source).expect_err("inside the current root");
    assert!(matches!(err, ResetError::Overlaps { .. }), "{err}");

    let held = DataRootLock::acquire(&t.old).unwrap();
    let err = opts(&t.source).expect_err("v0.4 uses the old root");
    assert_eq!(err.exit_code(), 2, "{err}");
    drop(held);
    assert!(t.old.join(DB_FILE).is_file());
    assert!(t.current.join(DB_FILE).is_file());
    assert_eq!(daily(&t.shared).len(), 2);
}

/// `--old` が改名の後に止まったら、次の `--old` が消しかけのフォルダを消し、処分の印を書かせる。古い根も消しかけも
/// 無ければ (消し終えた後に印を書けなかった)、外の退避を消して印を書かせる。別の根の消しかけには触れない。
///
/// verifies: AC-FR-218-1 (`--old` の途中の強制終了)
#[test]
fn reset_old_finishes_an_interrupted_disposal() {
    let t = imported("reset-old-resume");
    let renamed = t
        .tmp
        .path()
        .join("data-v0.4-20261126T000000000Z.deleting-20261127T000000000Z");
    std::fs::rename(&t.old, &renamed).unwrap();
    let p = prepare(&t.opts()).expect("prepare");
    assert!(!p.target_exists());
    assert_eq!(p.leftovers().collect::<Vec<_>>(), [renamed.as_path()]);
    let (done, marks) = t.run(p);
    assert!(done.left.is_empty(), "{done:?}");
    assert_eq!(marks.len(), 1);
    assert_eq!(siblings(t.tmp.path()), ["data", "shared"]);
    assert_eq!(ids_of(&daily(&t.shared)), [Some(t.current_id.clone())]);

    // 古い根も消しかけも無い: 印を書くだけ。
    let p = prepare(&t.opts()).expect("prepare");
    assert!(!p.target_exists());
    let (done, marks) = t.run(p);
    assert!(done.removed.is_empty() && done.left.is_empty(), "{done:?}");
    assert_eq!(marks.len(), 1);

    // 別の根 (ID が違う) の消しかけには触れない。
    let foreign = t
        .tmp
        .path()
        .join("data-v0.4-20261126T000000000Z.deleting-x");
    make_root(&foreign, 7, None);
    let err = prepare(&t.opts()).expect_err("foreign leftover");
    assert!(matches!(err, ResetError::UnknownLeftover { .. }), "{err}");
    assert!(foreign.join(DB_FILE).is_file());
    // doctor は今の根の隣の消しかけを示す (古い根の名前のものも)。
    assert_eq!(deleting_leftovers(&t.current), [foreign]);
}

#[cfg(unix)]
fn make_dir_link(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

/// Windows ではジャンクション (管理者の権限が要らない) を作る。
#[cfg(windows)]
fn make_dir_link(target: &Path, link: &Path) -> bool {
    std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|o| o.status.success())
}
