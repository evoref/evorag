//! 本体に埋め込むリリースの署名の公開鍵 (12 §5.2)。
//!
//! - リポジトリ直下の `minisign.pub` (今の鍵) は必ず埋め込む。無ければビルドを失敗させる。
//! - `minisign.next.pub` (次の鍵) は、置かれていれば埋め込む。鍵はエージェントが作らず、人間が置く。
//! - 埋め込むのは、各ファイルの 2 行目 (base64 の行) だけ。形は実行時と試験で確かめる。
//! - 次の鍵を後から置いたときは、手元のビルドの残りを `cargo clean -p evorag-update` で消す
//!   (無いファイルを `rerun-if-changed` に渡すと毎回作り直すので、あるファイルだけを見張る)。
//!   リリースのビルドは CI の新しい作業ツリーで作るので、この扱いの影響を受けない。

use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let root = Path::new(&manifest_dir).join("..").join("..");
    let mut keys = Vec::new();
    for (file, required) in [("minisign.pub", true), ("minisign.next.pub", false)] {
        let path = root.join(file);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("cannot read {}: {e} (12 §5.2)", path.display()),
        };
        println!("cargo:rerun-if-changed={}", path.display());
        let line = text.lines().nth(1).map(str::trim).unwrap_or_default();
        // minisign の公開鍵の base64 の行は、`RW` で始まる 56 文字 (07 §4.2)。
        assert!(
            line.len() == 56
                && line.starts_with("RW")
                && line
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='),
            "{} does not hold a minisign public key on its second line",
            path.display()
        );
        keys.push(format!("    ({file:?}, {line:?}),\n"));
    }
    let out =
        Path::new(&std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("embedded_keys.rs");
    let code = format!(
        "/// 埋め込んだ鍵 (ファイルの名前と base64 の行)。build.rs が作る。\n\
         pub(crate) const EMBEDDED_KEYS: &[(&str, &str)] = &[\n{}];\n",
        keys.concat()
    );
    std::fs::write(&out, code).unwrap_or_else(|e| panic!("cannot write {}: {e}", out.display()));
}
