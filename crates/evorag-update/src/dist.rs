//! `cargo xtask dist`・`verify-release` 向け: 署名の前の `release.json` と、自分で作った配布物を、
//! 本体の更新と同じ規則で読む (機能 `dist`)。
//!
//! 取得したもの (信頼できないバイト列) には使わない。本体は [`crate::verify_signature`] からの順で読む。

use crate::UpdateError;
use crate::archive::{self, LIMITS};
use crate::manifest::{self, ArchiveFormat, Manifest};

/// 署名を確かめずに `release.json` を読み、schema 1 の読む側の規則 (12 §5.3) を当てる。
///
/// # Errors
///
/// [`crate::SignedManifest::parse`] と同じ (trusted comment の版との比べを除く)。
pub fn parse_unsigned(json: &[u8]) -> Result<Manifest, UpdateError> {
    manifest::parse(json)
}

/// 自分で作った配布物から、名前が `exe` と同じ項目を取り出す。形式は配布物の名前の末尾で決める。
///
/// # Errors
///
/// 名前の末尾が `.zip`・`.tar.gz` でない、または本体の取り出しと同じ条件 (`exe` が無い・通常の
/// ファイルでない・同じ名前の項目が 2 つ以上・大きすぎる・読めない) に当たるとき。
pub fn exe_in_archive(archive_name: &str, bytes: &[u8], exe: &str) -> Result<Vec<u8>, UpdateError> {
    let format = ArchiveFormat::from_name(archive_name).ok_or_else(|| UpdateError::NoArtifact {
        target: archive_name.to_owned(),
    })?;
    archive::read_exe(format, bytes, exe, LIMITS)
}
