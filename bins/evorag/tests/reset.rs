//! `evorag reset [--old] [--yes]` の確認・終了コード・文言 (03 §10.2、07 §5)。実行ファイルを起動して確かめる。
//! 見本 `tests/fixtures/db/0.4.0/root` を一時のフォルダの `data` に写して使う (`--old` は、写した見本を
//! `import-v04` で取り込んでから)。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use evorag_store::lock::DataRootLock;
use evorag_store::meta;
use evorag_store::root_rename::read_meta;
use evorag_store::testutil::{TempDir, raw_remove_meta, raw_set_meta};
use evorag_store::{DB_FILE, JOURNAL_DIR};

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

/// 見本を `<一時>/data` に写す。
fn fixture(label: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new(label);
    let data = tmp.path().join("data");
    copy_dir(&fixture_root(), &data);
    (tmp, data)
}

fn evorag(dir: &Path, args: &[&str], stdin: &str) -> Output {
    let cfg = dir.join("config.toml");
    if !cfg.exists() {
        std::fs::write(&cfg, "").expect("write the config");
    }
    let exe = std::env::var_os("NEXTEST_BIN_EXE_evorag")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_evorag").into());
    let mut child = Command::new(exe)
        .arg("--config")
        .arg(&cfg)
        .arg("--data-root")
        .arg(dir.join("data"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start evorag");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// 見本を取り込んで、改名した古い根 (`data-v0.4-*`) を返す。
fn import(tmp: &Path) -> PathBuf {
    let out = evorag(tmp, &["import-v04"], "y\n");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let mut renamed: Vec<PathBuf> = std::fs::read_dir(tmp)
        .expect("read")
        .map(|e| e.expect("an entry").path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("data-v0.4-"))
        })
        .collect();
    assert_eq!(renamed.len(), 1, "{renamed:?}");
    renamed.remove(0)
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .expect("read")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
        .collect();
    v.sort();
    v
}

/// 消す範囲を示して尋ね、y のときだけ消す。y 以外 (入力の終わりを含む) は何も変えずに 1。消した後も設定は残る。
/// `--yes` は尋ねないが、消す範囲と元に戻せないことは示す。
///
/// verifies: AC-FR-218-1 (確認の後にだけ消し、設定は残る)
#[test]
fn reset_asks_first_then_deletes_the_data_root_and_keeps_the_config() {
    let (tmp, data) = fixture("cli-reset");
    for answer in ["n\n", ""] {
        let out = evorag(tmp.path(), &["reset"], answer);
        let t = text(&out);
        assert_eq!(out.status.code(), Some(1), "{t}");
        assert!(
            t.contains("丸ごと消します") && t.contains("元に戻せません"),
            "{t}"
        );
        assert!(t.contains("消しませんでした"), "{t}");
        assert!(data.join(DB_FILE).is_file());
    }
    let out = evorag(tmp.path(), &["reset"], "y\n");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    assert!(
        t.contains("消しました") && t.contains("新しいデータ根"),
        "{t}"
    );
    assert_eq!(names(tmp.path()), ["config.toml"]);

    // もう無い: データ根でないとして 1 (何も作らない)。
    let out = evorag(tmp.path(), &["reset", "--yes"], "");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(t.contains("データ根ではない"), "{t}");
    assert_eq!(names(tmp.path()), ["config.toml"]);

    let (tmp, _) = fixture("cli-reset-yes");
    let out = evorag(tmp.path(), &["reset", "--yes"], "");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    assert!(!t.contains("消しますか"), "{t}");
    assert!(
        t.contains("丸ごと消します") && t.contains("元に戻せません"),
        "{t}"
    );
    assert_eq!(names(tmp.path()), ["config.toml"]);
}

/// ほかの evorag がデータ根を使っていれば、PID を示して 2 で終わる (何も消さない)。
#[test]
fn reset_exits_with_2_while_the_root_is_in_use() {
    let (tmp, data) = fixture("cli-reset-locked");
    let held = DataRootLock::acquire(&data).expect("another evorag");
    for args in [&["reset", "--yes"][..], &["reset", "--old", "--yes"][..]] {
        let out = evorag(tmp.path(), args, "");
        let t = text(&out);
        assert_eq!(out.status.code(), Some(2), "{t}");
        assert!(t.contains(&std::process::id().to_string()), "{t}");
    }
    drop(held);
    assert!(data.join(DB_FILE).is_file());
}

/// `--old`: 取り込みの後の古い根を、忘れた値が残ることと除いたターンの数を示して確かめてから消し、今の根の
/// `import.old_root` を `disposed:<時刻>` にする (DB とジャーナル)。もう一度は、処分してあると示して 1。
/// 取り込みで作った根でなければ、何もせずに 1。
///
/// verifies: AC-FR-218-1 (`--old`), AC-FR-220-1 (CLI の層: 古い根が消え、import.old_root が disposed になる)
#[test]
fn reset_old_deletes_the_imported_old_root_and_records_the_disposal() {
    let (tmp, data) = fixture("cli-reset-old");
    let out = evorag(tmp.path(), &["reset", "--old"], "y\n");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(t.contains("import-v04 で作った根ではない"), "{t}");
    assert!(!t.contains("消しますか"), "{t}");

    let old = import(tmp.path());
    let out = evorag(tmp.path(), &["reset", "--old"], "n\n");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(t.contains(&old.display().to_string()), "{t}");
    assert!(
        t.contains("取り込みの後に忘れた 0 件の値") && t.contains("取り込みで除いたターンは"),
        "{t}"
    );
    assert!(old.join(DB_FILE).is_file());

    let out = evorag(tmp.path(), &["reset", "--old"], "y\n");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    assert!(t.contains("処分したこと"), "{t}");
    assert!(!old.exists());
    assert_eq!(names(tmp.path()), ["config.toml", "data"]);
    let disposed = read_meta(&data).expect("meta")[meta::IMPORT_OLD_ROOT].clone();
    assert!(
        disposed.starts_with(meta::IMPORT_OLD_ROOT_DISPOSED),
        "{disposed}"
    );
    // ジャーナルにも `meta` の行として書いた (DB を失って戻しても、処分を求め直さないため)。
    let journaled = std::fs::read_dir(data.join(JOURNAL_DIR))
        .expect("journal")
        .filter_map(Result::ok)
        .any(|e| std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains(disposed.as_str())));
    assert!(journaled, "the disposal is in the journal");

    let out = evorag(tmp.path(), &["reset", "--old", "--yes"], "");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(t.contains("もう処分してあります"), "{t}");
}

/// `--old`: 取り込みの後に古い根を v0.4 で使っていれば、増えたジャーナルの行の数と取り込み直しの案内を示し、
/// 尋ねずに 1 で終わる (何も消さない)。
#[test]
fn reset_old_refuses_an_old_root_used_after_the_import() {
    let (tmp, data) = fixture("cli-reset-old-used");
    let old = import(tmp.path());
    let seq: u64 = read_meta(&old).expect("meta")[meta::JOURNAL_LAST_SEQ]
        .parse()
        .expect("a number");
    raw_set_meta(&old, meta::JOURNAL_LAST_SEQ, &(seq + 2).to_string());
    let out = evorag(tmp.path(), &["reset", "--old"], "y\n");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(
        t.contains("2 件増えています") && t.contains("import-v04 --from"),
        "{t}"
    );
    assert!(!t.contains("消しますか"), "{t}");
    assert!(old.join(DB_FILE).is_file());
    assert_eq!(
        read_meta(&data).expect("meta")[meta::IMPORT_OLD_ROOT],
        meta::IMPORT_OLD_ROOT_KEPT
    );
}

/// 古い根が残っているときに今の根を消そうとすると、古い根のパスを示し、先に `--old` で消すことを勧める。
/// 消しかけの続きを消すときも今の根を消すことを明示し、古い根の消しかけはこの操作では消えず `--old` で消すと
/// 示す。`doctor` は、今の根の消しかけ (今の根も一緒に消えると添える) と古い根の消しかけ (`--old` を案内する) を
/// 分けて示す。`--old` は古い根の消しかけも消す。
#[test]
fn reset_guides_to_the_command_that_deletes_each_leftover() {
    let (tmp, data) = fixture("cli-reset-kept");
    let old = import(tmp.path());
    let current_left = tmp.path().join("data.deleting-20261127T000000000Z");
    std::fs::create_dir_all(current_left.join(JOURNAL_DIR)).expect("a leftover");
    let old_name = old
        .file_name()
        .expect("a name")
        .to_string_lossy()
        .into_owned();
    let old_left = tmp
        .path()
        .join(format!("{old_name}.deleting-20261127T000000000Z"));
    std::fs::create_dir_all(old_left.join(JOURNAL_DIR)).expect("a leftover");

    let out = evorag(tmp.path(), &["reset"], "n\n");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    let shown = |p: &Path| t.contains(&p.display().to_string());
    assert!(
        t.contains("evorag reset --old で消すことを勧めます") && shown(&old),
        "{t}"
    );
    assert!(
        t.contains("消しかけの続きだけでなく、今のデータ根") && shown(&data),
        "{t}"
    );
    assert!(
        t.contains("この操作では消えません (evorag reset --old で消します)") && shown(&old_left),
        "{t}"
    );
    assert!(data.join(DB_FILE).is_file());

    let out = evorag(tmp.path(), &["doctor"], "");
    let t = text(&out);
    let line = |needle: &str| {
        t.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_default()
            .to_owned()
    };
    let current_line = line(&current_left.display().to_string());
    assert!(
        current_line.contains("evorag reset で続きを消せます")
            && current_line.contains("も一緒に消えます"),
        "{t}"
    );
    let old_line = line(&old_left.display().to_string());
    assert!(
        old_line.contains("evorag reset --old で続きを消せます"),
        "{t}"
    );

    let out = evorag(tmp.path(), &["reset", "--old", "--yes"], "");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    assert!(!old.exists() && !old_left.exists(), "{t}");
    assert!(current_left.exists() && data.join(DB_FILE).is_file(), "{t}");
}

/// 取り込みの途中 (`<名前>.v05-import` がある) の `reset` と、照合を終えていない取り込みの根の `--old` は、何も
/// 消さずに 1 で断り、`import-v04` を案内する。
#[test]
fn reset_refuses_while_an_import_is_unfinished() {
    let (tmp, data) = fixture("cli-reset-import-pending");
    let old = import(tmp.path());
    let pending = tmp.path().join("data.v05-import");
    std::fs::create_dir_all(&pending).expect("a leftover import");
    let out = evorag(tmp.path(), &["reset", "--yes"], "");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(
        t.contains("途中です") && t.contains("evorag import-v04"),
        "{t}"
    );
    assert!(data.join(DB_FILE).is_file());
    std::fs::remove_dir(&pending).expect("remove");

    raw_remove_meta(&data, meta::IMPORT_REPORT);
    let out = evorag(tmp.path(), &["reset", "--old", "--yes"], "");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(t.contains("照合を終えていない"), "{t}");
    assert!(old.join(DB_FILE).is_file());
}
