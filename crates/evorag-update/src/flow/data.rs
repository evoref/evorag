//! `rollback` のデータの案内 (12 §7.2 の rollback の 3): 戻す版が、データ根の DB を書けるか・読めるか。

use std::path::{Path, PathBuf};

use evorag_store::inspect::{self, DbState};
use evorag_store::lock::DataRootLock;
use evorag_store::meta;
use evorag_store::restore::{self, BackupKind};
use evorag_store::version::Version as FormatVersion;

/// 探す移行の前の退避の世代の数 (新しいものから、03 §7.2)。
const PRE_OP_SHOWN: usize = 3;

/// 戻す版と、データ根の DB の形式の比べ。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DataVerdict {
    /// データ根か DB が無い (案内することは無い)。
    NoData,
    /// 戻した版でそのまま使える (`format.min_write` 以上)。
    Usable,
    /// 戻した版は読むだけで起動する (`min_read` 以上、`min_write` 未満)。書けるようにするには、移行の前の
    /// 退避から戻す。
    ReadOnly {
        /// 戻す版の DB の形式の版。
        format: String,
        /// DB の `format.min_write`。
        min_write: String,
    },
    /// 戻した版では開けない (`min_read` 未満)。移行の前の退避から戻す必要がある。
    Unopenable {
        /// 戻す版の DB の形式の版。
        format: String,
        /// DB の `format.min_read`。
        min_read: String,
    },
    /// 戻す版の形式の版、または DB の形式の版が分からない。一般の案内を示す。
    Unknown,
}

/// `rollback` のデータの案内。入れ替えの前に求め、入れ替えの後に示す。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DataGuidance {
    /// 比べた結果。
    pub verdict: DataVerdict,
    /// 移行の前の退避 (新しいものから 3 世代)。戻す必要があるか、分からないときだけ探す。
    pub pre_op_backups: Vec<PathBuf>,
    /// 設定の移行で残した写し (`config.toml.bak-v<旧版>`、03 §12.3)。
    pub config_backups: Vec<PathBuf>,
}

impl DataGuidance {
    /// 戻した後に、移行の前の退避から戻す必要がある (か、ありうる) か。
    #[must_use]
    pub const fn needs_restore(&self) -> bool {
        matches!(
            self.verdict,
            DataVerdict::ReadOnly { .. } | DataVerdict::Unopenable { .. } | DataVerdict::Unknown
        )
    }
}

/// 戻す版の DB の形式の版 `prev_format` と、データ根の DB の `format.min_read`・`format.min_write` を
/// 比べる。DB は `lock` を持ったまま、読むだけで開く。`lock` が無いのはデータ根が無いとき。
pub(crate) fn guidance(
    lock: Option<&DataRootLock>,
    data_root: &Path,
    backup_dir: Option<&Path>,
    config_file: Option<&Path>,
    prev_format: Option<&str>,
) -> DataGuidance {
    let verdict = lock.map_or(DataVerdict::NoData, |lock| {
        let (state, meta) = inspect::read_meta_locked(lock);
        match state {
            DbState::Missing => DataVerdict::NoData,
            DbState::Opened => compare(
                prev_format,
                meta.get(meta::FORMAT_MIN_READ).map(String::as_str),
                meta.get(meta::FORMAT_MIN_WRITE).map(String::as_str),
            ),
            DbState::NeedsRepair(_) | DbState::Unreadable(_) => DataVerdict::Unknown,
        }
    });
    let mut out = DataGuidance {
        verdict,
        pre_op_backups: Vec::new(),
        config_backups: config_file.map(config_backups).unwrap_or_default(),
    };
    if out.needs_restore() {
        out.pre_op_backups = restore::list_backups(data_root, backup_dir)
            .into_iter()
            .filter(|b| b.kind == BackupKind::PreOp)
            .take(PRE_OP_SHOWN)
            .map(|b| b.dir)
            .collect();
    }
    out
}

/// 形式の版を比べる (03 §4 の規則 6)。どれかが無い・読めなければ [`DataVerdict::Unknown`]。
pub(crate) fn compare(
    prev_format: Option<&str>,
    min_read: Option<&str>,
    min_write: Option<&str>,
) -> DataVerdict {
    let parse = |s: Option<&str>| s.and_then(|s| s.parse::<FormatVersion>().ok());
    let (Some(prev), Some(read), Some(write)) =
        (parse(prev_format), parse(min_read), parse(min_write))
    else {
        return DataVerdict::Unknown;
    };
    let format = prev_format.unwrap_or_default().to_owned();
    if prev >= write {
        DataVerdict::Usable
    } else if prev >= read {
        DataVerdict::ReadOnly {
            format,
            min_write: min_write.unwrap_or_default().to_owned(),
        }
    } else {
        DataVerdict::Unopenable {
            format,
            min_read: min_read.unwrap_or_default().to_owned(),
        }
    }
}

/// `config.toml` と同じフォルダの `config.toml.bak-v<版>` (名前の順)。
fn config_backups(config_file: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (config_file.parent(), config_file.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{}.bak-v", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_prefix(&prefix))
                .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_compared_with_min_read_and_min_write() {
        assert_eq!(
            compare(Some("0.3"), Some("0.1"), Some("0.1")),
            DataVerdict::Usable
        );
        assert_eq!(
            compare(Some("0.2"), Some("0.1"), Some("0.3")),
            DataVerdict::ReadOnly {
                format: "0.2".into(),
                min_write: "0.3".into()
            }
        );
        assert_eq!(
            compare(Some("0.1"), Some("0.2"), Some("0.3")),
            DataVerdict::Unopenable {
                format: "0.1".into(),
                min_read: "0.2".into()
            }
        );
        assert_eq!(
            compare(Some("0.3"), Some("0.3"), Some("0.3")),
            DataVerdict::Usable
        );
        for (p, r, w) in [
            (None, Some("0.1"), Some("0.1")),
            (Some("0.2"), None, Some("0.1")),
            (Some("0.2"), Some("0.1"), None),
            (Some("x"), Some("0.1"), Some("0.1")),
        ] {
            assert_eq!(compare(p, r, w), DataVerdict::Unknown, "{p:?} {r:?} {w:?}");
        }
    }

    #[test]
    fn config_backups_are_listed_by_name() {
        let dir = evorag_store::testutil::TempDir::new("update-config-bak");
        let config = dir.path().join("config.toml");
        for name in [
            "config.toml",
            "config.toml.bak-v0",
            "config.toml.bak-v1",
            "config.toml.bak-vx",
            "other.toml.bak-v0",
        ] {
            std::fs::write(dir.path().join(name), "x").expect("write");
        }
        assert_eq!(
            config_backups(&config),
            [
                dir.path().join("config.toml.bak-v0"),
                dir.path().join("config.toml.bak-v1")
            ]
        );
    }
}
