//! ログ (02 §7)。状態のフォルダの `logs/` に、日ごとのファイルで書く。文言は英語。
//!
//! - **会話の本文・事実の値・API キーを出さない**。出すのは ID・長さ・件数・時間・分類だけ (NFR-502)。
//!   llama-server のエラーの本文 (要求の一部を含みうる) も出さず、分類と状態コードだけにする。
//! - ファイルは所有者だけの権限。7 日より古いファイルは、起動のときに消す。

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 残す日数。
const KEEP_DAYS: i64 = 7;
const PREFIX: &str = "evorag-";
const SUFFIX: &str = ".log";

fn day_number(ms: i64) -> i64 {
    ms.div_euclid(86_400_000)
}

/// `evorag-YYYY-MM-DD.log` (UTC の日付)。
pub fn file_name(now_ms: i64) -> String {
    let (y, m, d, _, _) = crate::localtime::civil(now_ms);
    format!("{PREFIX}{y:04}-{m:02}-{d:02}{SUFFIX}")
}

/// ファイル名の日付 (1970-01-01 からの日数)。
fn day_of(name: &str) -> Option<i64> {
    let date = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    let mut it = date.split('-').map(|s| s.parse::<i64>().ok());
    let (y, m, d) = (it.next()??, it.next()??, it.next()??);
    ((1..=12).contains(&m) && (1..=31).contains(&d))
        .then(|| crate::localtime::days_from_civil(y, m, d))
}

/// 古いログを消す。
pub fn prune(dir: &Path, now_ms: i64) {
    let today = day_number(now_ms);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(day) = day_of(&name)
            && today - day >= KEEP_DAYS
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

struct LogFile(Mutex<std::fs::File>);

impl Write for &LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

/// ログを始める (プロセスで 1 回)。書いたファイルを返す。既に始めていれば何もしない。
pub fn init(dir: &Path, now_ms: i64) -> io::Result<PathBuf> {
    evorag_store::fsutil::create_private_dir(dir)?;
    prune(dir, now_ms);
    let path = dir.join(file_name(now_ms));
    evorag_store::fsutil::prepare_private_file(&path)?;
    let file = std::fs::OpenOptions::new().append(true).open(&path)?;
    let writer: &'static LogFile = Box::leak(Box::new(LogFile(Mutex::new(file))));
    let _ = tracing_subscriber::fmt()
        .with_writer(move || writer)
        .with_ansi(false)
        .with_target(false)
        .with_max_level(tracing::Level::INFO)
        .try_init();
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_old_files_are_pruned() {
        let now = 1_762_650_000_000; // 2025-11-09
        assert_eq!(file_name(now), "evorag-2025-11-09.log");
        assert_eq!(day_of(&file_name(now)), Some(day_number(now)));
        assert_eq!(day_of("evorag-x.log"), None);

        let dir = std::env::temp_dir().join(format!("evorag-logs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let day = 86_400_000;
        for back in [0, 6, 7, 30] {
            std::fs::write(dir.join(file_name(now - back * day)), b"x").unwrap();
        }
        std::fs::write(dir.join("other.txt"), b"x").unwrap();
        prune(&dir, now);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "evorag-2025-11-03.log",
                "evorag-2025-11-09.log",
                "other.txt"
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
