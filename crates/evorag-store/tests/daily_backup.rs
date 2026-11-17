//! 自動の退避の試験 (03 §7.1、ADR-0007)。

use std::path::Path;
use std::sync::Arc;

use evorag_store::backup::{DAILY_DIR, WEEKLY_DIR};
use evorag_store::testutil::TempDir;
use evorag_store::{DB_FILE, Durability, Envelope, KindSpec, OpenOptions, Read, Store};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};
use serde::Serialize;

const SAMPLE: KindSpec = KindSpec::new("sample", 1);
const DAY: i64 = 86_400_000;

#[derive(Serialize)]
struct Body {
    n: u32,
}

fn open(root: &Path, clock: Arc<ManualClock>) -> Store {
    Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.5".into(),
        kinds: vec![SAMPLE],
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 2)),
        config_file: None,
    })
    .unwrap()
    .0
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// verifies: AC-FR-204-1
#[test]
fn one_backup_a_day_and_only_7_daily_and_4_weekly_are_kept() {
    let dir = TempDir::new("daily");
    // 2026-01-05 (月) 12:00 UTC から 40 日。
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_767_614_400_000)));
    let store = open(dir.path(), clock.clone());
    let mut made = 0;
    for _ in 0..40 {
        for _ in 0..3 {
            if store.daily_backup(None).unwrap().is_some() {
                made += 1;
            }
            clock.advance(3 * 3_600_000);
        }
        clock.advance(DAY - 9 * 3_600_000);
    }
    assert_eq!(made, 40, "exactly one backup per day");
    let backups = dir.path().join("backups");
    let daily = names(&backups.join(DAILY_DIR));
    assert_eq!(daily.len(), 7, "{daily:?}");
    assert_eq!(daily.last().unwrap(), "20260213");
    let weekly = names(&backups.join(WEEKLY_DIR));
    assert_eq!(weekly.len(), 4, "{weekly:?}");
    // 週の最初 (月曜) の日次が週次に残る。
    assert!(
        weekly.iter().all(|w| w.ends_with("20260119")
            || w.ends_with("20260126")
            || w.ends_with("20260202")
            || w.ends_with("20260209")),
        "{weekly:?}"
    );
}

/// verifies: AC-FR-204-2 (退避は単独で開け、書いている途中の記録を半端に含まない)
#[test]
fn a_backup_taken_while_writing_is_consistent_and_opens_on_its_own() {
    let dir = TempDir::new("daily-concurrent");
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_767_614_400_000)));
    let store = Arc::new(open(dir.path(), clock.clone()));
    let tok = store.take_write_token().unwrap();
    let committed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let writer = {
        let store = store.clone();
        let committed = committed.clone();
        std::thread::spawn(move || {
            for n in 0..300 {
                let mut tx = store.write(&tok, Durability::Immediate).unwrap();
                // 2 件を 1 つのトランザクションで書く (退避には 2 件そろって入るか、どちらも入らない)。
                for _ in 0..2 {
                    let mut e =
                        Envelope::new(store.ids().next_id(), "sample", 1, store.clock().now(), "");
                    e.set_body(&Body { n }).unwrap();
                    tx.put(&mut e).unwrap();
                }
                tx.commit().unwrap();
                committed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        })
    };
    // 書いている最中に退避を取る (負荷が高くても、書き始めてから取る)。
    while committed.load(std::sync::atomic::Ordering::SeqCst) < 10 {
        std::thread::yield_now();
    }
    let backup = store.daily_backup(None).unwrap().unwrap();
    writer.join().unwrap();

    // 写しだけを別のデータ根に置いて開く。
    let other = TempDir::new("daily-concurrent-copy");
    std::fs::copy(backup.join(DB_FILE), other.path().join(DB_FILE)).unwrap();
    let copy = open(other.path(), clock);
    let census = copy.census().unwrap();
    let c = census.known.get("sample").cloned().unwrap_or_default();
    assert_eq!(c.corrupt + c.unreadable, 0);
    assert_eq!(c.ok % 2, 0, "a transaction was split: {}", c.ok);
    let mut seen = 0;
    copy.read()
        .unwrap()
        .for_each("sample", |r| {
            assert!(matches!(r, Read::Ok(_)));
            seen += 1;
            true
        })
        .unwrap();
    assert_eq!(seen, c.ok);
}

/// 開けない退避があれば、整理は失敗を返す (呼び手が job を残して再試行する)。開ける退避は写し直す。
/// verifies: 03 §7.3、04 §7.7 の 5 (開けない退避があれば job を残す)
#[test]
fn a_purge_reports_a_backup_it_cannot_open() {
    let dir = TempDir::new("daily-purge-unopenable");
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_767_614_400_000)));
    let store = open(dir.path(), clock.clone());
    let tok = store.take_write_token().unwrap();
    let mut e = Envelope::new(store.ids().next_id(), "sample", 1, store.clock().now(), "");
    e.set_body(&Body { n: 7 }).unwrap();
    let gone = e.id;
    let mut tx = store.write(&tok, Durability::Immediate).unwrap();
    tx.put(&mut e).unwrap();
    tx.commit().unwrap();
    let good = store.daily_backup(None).unwrap().unwrap();
    // 壊れた退避 (前の日の分)。
    let bad = dir.path().join("backups").join(DAILY_DIR).join("20260101");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join(DB_FILE), b"not a database").unwrap();

    let r = store.purge_backups(None, &[gone]);
    assert!(r.is_err(), "{r:?}");
    let copy = TempDir::new("daily-purge-unopenable-copy");
    std::fs::copy(good.join(DB_FILE), copy.path().join(DB_FILE)).unwrap();
    let back = open(copy.path(), clock);
    assert_eq!(back.read().unwrap().get("sample", gone).unwrap(), None);
}
