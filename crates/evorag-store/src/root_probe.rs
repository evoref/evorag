//! データ根を書き込みで開く前の、形式の見分け (03 §8 の「v0.4 のデータ根を v0.5 で開いた」、§12.4)。何も書かない。
//!
//! v0.5 は v0.4 の根を在り場所で移行しない。redb を書き込みで開くと DB のファイルを書き換えうる (正しく閉じていない
//! DB は開くときに修復される) ので、形式は次の順で、データ根に書かずに見分ける。
//!
//! 1. DB のファイルを読むだけで開き (`ReadOnlyDatabase`)、`meta` を読む。
//! 2. 読むだけでは開けない (修復が要る) ときは、DB のファイルを**データ根の外の一時のフォルダ**に写し、その写しを
//!    書き込みで開いて修復してから `meta` を読み、写しを消す (元のファイルには触れない)。
//! 3. DB が無い・空・写しも読めないときは、ジャーナルの `format.version` の `meta` の行 (v0.5 の根が作るときに書く、
//!    03 §6) で見分ける。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use redb::ReadOnlyDatabase;

use crate::fsutil;
pub use crate::journal::JournalMark;
use crate::meta;
use crate::store::{DB_FILE, FORMAT_V05, read_meta_db_any};
use crate::version::Version;

/// 書き込みで開く前に見た、データ根の形式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootProbe {
    /// DB の `meta` を読めた。`clean` は読むだけで開けた (偽なら、修復の要る DB を、データ根の外の写しを直して読んだ)。
    Readable {
        meta: BTreeMap<String, String>,
        clean: bool,
    },
    /// DB のファイルが無い・空・写しを直しても読めない。`journal` はジャーナルから見た形式。
    NoDatabase { journal: JournalMark },
}

/// v0.5 では在り場所で開かない、v0.4 以前の根 ([`RootProbe::v04`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V04Root {
    /// DB の形式の版 (分からなければ `None`)。
    pub format: Option<String>,
    /// 前の v0.4 が正しく閉じた (読むだけで開けた) 根。偽なら、v0.4 で一度開いて閉じてから取り込む (03 §12.4 の
    /// 取り込める根の (6))。
    pub clean: bool,
}

impl RootProbe {
    /// DB の `meta` の形式の版。
    #[must_use]
    pub fn format(&self) -> Option<&str> {
        match self {
            RootProbe::Readable { meta: m, .. } => m.get(meta::FORMAT_VERSION).map(String::as_str),
            RootProbe::NoDatabase { .. } => None,
        }
    }

    /// v0.4 以前の根 (か、そうとみなす根) なら、その様子。DB の形式の版が v0.5 より古いか、DB を読めずに、ジャーナルに
    /// 行があるのに v0.5 の根の印が無い (か v0.4 の指示の行がある) とき。
    #[must_use]
    pub fn v04(&self) -> Option<V04Root> {
        match self {
            RootProbe::Readable { clean, .. } => {
                let format = self.format()?;
                older_than_v05(format).then(|| V04Root {
                    format: Some(format.to_owned()),
                    clean: *clean,
                })
            }
            RootProbe::NoDatabase {
                journal: JournalMark::Older,
            } => Some(V04Root {
                format: None,
                clean: false,
            }),
            RootProbe::NoDatabase { .. } => None,
        }
    }
}

/// 形式の版 `format` が v0.5 より古いか (読めない版は古いとみなさない。開く側が断る)。
#[must_use]
pub fn older_than_v05(format: &str) -> bool {
    let v05: Version = FORMAT_V05.parse().expect("FORMAT_V05 is a version");
    format.parse::<Version>().is_ok_and(|v| v < v05)
}

/// データ根 `root` の形式を、何も書かずに見分ける (このモジュールの冒頭の順)。ロックは取らない (呼ぶ側が要れば
/// 先に取る)。
///
/// # Errors
/// ジャーナルのフォルダを読めないとき。
pub fn probe_root(root: &Path) -> std::io::Result<RootProbe> {
    let file = root.join(DB_FILE);
    let has_db = std::fs::metadata(&file).is_ok_and(|m| m.is_file() && m.len() > 0);
    if has_db {
        if let Ok(ro) = ReadOnlyDatabase::open(&file) {
            let m = read_meta_db_any(&ro);
            // 閉じるときも書かない (読むだけの取っ手)。
            let _ = ro.close();
            if let Ok(m) = m
                && !m.is_empty()
            {
                return Ok(RootProbe::Readable {
                    meta: m,
                    clean: true,
                });
            }
        }
        if let Some(m) = meta_of_repaired_copy(&file) {
            return Ok(RootProbe::Readable {
                meta: m,
                clean: false,
            });
        }
    }
    Ok(RootProbe::NoDatabase {
        journal: crate::journal::format_mark(root)?,
    })
}

/// 消し忘れない一時のフォルダ。
struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// DB のファイル `file` を、データ根の外の一時のフォルダ (所有者だけの権限) に写し、写しを書き込みで開いて修復して
/// から `meta` を読む。写しは読んだら消す。写せない (空きが無いなど)・写しも開けない・`meta` が空なら `None`。
fn meta_of_repaired_copy(file: &Path) -> Option<BTreeMap<String, String>> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!(
        "evorag-probe-{}-{nanos}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    // 写しは会話を含むので、ほかの利用者が読めないフォルダに置く。
    fsutil::create_private_dir_new(&dir).ok()?;
    let scratch = ScratchDir(dir);
    let copy = scratch.0.join(DB_FILE);
    fsutil::prepare_private_file(&copy).ok()?;
    std::fs::copy(file, &copy).ok()?;
    let db = redb::Database::open(&copy).ok()?;
    let m = read_meta_db_any(&db).ok();
    drop(db);
    drop(scratch);
    m.filter(|m| !m.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_older_than_v05_are_recognised() {
        assert!(older_than_v05("0.3"));
        assert!(older_than_v05("0.1"));
        assert!(!older_than_v05("0.5"));
        assert!(!older_than_v05("1"));
        assert!(!older_than_v05("not a version"));
    }
}
