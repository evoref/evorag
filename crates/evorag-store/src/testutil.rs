//! 試験の道具。外部の crate (tempfile) を足さずに、一時フォルダを作って片付ける。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 落とすと消える一時フォルダ。
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "evorag-test-{label}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).expect("create temp dir");
        Self(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
