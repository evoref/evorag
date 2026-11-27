//! 退避の置き場所 (`storage.backup_dir`) をほかの根と共有するときに、退避を根の ID で区別すること (03 §7.1 の
//! 「退避と根の ID」)。`import-v04` の古い根と新しい根が、同じ置き場所を使うときのため。

use std::path::Path;
use std::sync::Arc;

use evorag_store::backup::{DAILY_DIR, WEEKLY_DIR};
use evorag_store::restore::{BackupOwner, restore_candidates};
use evorag_store::testutil::TempDir;
use evorag_store::{DB_FILE, OpenOptions, Store, meta};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};

const DAY: i64 = 86_400_000;
/// 2026-11-23 (月曜) の正午。
const MONDAY: i64 = 1_795_435_200_000;

fn open(root: &Path, clock: Arc<ManualClock>, seed: u64) -> Store {
    Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.5.0".into(),
        kinds: Vec::new(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
        config_file: None,
    })
    .expect("open")
    .0
}

fn id_of(store: &Store) -> String {
    store
        .meta(meta::DATA_ROOT_ID)
        .expect("meta")
        .expect("a data root id")
}

fn dirs(base: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(base)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.path().join(DB_FILE).is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// 同じ日に 2 つの根が退避を作っても、どちらの日次も作られ、世代の整理はほかの根の退避を消さない。戻す候補は
/// その根の退避だけ。
#[test]
fn roots_sharing_a_backup_dir_keep_their_own_backups() {
    let tmp = TempDir::new("backup-owner");
    let shared = tmp.path().join("backups");
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(MONDAY)));
    let old = open(&tmp.path().join("old"), clock.clone(), 1);
    let new = open(&tmp.path().join("new"), clock.clone(), 2);
    let (old_id, new_id) = (id_of(&old), id_of(&new));
    assert_ne!(old_id, new_id);

    let old_daily = old
        .daily_backup(Some(&shared))
        .expect("backup")
        .expect("made");
    assert!(!old.daily_backup_due(Some(&shared)));
    // ほかの根の今日の退避は、この根の退避に数えない。
    assert!(new.daily_backup_due(Some(&shared)));
    let new_daily = new
        .daily_backup(Some(&shared))
        .expect("backup")
        .expect("made");
    assert_ne!(old_daily, new_daily);
    assert!(!new.daily_backup_due(Some(&shared)));
    assert_eq!(dirs(&shared.join(DAILY_DIR)).len(), 2);
    assert_eq!(
        dirs(&shared.join(WEEKLY_DIR)).len(),
        2,
        "each root has its weekly"
    );

    // 新しい根だけが 10 日続けて退避を作っても、古い根の退避は残る。
    for _ in 0..10 {
        clock.advance(DAY);
        new.daily_backup(Some(&shared)).expect("backup");
    }
    assert!(
        old_daily.join(DB_FILE).is_file(),
        "the old root's daily is kept"
    );
    assert_eq!(dirs(&shared.join(DAILY_DIR)).len(), 7 + 1);

    let new_root = new.data_root().to_path_buf();
    let candidates = restore_candidates(&new_root, Some(&shared), &BackupOwner::Id(new_id.clone()));
    assert!(!candidates.is_empty());
    assert!(
        candidates
            .iter()
            .all(|b| b.data_root_id.as_deref() == Some(new_id.as_str())),
        "only the new root's backups are offered"
    );
    // 根の ID が分からないときは、ほかの根と分かっている ID (取り込み元) の退避を除く。
    let unknown = BackupOwner::Unknown {
        not: vec![old_id.clone()],
    };
    let candidates = restore_candidates(&new_root, Some(&shared), &unknown);
    assert!(
        candidates
            .iter()
            .all(|b| b.data_root_id.as_deref() != Some(old_id.as_str()))
    );

    // ID の分からない退避は、候補の終わりに示し、整理で消さない。
    let odd = shared.join(DAILY_DIR).join("00000000");
    std::fs::create_dir_all(&odd).expect("mkdir");
    std::fs::write(odd.join(DB_FILE), b"not a database").expect("write");
    clock.advance(DAY);
    new.daily_backup(Some(&shared)).expect("backup");
    assert!(
        odd.join(DB_FILE).is_file(),
        "an unknown backup is never pruned"
    );
    let candidates = restore_candidates(&new_root, Some(&shared), &BackupOwner::Id(new_id));
    let last = candidates.last().expect("some");
    assert!(last.data_root_id.is_none() && last.dir == odd);
}
