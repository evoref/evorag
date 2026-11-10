//! 破損・復元・退避の整理の試験 (03 §7.3、§8、§10)。会話の種類とジャーナルを実際に使う。

use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use evorag_memory::*;
use evorag_store::restore::{list_backups, purge_backups, restore};
use evorag_store::testutil::TempDir;
use evorag_store::{
    DB_FILE, OpenError, OpenOptions, Read, Store, WriteOp, WriteRequest, Writer, WriterHandle,
};
use evorag_types::{Clock, Id, ManualClock, Timestamp, UuidV7Gen};

struct Env {
    store: Arc<Store>,
    writer: WriterHandle,
    thread: Option<JoinHandle<()>>,
    clock: Arc<ManualClock>,
    ids: Arc<UuidV7Gen>,
}

fn options(root: &Path, clock: Arc<ManualClock>, ids: Arc<UuidV7Gen>) -> OpenOptions {
    OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.5".into(),
        kinds: kinds(),
        clock,
        ids,
        config_file: None,
    }
}

impl Env {
    fn open(root: &Path, clock: Arc<ManualClock>) -> Result<Self, OpenError> {
        let ids = Arc::new(UuidV7Gen::with_seed(
            clock.clone(),
            clock.now().as_millis() as u64,
        ));
        let (store, _) = Store::open(options(root, clock.clone(), ids.clone()))?;
        let store = Arc::new(store);
        let (w, writer) = Writer::new(store.clone()).unwrap();
        let thread = Some(std::thread::spawn(move || w.run()));
        Ok(Self {
            store,
            writer,
            thread,
            clock,
            ids,
        })
    }
    fn writes(&self) -> Writes<'_> {
        Writes {
            clock: &*self.clock,
            ids: &*self.ids,
        }
    }
    fn write(&self, ops: Vec<WriteOp>) {
        self.writer
            .submit_wait(WriteRequest::conversation(ops))
            .unwrap();
    }
    fn session(&self, id: Id) -> evorag_store::Envelope {
        load_session(&self.store.read().unwrap(), id)
            .unwrap()
            .envelope
    }
    fn say(&self, sid: Id, text: &str) -> Id {
        let (u, ops) = self.writes().user_turn(&self.session(sid), text).unwrap();
        self.write(ops);
        self.clock.advance(1);
        u
    }
    fn texts(&self, sid: Id) -> Vec<String> {
        session_turns(&self.store.read().unwrap(), sid)
            .unwrap()
            .into_iter()
            .map(|t| t.body.text)
            .collect()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.writer.shutdown();
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(Timestamp::from_millis(1_767_614_400_000)))
}

/// verifies: AC-FR-205-1, AC-FR-208-1, AC-NFR-302-1
#[test]
fn a_broken_database_is_moved_aside_and_restored_from_the_backup_and_the_journal() {
    let dir = TempDir::new("restore-broken");
    let c = clock();
    let (sid, gone);
    {
        let env = Env::open(dir.path(), c.clone()).unwrap();
        let (id, ops) = env.writes().create_session("残す").unwrap();
        sid = id;
        env.write(ops);
        let (g, ops) = env.writes().create_session("後で消す").unwrap();
        gone = g;
        env.write(ops);
        env.say(sid, "退避の前");
        env.store.daily_backup(None).unwrap().unwrap();
        // 退避の後の会話と削除 (ジャーナルから足され、当て直されること)。
        env.say(sid, "退避の後");
        let ops = {
            let rtx = env.store.read().unwrap();
            delete_session(&rtx, &env.session(gone)).unwrap()
        };
        env.write(ops);
    }
    // DB のファイルを壊す。
    std::fs::write(dir.path().join(DB_FILE), b"this is not a redb file at all").unwrap();
    let moved_to = match Env::open(dir.path(), c.clone()) {
        Err(OpenError::Broken { moved_to, .. }) => moved_to,
        Err(e) => panic!("expected the broken database to be moved aside, got {e:?}"),
        Ok(_) => panic!("expected the broken database to be moved aside, got a store"),
    };
    assert!(moved_to.starts_with(dir.path().join("broken")));
    assert!(moved_to.is_file(), "the broken file is kept, not deleted");

    let latest = &list_backups(dir.path(), None)[0];
    restore(dir.path(), &latest.dir, c.now().as_millis(), None).unwrap();
    let env = Env::open(dir.path(), c).unwrap();
    // 退避の状態に、ジャーナルの差分が足される (重複も抜けも無い)。
    assert_eq!(env.texts(sid), ["退避の前", "退避の後"]);
    let rtx = env.store.read().unwrap();
    assert!(
        load_session(&rtx, gone).is_err(),
        "the delete after the backup is re-applied"
    );
}

/// 03 §10: restore の前に今の DB を退避する。
#[test]
fn restore_backs_up_the_current_database_first() {
    let dir = TempDir::new("restore-preop");
    let c = clock();
    {
        let env = Env::open(dir.path(), c.clone()).unwrap();
        let (sid, ops) = env.writes().create_session("s").unwrap();
        env.write(ops);
        env.say(sid, "a");
        env.store.daily_backup(None).unwrap();
    }
    let latest = list_backups(dir.path(), None)[0].dir.clone();
    let pre = restore(dir.path(), &latest, c.now().as_millis(), None).unwrap();
    let pre = pre.expect("a pre-op backup of the current database");
    assert!(pre.join(DB_FILE).is_file());
    assert!(
        pre.file_name()
            .unwrap()
            .to_string_lossy()
            .contains("-restore-")
    );
}

fn contains(file: &Path, text: &str) -> bool {
    let bytes = std::fs::read(file).unwrap();
    bytes.windows(text.len()).any(|w| w == text.as_bytes())
}

/// 03 §7.3: 消したセッションの本文は、保持している退避からも除かれる。
#[test]
fn deleted_records_are_purged_from_the_backups() {
    let dir = TempDir::new("restore-purge");
    let c = clock();
    let env = Env::open(dir.path(), c.clone()).unwrap();
    let (sid, ops) = env.writes().create_session("消す").unwrap();
    env.write(ops);
    let u = env.say(sid, "退避に残ってはいけない本文");
    let (keep, ops) = env.writes().create_session("残す").unwrap();
    env.write(ops);
    env.say(keep, "残す本文");
    let backup = env.store.daily_backup(None).unwrap().unwrap();
    assert!(contains(
        &backup.join(DB_FILE),
        "退避に残ってはいけない本文"
    ));

    let ops = {
        let rtx = env.store.read().unwrap();
        delete_session(&rtx, &env.session(sid)).unwrap()
    };
    env.write(ops);
    let n = purge_backups(dir.path(), None, &[sid, u]).unwrap();
    assert!(n >= 1);
    let file = backup.join(DB_FILE);
    assert!(!contains(&file, "退避に残ってはいけない本文"));
    assert!(contains(&file, "残す本文"));
    // 今の DB の残す記録は読める。
    assert!(matches!(
        env.store.read().unwrap().get(KIND_SESSION, keep).unwrap(),
        Some(Read::Ok(_))
    ));
}
