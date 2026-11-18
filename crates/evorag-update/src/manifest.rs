//! `release.json` (schema 1) の形と、読む側の規則 (12 §5.3)。
//!
//! ここの関数は、署名を確かめた後 ([`crate::SignedManifest`]) か、`cargo xtask` が自分で作った
//! `release.json` (`unverified-dist` の機能) からだけ呼ぶ。

use serde::Deserialize;

use crate::{
    DISPLAY_MAX_BYTES, ReleaseDate, Sha256Digest, UpdateError, Version, VersionSource,
    sanitize_for_display,
};

/// この版が読める `release.json` の形式の版。
pub const SCHEMA: u64 = 1;

/// `release.json` の `name`。
pub const NAME: &str = "evorag";

/// 配布物の形式。配布物の名前の末尾で決める (ビルドの `cfg` では決めない、12 §5.3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArchiveFormat {
    /// `.zip` (deflate か無圧縮)。Windows の対象。
    Zip,
    /// `.tar.gz`。Linux・macOS の対象。
    TarGz,
}

impl ArchiveFormat {
    /// 名前の末尾。
    #[must_use]
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Zip => ".zip",
            Self::TarGz => ".tar.gz",
        }
    }

    /// 配布物の名前の末尾から形式を決める。
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Zip, Self::TarGz]
            .into_iter()
            .find(|f| name.ends_with(f.suffix()))
    }

    /// 対象 (target triple) でこの版が読む形式 (v0.3: Windows は `.zip`、Linux・macOS は `.tar.gz`)。
    #[must_use]
    pub fn for_target(target: &str) -> Option<Self> {
        if target.contains("-windows-") {
            Some(Self::Zip)
        } else if target.contains("-linux-") || target.ends_with("-apple-darwin") {
            Some(Self::TarGz)
        } else {
            None
        }
    }
}

/// `artifacts` の要素 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    target: String,
    name: String,
    sha256: Sha256Digest,
    size: u64,
    exe: String,
    exe_sha256: Sha256Digest,
}

impl Artifact {
    /// 対象 (Rust の target triple)。
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// 配布物のファイル名 (Releases の asset の名前)。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 配布物の SHA-256。
    #[must_use]
    pub const fn sha256(&self) -> Sha256Digest {
        self.sha256
    }

    /// 配布物の大きさ (バイト)。
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// 配布物の中の実行ファイルのパス。項目と比べるためだけに使い、書き出す先には使わない。制御文字と
    /// 双方向の書式の文字を含むものは読むときに断るので、そのまま示してよい。
    #[must_use]
    pub fn exe(&self) -> &str {
        &self.exe
    }

    /// 実行ファイルの SHA-256。
    #[must_use]
    pub const fn exe_sha256(&self) -> Sha256Digest {
        self.exe_sha256
    }

    /// 名前の末尾から決めた形式。
    #[must_use]
    pub fn format(&self) -> Option<ArchiveFormat> {
        ArchiveFormat::from_name(&self.name)
    }
}

/// 読んで形を確かめた `release.json` (schema 1)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    version: Version,
    date: ReleaseDate,
    upgrade_from: Version,
    notes: Option<String>,
    artifacts: Vec<Artifact>,
    db_format: Option<String>,
    db_min_write: Option<String>,
    llama_server_min_build: Option<u64>,
}

impl Manifest {
    /// 版。
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// リリースの日付。
    #[must_use]
    pub const fn date(&self) -> ReleaseDate {
        self.date
    }

    /// この版へ更新できる、今の版の下限。
    #[must_use]
    pub const fn upgrade_from(&self) -> Version {
        self.upgrade_from
    }

    /// 変更の要点。示せるように整えたもの ([`sanitize_for_display`]、4 KiB まで)。
    #[must_use]
    pub fn notes(&self) -> Option<&str> {
        self.notes.as_deref()
    }

    /// すべての配布物。
    #[must_use]
    pub fn artifacts(&self) -> &[Artifact] {
        &self.artifacts
    }

    /// この版が書く DB の形式の版。示せるように整えたもの (制御文字を除き、64 バイトまで)。
    #[must_use]
    pub fn db_format(&self) -> Option<&str> {
        self.db_format.as_deref()
    }

    /// この版が移行した後の DB の `format.min_write`。示せるように整えたもの (制御文字を除き、64 バイトまで)。
    #[must_use]
    pub fn db_min_write(&self) -> Option<&str> {
        self.db_min_write.as_deref()
    }

    /// 対応する llama-server の build 番号の下限。
    #[must_use]
    pub const fn llama_server_min_build(&self) -> Option<u64> {
        self.llama_server_min_build
    }

    /// 対象の配布物を選ぶ: `target` が同じで、名前の末尾がその対象でこの版が読む形式のもの。
    ///
    /// # Errors
    ///
    /// 当てはまるものが 0 か 2 つ以上のとき (使えない理由)。
    pub fn artifact_for(&self, target: &str) -> Result<&Artifact, UpdateError> {
        let no_artifact = || UpdateError::NoArtifact {
            target: target.to_owned(),
        };
        let format = ArchiveFormat::for_target(target).ok_or_else(no_artifact)?;
        let found: Vec<&Artifact> = self
            .artifacts
            .iter()
            .filter(|a| a.target == target && a.name.ends_with(format.suffix()))
            .collect();
        match found.as_slice() {
            [one] => Ok(one),
            [] => Err(no_artifact()),
            many => Err(UpdateError::AmbiguousArtifact {
                target: target.to_owned(),
                count: many.len(),
            }),
        }
    }
}

/// `schema`・`name`・`version` だけを見る (形式の版が違えば、ほかの項目の決まりも違うため、先に見る。
/// `name` と `version` は、ほかの項目の形より先に比べる)。
#[derive(Deserialize)]
struct Probe {
    schema: Option<serde_json::Value>,
    name: Option<serde_json::Value>,
    version: Option<serde_json::Value>,
}

/// schema 1 の項目。知らない項目は無視する。
#[derive(Deserialize)]
struct RawManifest {
    version: String,
    date: String,
    upgrade_from: String,
    notes: Option<String>,
    artifacts: Vec<RawArtifact>,
    db_format: Option<String>,
    db_min_write: Option<String>,
    llama_server_min_build: Option<u64>,
}

#[derive(Deserialize)]
struct RawArtifact {
    target: String,
    name: String,
    sha256: String,
    size: u64,
    exe: String,
    exe_sha256: String,
}

/// 示すために整える短い項目 (`db_format`・`db_min_write`・断った理由の値) の上限 (バイト)。
const SHORT_FIELD_MAX_BYTES: usize = 64;

/// 示すと端末を乱す文字 (制御文字と双方向の書式の文字) を含むか。
fn has_unsafe_chars(s: &str) -> bool {
    sanitize_for_display(s, s.len()) != s
}

fn malformed(reason: impl std::fmt::Display) -> UpdateError {
    UpdateError::Malformed {
        reason: sanitize_for_display(&reason.to_string(), DISPLAY_MAX_BYTES),
    }
}

/// 配布物の名前: `A-Z`・`a-z`・`0-9`・`.`・`_`・`+`・`-` だけで、`..` を含まない。
fn name_is_allowed(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

/// 配布物の中のパス: `/` で区切り、`..`・絶対パス・`\` を含まない。
fn exe_is_allowed(exe: &str) -> bool {
    let b = exe.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    !exe.is_empty()
        && !exe.starts_with('/')
        && !exe.ends_with('/')
        && !drive
        && !exe.contains("..")
        && !exe.contains('\\')
        && !has_unsafe_chars(exe)
}

fn artifact(raw: RawArtifact, i: usize) -> Result<Artifact, UpdateError> {
    if raw.target.is_empty() || has_unsafe_chars(&raw.target) {
        return Err(malformed(format!(
            "artifacts[{i}].target is empty or has control characters"
        )));
    }
    if !name_is_allowed(&raw.name) {
        return Err(malformed(format!(
            "artifacts[{i}].name has characters other than A-Z a-z 0-9 . _ + - or `..`"
        )));
    }
    if !exe_is_allowed(&raw.exe) {
        return Err(malformed(format!(
            "artifacts[{i}].exe is absolute or has `..`, `\\` or control characters"
        )));
    }
    let digest = |field: &str, v: &str| {
        v.parse::<Sha256Digest>()
            .map_err(|e| malformed(format!("artifacts[{i}].{field}: {e}")))
    };
    Ok(Artifact {
        sha256: digest("sha256", &raw.sha256)?,
        exe_sha256: digest("exe_sha256", &raw.exe_sha256)?,
        target: raw.target,
        name: raw.name,
        size: raw.size,
        exe: raw.exe,
    })
}

/// `release.json` のバイト列を読む。順は、`schema` (1 でなければ断る)、`name` (`evorag` でなければ
/// 改ざんを疑う)、`version` (`expected_version` があれば、その版の文字列とちょうど同じでなければ改ざんを
/// 疑う。版として読めないものも同じ)、項目の型と形の条件 (使えない理由)。
///
/// 署名を確かめた後のバイト列だけを渡す (呼ぶのは [`crate::SignedManifest`] と、xtask 向けの
/// `unverified-dist` の機能)。
pub(crate) fn parse(
    json: &[u8],
    expected_version: Option<Version>,
) -> Result<Manifest, UpdateError> {
    let probe: Probe = serde_json::from_slice(json).map_err(malformed)?;
    if probe.schema.as_ref().and_then(serde_json::Value::as_u64) != Some(SCHEMA) {
        return Err(UpdateError::UnknownSchema);
    }
    if probe.name.as_ref().and_then(serde_json::Value::as_str) != Some(NAME) {
        return Err(UpdateError::NameMismatch);
    }
    if let Some(expected) = expected_version {
        let found = probe.version.as_ref().and_then(serde_json::Value::as_str);
        if found != Some(expected.to_string().as_str()) {
            let found = found.map_or_else(
                || {
                    probe
                        .version
                        .as_ref()
                        .map_or_else(|| "(none)".to_owned(), ToString::to_string)
                },
                str::to_owned,
            );
            return Err(UpdateError::VersionMismatch {
                source_of: VersionSource::TrustedComment,
                found: sanitize_for_display(&found, SHORT_FIELD_MAX_BYTES),
                expected: expected.to_string(),
            });
        }
    }
    let raw: RawManifest = serde_json::from_slice(json).map_err(malformed)?;
    let version = raw
        .version
        .parse()
        .map_err(|e| malformed(format!("version: {e}")))?;
    let date = raw
        .date
        .parse()
        .map_err(|e| malformed(format!("date: {e}")))?;
    let upgrade_from = raw
        .upgrade_from
        .parse()
        .map_err(|e| malformed(format!("upgrade_from: {e}")))?;
    let artifacts = raw
        .artifacts
        .into_iter()
        .enumerate()
        .map(|(i, a)| artifact(a, i))
        .collect::<Result<Vec<_>, _>>()?;
    let short = |v: Option<String>| v.map(|v| sanitize_for_display(&v, SHORT_FIELD_MAX_BYTES));
    Ok(Manifest {
        version,
        date,
        upgrade_from,
        notes: raw
            .notes
            .map(|n| sanitize_for_display(&n, DISPLAY_MAX_BYTES)),
        artifacts,
        db_format: short(raw.db_format),
        db_min_write: short(raw.db_min_write),
        llama_server_min_build: raw.llama_server_min_build,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_names_and_exe_paths_follow_the_rules() {
        assert!(name_is_allowed("evorag-0.3.0-x86_64-pc-windows-msvc.zip"));
        assert!(name_is_allowed("evorag+x_1.tar.gz"));
        for bad in [
            "",
            "a..zip",
            "a/b.zip",
            "a\\b.zip",
            "a b.zip",
            "evorag.zip\n",
            "é.zip",
            "a%2f.zip",
        ] {
            assert!(!name_is_allowed(bad), "{bad:?}");
        }
        assert!(exe_is_allowed(
            "evorag-0.3.0-x86_64-pc-windows-msvc/evorag.exe"
        ));
        assert!(exe_is_allowed("evorag"));
        for bad in [
            "",
            "/evorag",
            "a/../evorag",
            "..",
            "a\\evorag.exe",
            "C:/evorag.exe",
            "a/",
            "a\0b",
        ] {
            assert!(!exe_is_allowed(bad), "{bad:?}");
        }
    }

    #[test]
    fn formats_come_from_the_name_and_the_target() {
        assert_eq!(ArchiveFormat::from_name("x.zip"), Some(ArchiveFormat::Zip));
        assert_eq!(
            ArchiveFormat::from_name("x.tar.gz"),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(ArchiveFormat::from_name("x.msi"), None);
        assert_eq!(
            ArchiveFormat::for_target("x86_64-pc-windows-msvc"),
            Some(ArchiveFormat::Zip)
        );
        assert_eq!(
            ArchiveFormat::for_target("x86_64-unknown-linux-musl"),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(
            ArchiveFormat::for_target("aarch64-apple-darwin"),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(ArchiveFormat::for_target("x86_64-unknown-freebsd"), None);
    }
}
