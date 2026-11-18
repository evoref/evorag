//! 署名から中身までの検証の順 (12 §7.2 の apply の 3・4)。

use crate::archive::VerifiedArchive;
use crate::manifest::{self, Artifact, Manifest};
use crate::{
    Keyring, ReleaseDate, Sha256Digest, Signer, SizedInput, UpdateError, Version, VersionSource,
};

/// 取得する `release.json` の大きさの上限 (12 §7.5)。
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// 取得する `release.json.minisig` の大きさの上限 (12 §7.5)。
pub const MAX_SIGNATURE_BYTES: usize = 4 * 1024;

/// `release.json` の署名の trusted comment (12 §5.2): `evorag <版> release.json sha256 <SHA-256>`。
#[must_use]
pub fn trusted_comment(version: Version, digest: &Sha256Digest) -> String {
    format!("evorag {version} release.json sha256 {digest}")
}

/// trusted comment から版を読み、全体がこのバイト列の決まった文字列とちょうど同じことを確かめる。
fn trusted_comment_version(comment: &str, digest: &Sha256Digest) -> Result<Version, UpdateError> {
    let version: Version = comment
        .strip_prefix("evorag ")
        .and_then(|rest| rest.split(' ').next())
        .and_then(|v| v.parse().ok())
        .ok_or(UpdateError::TrustedCommentMismatch)?;
    if comment == trusted_comment(version, digest) {
        Ok(version)
    } else {
        Err(UpdateError::TrustedCommentMismatch)
    }
}

/// 署名と trusted comment を確かめた `release.json` のバイト列。JSON としてはまだ読んでいない。
///
/// 作れるのは [`verify_signature`] だけなので、中身を読む ([`SignedManifest::parse`]・
/// [`SignedManifest::verify`]) のは、署名が通った後に限られる。
#[derive(Debug, Clone)]
pub struct SignedManifest<'a> {
    bytes: &'a [u8],
    comment_version: Version,
    signer: Signer,
}

/// `release.json` の署名を確かめる (12 §7.2 の apply の 3)。
///
/// 埋め込んだ鍵と足した鍵のうち、署名の鍵の ID と同じ鍵をすべて試す (prehash の形だけ)。全体の署名が
/// 通った後に、trusted comment が `evorag <版> release.json sha256 <SHA-256>` (`<SHA-256>` はこの
/// バイト列のもの) とちょうど同じことを確かめる。JSON としては読まない。
///
/// # Errors
///
/// `release.json` が [`MAX_MANIFEST_BYTES`]、`.minisig` が [`MAX_SIGNATURE_BYTES`] を超える (使えない
/// 理由、何も読まずに断る)。署名が通らない・legacy の署名・trusted comment が違う (改ざんを疑う理由)。
pub fn verify_signature<'a>(
    json: &'a [u8],
    minisig: &[u8],
    keys: &Keyring,
) -> Result<SignedManifest<'a>, UpdateError> {
    let over = |len: usize, max: usize, what| {
        (len > max).then_some(UpdateError::TooLarge {
            what,
            limit: max as u64,
        })
    };
    if let Some(e) = over(json.len(), MAX_MANIFEST_BYTES, SizedInput::Manifest)
        .or_else(|| over(minisig.len(), MAX_SIGNATURE_BYTES, SizedInput::Signature))
    {
        return Err(e);
    }
    let sig = keys.verify(json, minisig)?;
    let comment_version = trusted_comment_version(sig.trusted_comment(), &Sha256Digest::of(json))?;
    Ok(SignedManifest {
        bytes: json,
        comment_version,
        signer: sig.signer(),
    })
}

/// `release.json` の署名と中身を確かめる ([`verify_signature`] と [`SignedManifest::verify`])。
///
/// # Errors
///
/// [`verify_signature`] と [`SignedManifest::verify`] のとおり。
pub fn verify_manifest(
    json: &[u8],
    minisig: &[u8],
    keys: &Keyring,
    expect: &Expectations,
) -> Result<VerifiedRelease, UpdateError> {
    verify_signature(json, minisig, keys)?.verify(expect)
}

impl SignedManifest<'_> {
    /// 署名が通った鍵。
    #[must_use]
    pub const fn signer(&self) -> Signer {
        self.signer
    }

    /// trusted comment の版。
    #[must_use]
    pub const fn trusted_comment_version(&self) -> Version {
        self.comment_version
    }

    /// 中身を読み、形を確かめる。順は、`schema` が 1、`name` が `evorag`、`version` が trusted comment
    /// の版とちょうど同じ文字列、必須の項目と形の条件。今の版との比べは [`SignedManifest::verify`] で行う。
    ///
    /// # Errors
    ///
    /// `schema` を知らない・項目が欠ける・型や形が違う (使えない理由)、`name`・`version` が違う
    /// (`version` が版として読めないものを含む。改ざんを疑う理由)。
    pub fn parse(&self) -> Result<Manifest, UpdateError> {
        manifest::parse(self.bytes, Some(self.comment_version))
    }

    /// 中身を確かめる (12 §7.2 の apply の 4)。順は、形 ([`SignedManifest::parse`])、タグと指定した版、
    /// 今より新しいこと、日付、`upgrade_from`、今の対象の配布物。
    ///
    /// # Errors
    ///
    /// 改ざんを疑う理由: `name`・版・タグ・指定した版の不一致、`date` が今の版のリリースの日付より古い。
    /// 使えない理由: `schema` を知らない・形が違う、今と同じか古い版、今の版が `upgrade_from` を下回る
    /// ([`UpdateError::BelowUpgradeFrom`] は中身を持つ)、今の対象の配布物を 1 つに決められない。
    pub fn verify(&self, expect: &Expectations) -> Result<VerifiedRelease, UpdateError> {
        let manifest = self.parse()?;
        let version = manifest.version();
        let mismatch = |source_of, expected: Version| UpdateError::VersionMismatch {
            source_of,
            found: version.to_string(),
            expected: expected.to_string(),
        };
        if let Some(tag) = expect.tag
            && tag != version
        {
            return Err(mismatch(VersionSource::Tag, tag));
        }
        if let Some(requested) = expect.requested_version
            && requested != version
        {
            return Err(mismatch(VersionSource::Requested, requested));
        }
        if version <= expect.current_version {
            return Err(UpdateError::NotNewer {
                version,
                current: expect.current_version,
            });
        }
        if let Some(current) = expect.release_date
            && manifest.date() < current
        {
            return Err(UpdateError::DateTooOld {
                date: manifest.date(),
                current,
            });
        }
        if expect.current_version < manifest.upgrade_from() {
            return Err(UpdateError::BelowUpgradeFrom {
                upgrade_from: manifest.upgrade_from(),
                release: Box::new(manifest),
                signer: self.signer,
            });
        }
        let artifact = manifest.artifact_for(&expect.target)?.clone();
        Ok(VerifiedRelease {
            manifest,
            artifact,
            signer: self.signer,
        })
    }
}

/// 配布物の入れ方 (`EVORAG_CHANNEL`、12 §7.4)。この経路のビルドは、リリースの日付を必ず持つ。
pub const ZIP_CHANNEL: &str = "zip";

/// 今のビルドのリリースの日付を決める (12 §7.2 の apply の 4)。呼ぶ側は
/// `release_date_of_build(env!("EVORAG_CHANNEL"), option_env!("EVORAG_RELEASE_DATE"))` と渡す。
///
/// - 入れ方が `zip` なら、日付が必ず要る。無い・読めないときは、日付の確かめを飛ばさずに断る
///   (fail-closed)。
/// - ほかの入れ方 (開発のビルド・`cargo install`) で日付が無ければ `None` (確かめない)。日付があるのに
///   読めなければ断る。
///
/// # Errors
///
/// 上の断る場合 ([`UpdateError::BuildDateUnreadable`]、使えない理由)。
pub fn release_date_of_build(
    channel: &str,
    embedded: Option<&str>,
) -> Result<Option<ReleaseDate>, UpdateError> {
    match embedded.filter(|d| !d.is_empty()) {
        Some(date) => date
            .parse()
            .map(Some)
            .map_err(|_| UpdateError::BuildDateUnreadable),
        None if channel == ZIP_CHANNEL => Err(UpdateError::BuildDateUnreadable),
        None => Ok(None),
    }
}

/// 中身と比べる、今の evorag の値 (12 §7.2 の apply の 4)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expectations {
    current_version: Version,
    target: String,
    tag: Option<Version>,
    requested_version: Option<Version>,
    release_date: Option<ReleaseDate>,
}

impl Expectations {
    /// 今の版と対象 (target triple)。
    #[must_use]
    pub fn new(current_version: Version, target: &str) -> Self {
        Self {
            current_version,
            target: target.to_owned(),
            tag: None,
            requested_version: None,
            release_date: None,
        }
    }

    /// `release.json` を取得した URL のタグの版 ([`Version::from_tag`] で読んだもの)。`version` と同じで
    /// なければ断る。
    #[must_use]
    pub const fn with_tag(mut self, tag: Version) -> Self {
        self.tag = Some(tag);
        self
    }

    /// 利用者が指定した版 (`evorag update apply <版>`)。
    #[must_use]
    pub const fn with_requested_version(mut self, version: Version) -> Self {
        self.requested_version = Some(version);
        self
    }

    /// 今の版のリリースの日付 (`EVORAG_RELEASE_DATE`)。開発のビルドでは与えない (日付を確かめない)。
    /// 値は [`release_date_of_build`] で求める (配布物の版で日付が読めなければ、更新を断る)。
    #[must_use]
    pub const fn with_release_date(mut self, date: ReleaseDate) -> Self {
        self.release_date = Some(date);
        self
    }
}

/// 署名と中身を確かめた更新の先。示す文字列は、これから取る (12 §7.2 の apply の 5)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRelease {
    manifest: Manifest,
    artifact: Artifact,
    signer: Signer,
}

impl VerifiedRelease {
    /// 確かめた `release.json` の中身。
    #[must_use]
    pub const fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// 版。
    #[must_use]
    pub const fn version(&self) -> Version {
        self.manifest.version()
    }

    /// 今の対象の配布物。
    #[must_use]
    pub const fn artifact(&self) -> &Artifact {
        &self.artifact
    }

    /// 署名が通った鍵。[`crate::KeySource::Added`] なら、鍵の ID を示して警告する (12 §5.2)。
    #[must_use]
    pub const fn signer(&self) -> Signer {
        self.signer
    }

    /// 取得した配布物の大きさと SHA-256 を確かめる (12 §7.2 の apply の 6)。圧縮の形式としては
    /// 読まない。
    ///
    /// # Errors
    ///
    /// [`crate::MAX_ARCHIVE_BYTES`] を超える (使えない理由、何も読まずに断る)。大きさか SHA-256 が
    /// `size`・`sha256` と違う (改ざんを疑う理由)。
    pub fn verify_archive(&self, bytes: Vec<u8>) -> Result<VerifiedArchive, UpdateError> {
        VerifiedArchive::check(&self.artifact, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// lib.rs の compile_fail の例が、すべての項目を書いていること (項目の非公開だけで失敗すること)。
    #[test]
    fn the_compile_fail_literal_names_every_field() {
        fn forge_signed(signer: Signer) -> SignedManifest<'static> {
            SignedManifest {
                bytes: b"{}",
                comment_version: Version::new(0, 3, 0),
                signer,
            }
        }
        fn forge_release(
            manifest: Manifest,
            artifact: Artifact,
            signer: Signer,
        ) -> VerifiedRelease {
            VerifiedRelease {
                manifest,
                artifact,
                signer,
            }
        }
        let _ = (forge_signed, forge_release);
    }

    #[test]
    fn a_zip_build_without_a_readable_date_refuses_to_update() {
        let date = |s: &str| s.parse::<ReleaseDate>().expect(s);
        assert_eq!(
            release_date_of_build("zip", Some("2026-11-18")).ok(),
            Some(Some(date("2026-11-18")))
        );
        for bad in [None, Some(""), Some("2026-02-30"), Some("2026/11/18")] {
            assert!(
                matches!(
                    release_date_of_build("zip", bad),
                    Err(UpdateError::BuildDateUnreadable)
                ),
                "{bad:?}"
            );
        }
        // 開発のビルド・cargo install は、日付が無ければ確かめない。あるのに読めなければ断る。
        assert_eq!(release_date_of_build("cargo", None).ok(), Some(None));
        assert_eq!(release_date_of_build("cargo", Some("")).ok(), Some(None));
        assert_eq!(
            release_date_of_build("cargo", Some("2026-11-18")).ok(),
            Some(Some(date("2026-11-18")))
        );
        assert!(release_date_of_build("cargo", Some("someday")).is_err());
        assert_eq!(UpdateError::BuildDateUnreadable.exit_code(), 1);
    }

    #[test]
    fn oversized_inputs_are_refused_before_they_are_read() {
        let ring = Keyring::embedded();
        let big = vec![b' '; MAX_MANIFEST_BYTES + 1];
        let e = verify_signature(&big, b"", &ring).expect_err("too large");
        assert!(
            matches!(
                e,
                UpdateError::TooLarge {
                    what: SizedInput::Manifest,
                    ..
                }
            ),
            "{e}"
        );
        let sig = vec![b'a'; MAX_SIGNATURE_BYTES + 1];
        let e = verify_signature(b"{}", &sig, &ring).expect_err("too large");
        assert!(
            matches!(
                e,
                UpdateError::TooLarge {
                    what: SizedInput::Signature,
                    ..
                }
            ),
            "{e}"
        );
        assert_eq!(e.exit_code(), 1);
        // 上限ちょうどは大きさでは断らない (署名の形で断る)。
        let e = verify_signature(&vec![b' '; MAX_MANIFEST_BYTES], b"", &ring)
            .expect_err("no signature");
        assert!(matches!(e, UpdateError::SignatureMalformed), "{e}");
    }
}
