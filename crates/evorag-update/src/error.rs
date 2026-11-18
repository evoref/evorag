//! 検証で断った理由と、その分け方 (07 §5 の終了コード 5 と 1)。

use std::fmt;

use crate::{KeyId, Manifest, ReleaseDate, Signer, Version};

/// 断った理由の分け方 (07 §5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    /// 改ざんを疑う理由 (終了コード 5)。
    Tampered,
    /// 改ざんを疑わないが、使えない (終了コード 1)。
    Unusable,
}

impl Severity {
    /// 07 §5 の終了コード。
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Tampered => 5,
            Self::Unusable => 1,
        }
    }
}

/// `release.json` の `version` と比べた相手 (12 §7.2 の apply の 4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VersionSource {
    /// 署名の trusted comment の版。
    TrustedComment,
    /// 取得した URL のタグ。
    Tag,
    /// 利用者が指定した版。
    Requested,
}

impl fmt::Display for VersionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TrustedComment => "the trusted comment",
            Self::Tag => "the tag",
            Self::Requested => "the requested version",
        })
    }
}

/// 更新の検証で断った理由。
///
/// 文言はログ向けの英語で、署名の前の内容を含めない。利用者に示す文言は、呼ぶ側が文言キーで作る。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UpdateError {
    /// `.minisig` が minisign の署名の形でない。
    #[error("the signature is not a minisign signature")]
    SignatureMalformed,
    /// prehash でない古い形 (legacy) の署名。
    #[error("legacy (not prehashed) signatures are refused")]
    LegacySignature,
    /// 署名の鍵の ID を持つ鍵が無い。
    #[error("no trusted key has the key id {key_id} of the signature")]
    UnknownKey {
        /// 署名の鍵の ID。
        key_id: KeyId,
    },
    /// 同じ ID の鍵のどれでも署名が通らない。
    #[error("the signature does not verify with the trusted keys of id {key_id}")]
    BadSignature {
        /// 署名の鍵の ID。
        key_id: KeyId,
    },
    /// trusted comment が `evorag <版> release.json sha256 <SHA-256>` (このバイト列の SHA-256) と違う。
    #[error(
        "the trusted comment is not `evorag <version> release.json sha256 <sha256 of these bytes>`"
    )]
    TrustedCommentMismatch,
    /// `release.json` の `schema` が無い、または 1 でない (手作業の更新を案内する)。
    #[error("release.json has no schema or a schema other than 1")]
    UnknownSchema,
    /// 署名は通るが、`release.json` の項目が欠ける・型が違う・形の条件に合わない。
    #[error("release.json is malformed: {reason}")]
    Malformed {
        /// 理由 (表示用に整えた英語)。
        reason: String,
    },
    /// `name` が `evorag` でない。
    #[error("release.json is not for evorag")]
    NameMismatch,
    /// `version` が、比べた相手の版と違う。
    #[error("release.json has version {found}, but {source_of} is {expected}")]
    VersionMismatch {
        /// 比べた相手。
        source_of: VersionSource,
        /// `release.json` の版。
        found: Version,
        /// 相手の値 (タグはそのまま)。
        expected: String,
    },
    /// 版が今の版より新しくない (巻き戻しは `rollback` だけ)。
    #[error("version {version} is not newer than the current version {current}")]
    NotNewer {
        /// `release.json` の版。
        version: Version,
        /// 今の版。
        current: Version,
    },
    /// `date` が今の版のリリースの日付より古い (凍結への備え、12 §7.5)。
    #[error("the release date {date} is older than the date {current} of the current version")]
    DateTooOld {
        /// `release.json` の日付。
        date: ReleaseDate,
        /// 今の版のリリースの日付。
        current: ReleaseDate,
    },
    /// 今の版が `upgrade_from` を下回る。先に `upgrade_from` の版へ更新するよう案内する。
    /// `update check` が版を示せるように、検証した中身と署名の鍵を持つ。
    #[error(
        "the current version is below upgrade_from {upgrade_from}; update to {upgrade_from} first"
    )]
    BelowUpgradeFrom {
        /// 先に入れる版。
        upgrade_from: Version,
        /// 検証した `release.json` の中身。
        release: Box<Manifest>,
        /// 署名の通った鍵。
        signer: Signer,
    },
    /// 今の対象の配布物が無い。
    #[error("release.json has no archive for {target}")]
    NoArtifact {
        /// 今の対象。
        target: String,
    },
    /// 今の対象の配布物が 2 つ以上ある。
    #[error("release.json has {count} archives for {target}")]
    AmbiguousArtifact {
        /// 今の対象。
        target: String,
        /// 当てはまる数。
        count: usize,
    },
    /// 配布物の大きさが `size` と違う。
    #[error("the archive is {actual} bytes, not {expected}")]
    ArchiveSizeMismatch {
        /// `release.json` の `size`。
        expected: u64,
        /// 取得した大きさ。
        actual: u64,
    },
    /// 配布物の SHA-256 が `sha256` と違う。
    #[error("the archive does not match its sha256")]
    ArchiveDigestMismatch,
    /// 検証した配布物を、圧縮の形式として読めない。
    #[error("the archive cannot be read: {reason}")]
    ArchiveUnreadable {
        /// 理由。
        reason: String,
    },
    /// 配布物に `exe` の項目が無い。
    #[error("the archive has no entry named like exe")]
    ExeMissing,
    /// `exe` の項目が通常のファイルでない (リンクなど)。
    #[error("the exe entry is not a regular file")]
    ExeNotRegularFile,
    /// 配布物に同じ名前の項目が 2 つ以上ある。
    #[error("the archive has two or more entries with the same name")]
    DuplicateEntries,
    /// 展開した実行ファイルが上限を超える。
    #[error("the exe entry is over {limit} bytes")]
    ExeTooLarge {
        /// 上限 (バイト)。
        limit: u64,
    },
    /// 展開した配布物の全体が上限を超える。
    #[error("the unpacked archive is over {limit} bytes")]
    UnpackedTooLarge {
        /// 上限 (バイト)。
        limit: u64,
    },
    /// 取り出した実行ファイルの SHA-256 が `exe_sha256` と違う。
    #[error("the exe does not match its exe_sha256")]
    ExeDigestMismatch,
}

impl UpdateError {
    /// 改ざんを疑うか (07 §5、AC-NFR-505-2)。
    #[must_use]
    pub const fn severity(&self) -> Severity {
        match self {
            Self::SignatureMalformed
            | Self::LegacySignature
            | Self::UnknownKey { .. }
            | Self::BadSignature { .. }
            | Self::TrustedCommentMismatch
            | Self::NameMismatch
            | Self::VersionMismatch { .. }
            | Self::DateTooOld { .. }
            | Self::ArchiveSizeMismatch { .. }
            | Self::ArchiveDigestMismatch
            | Self::ExeDigestMismatch => Severity::Tampered,
            Self::UnknownSchema
            | Self::Malformed { .. }
            | Self::NotNewer { .. }
            | Self::BelowUpgradeFrom { .. }
            | Self::NoArtifact { .. }
            | Self::AmbiguousArtifact { .. }
            | Self::ArchiveUnreadable { .. }
            | Self::ExeMissing
            | Self::ExeNotRegularFile
            | Self::DuplicateEntries
            | Self::ExeTooLarge { .. }
            | Self::UnpackedTooLarge { .. } => Severity::Unusable,
        }
    }

    /// 07 §5 の終了コード (5 か 1)。
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        self.severity().exit_code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const _: () = {
        const fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<UpdateError>();
    };

    #[test]
    fn exit_codes_follow_the_severity() {
        assert_eq!(UpdateError::LegacySignature.exit_code(), 5);
        assert_eq!(UpdateError::UnknownSchema.exit_code(), 1);
    }
}
