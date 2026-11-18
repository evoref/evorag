//! `evorag backups purge` の消し方 (03 §7.3・§10、FR-213): すべての退避と作りかけの退避を消し、リンクはたどらず、
//! データ根に見えるフォルダは消さない。

use std::path::{Path, PathBuf};

use evorag_store::restore::{list_backups, purge_all_backups};
use evorag_store::testutil::TempDir;

/// DB の写しを持つ退避のフォルダ (中身は試験の手で置いたファイル。DB として開けなくても数える)。
fn fake_backup(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(evorag_store::DB_FILE), b"copy").unwrap();
    dir.to_path_buf()
}

/// 作りかけの退避 (写しの一時名だけを持つフォルダ)。
fn partial_backup(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("evorag.partial"), b"half").unwrap();
    dir.to_path_buf()
}

/// verifies: AC-FR-213-1 (日次・週次・移行と作り直しの前の退避と、作りかけの退避を消す。利用者のほかのフォルダには
/// 触れない)
#[test]
fn purge_removes_every_backup_and_partial_backups() {
    let dir = TempDir::new("purge-all");
    let root = dir.path().join("data");
    let backups = root.join("backups");
    let daily = fake_backup(&backups.join("daily").join("2026-11-01"));
    let weekly = fake_backup(&backups.join("weekly").join("2026-W44"));
    let pre_op = fake_backup(&backups.join("pre-op").join("20261101-repair-v0.3.0"));
    let partial = partial_backup(&backups.join("daily").join("2026-11-02"));
    let mine = backups.join("daily").join("notes");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::write(mine.join("memo.txt"), b"mine").unwrap();
    assert_eq!(list_backups(&root, None).len(), 3);

    let purged = purge_all_backups(&root, None);
    assert_eq!(purged.removed, 3, "{purged:?}");
    assert!(purged.failed.is_empty(), "{purged:?}");
    for d in [&daily, &weekly, &pre_op, &partial] {
        assert!(!d.exists(), "{}", d.display());
    }
    assert!(mine.join("memo.txt").is_file(), "a user's folder is left");
    assert!(list_backups(&root, None).is_empty());
}

/// `storage.backup_dir` を指定していれば、そこの退避と、データ根の `backups/pre-op/` の退避の両方を消す。消す退避が
/// 0 個でも、作りかけの退避は消す。
///
/// verifies: AC-FR-213-1 (storage.backup_dir と、退避が 0 個のときの作りかけの退避)
#[test]
fn purge_covers_the_backup_dir_and_the_data_roots_pre_op() {
    let dir = TempDir::new("purge-elsewhere");
    let root = dir.path().join("data");
    let elsewhere = dir.path().join("elsewhere");
    let daily = fake_backup(&elsewhere.join("daily").join("2026-11-01"));
    let pre_op = fake_backup(&root.join("backups").join("pre-op").join("20261101-migrate"));
    assert_eq!(list_backups(&root, Some(&elsewhere)).len(), 2);
    let purged = purge_all_backups(&root, Some(&elsewhere));
    assert_eq!(purged.removed, 2, "{purged:?}");
    assert!(!daily.exists() && !pre_op.exists());

    // 退避が 0 個で、作りかけの退避だけがある。
    let partial = partial_backup(&elsewhere.join("weekly").join("2026-W45"));
    assert!(list_backups(&root, Some(&elsewhere)).is_empty());
    let purged = purge_all_backups(&root, Some(&elsewhere));
    assert_eq!(purged.removed, 0, "{purged:?}");
    assert!(purged.failed.is_empty(), "{purged:?}");
    assert!(!partial.exists());
}

/// データ根に見えるフォルダ (ジャーナルのフォルダかロックのファイルを持つもの・データ根そのもの) は、退避の置き場所
/// にあっても消さず、理由を返す。一覧にも出さない。
///
/// verifies: AC-FR-213-1 (データ根を消さない)
#[test]
fn purge_refuses_folders_that_look_like_a_data_root() {
    let dir = TempDir::new("purge-root");
    let elsewhere = dir.path().join("elsewhere");
    let with_journal = fake_backup(&elsewhere.join("daily").join("2026-11-01"));
    std::fs::create_dir_all(with_journal.join(evorag_store::JOURNAL_DIR)).unwrap();
    let with_lock = fake_backup(&elsewhere.join("weekly").join("2026-W44"));
    std::fs::write(with_lock.join(evorag_store::lock::LOCK_FILE), b"").unwrap();
    // `storage.backup_dir` の `daily/` の下にデータ根そのものがある。
    let nested_root = elsewhere.join("daily").join("root");
    fake_backup(&nested_root);
    let ok = fake_backup(&elsewhere.join("daily").join("2026-11-02"));

    assert_eq!(list_backups(&nested_root, Some(&elsewhere)).len(), 1);
    let purged = purge_all_backups(&nested_root, Some(&elsewhere));
    assert_eq!(purged.removed, 1, "{purged:?}");
    assert!(!ok.exists());
    assert_eq!(purged.failed.len(), 3, "{purged:?}");
    assert!(
        purged
            .failed
            .iter()
            .all(|f| f.starts_with("data root, not deleted: ")),
        "{purged:?}"
    );
    assert!(with_journal.join(evorag_store::DB_FILE).is_file());
    assert!(with_lock.join(evorag_store::DB_FILE).is_file());
    assert!(nested_root.join(evorag_store::DB_FILE).is_file());
}

/// リンク (シンボリックリンク・ジャンクション) の退避はたどらずに残し、理由を返す。リンクの先は消さない。
///
/// verifies: AC-FR-213-1 (リンクをたどらない)
#[test]
fn purge_does_not_follow_links() {
    let dir = TempDir::new("purge-link");
    let root = dir.path().join("data");
    let target = fake_backup(&dir.path().join("target"));
    let daily = root.join("backups").join("daily");
    std::fs::create_dir_all(&daily).unwrap();
    let link = daily.join("2026-11-01");
    if !make_dir_link(&target, &link) {
        eprintln!("skipped: this system cannot make a directory link here");
        return;
    }
    assert!(list_backups(&root, None).is_empty());
    let purged = purge_all_backups(&root, None);
    assert_eq!(purged.removed, 0, "{purged:?}");
    assert_eq!(
        purged.failed,
        [format!("link, not deleted: {}", link.display())],
        "{purged:?}"
    );
    assert!(target.join(evorag_store::DB_FILE).is_file());
    assert!(std::fs::symlink_metadata(&link).is_ok(), "the link is left");
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
