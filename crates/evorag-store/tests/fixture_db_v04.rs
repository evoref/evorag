//! v0.4 のデータ根の見本 (`tests/fixtures/db/0.4.0/`、09 §3.6 の「v0.4 のデータ根からの取り込み」、M5.1) が
//! 壊れていないこと。
//!
//! 見本は v0.4.0 のコードで作った根で、`import-v04` の試験 (03 §12.4、AC-FR-219-1) が読む。ここでは開かずに、
//! `MANIFEST.sha256` に書いたファイルがそろっていて、SHA-256 が合い、ほかのファイルが無いことだけを確かめる
//! (見本を作り直したら、`README.md` の手順で `MANIFEST.sha256` も書き直す)。
//!
//! verifies: 09 §3.6 (v0.4 のデータ根の見本)

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn fixture() -> PathBuf {
    // 実行時の値を先に見る (no-network の CI は、試験を別の場所に写した作業ツリーで走らせる)。
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    dir.join("../../tests/fixtures/db/0.4.0")
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        write!(s, "{b:02x}").expect("writing to a String cannot fail");
        s
    })
}

/// `dir` の下のすべてのファイル (`/` で区切った相対パス)。
fn files_under(dir: &Path, base: &Path, out: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            files_under(&path, base, out);
        } else {
            let rel = path.strip_prefix(base).expect("under the base");
            let parts: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            out.insert(parts.join("/"));
        }
    }
}

#[test]
fn the_v0_4_root_fixture_matches_its_manifest() {
    let base = fixture();
    let manifest = std::fs::read_to_string(base.join("MANIFEST.sha256"))
        .expect("tests/fixtures/db/0.4.0/MANIFEST.sha256 is readable");
    let mut listed = BTreeSet::new();
    for line in manifest.lines().filter(|l| !l.trim().is_empty()) {
        // `sha256sum` の形: `<64 桁の 16 進>  <パス>`。
        let (want, path) = line
            .split_once("  ")
            .unwrap_or_else(|| panic!("malformed manifest line: {line:?}"));
        assert_eq!(want.len(), 64, "malformed digest in {line:?}");
        let bytes = std::fs::read(base.join(path))
            .unwrap_or_else(|e| panic!("{path} is missing from the fixture: {e}"));
        assert_eq!(
            hex(&Sha256::digest(&bytes)),
            want,
            "{path} changed: the v0.4 root fixture must stay as v0.4.0 wrote it"
        );
        assert!(listed.insert(path.to_owned()), "{path} is listed twice");
    }
    let mut present = BTreeSet::new();
    files_under(&base.join("root"), &base, &mut present);
    assert_eq!(
        present, listed,
        "the files under root/ differ from MANIFEST.sha256"
    );
    for must in ["root/evorag.redb", "root/journal/2026-11.jsonl"] {
        assert!(listed.contains(must), "{must} is not in the fixture");
    }
}
