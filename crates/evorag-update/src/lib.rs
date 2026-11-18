//! evorag 本体の更新の検証: 署名つきの `release.json` と配布物を確かめる (12 §5.2・§5.3・§7.2)。
//!
//! 通信もファイルの置き換えもしない、純粋な核。確かめる順は型で決まっている。
//!
//! 1. [`verify_signature`]: `release.json` のバイト列の minisign の署名 (prehash の形だけ) と、
//!    trusted comment (`evorag <版> release.json sha256 <SHA-256>`) を確かめ、[`SignedManifest`] を返す。
//!    JSON として読むのは、これが通った後だけ。
//! 2. [`SignedManifest::verify`]: 中身を読み、[`Expectations`] (今の版・対象・タグなど) と比べて
//!    [`VerifiedRelease`] を返す。[`verify_manifest`] は 1 と 2 をまとめたもの。
//! 3. [`VerifiedRelease::verify_archive`]: 配布物の大きさと SHA-256 を確かめ、[`VerifiedArchive`] を返す。
//!    配布物を圧縮の形式として読むのは、これが通った後だけ。
//! 4. [`VerifiedArchive::extract_exe`]: `exe` の 1 項目だけを主記憶へ取り出し、`exe_sha256` を確かめて
//!    [`Executable`] を返す。
//!
//! 断る理由は [`UpdateError`] で、[`UpdateError::severity`] が改ざんを疑うもの (終了コード 5) と
//! 使えないもの (終了コード 1) を分ける (07 §5)。
//!
//! 確かめる前のバイト列から、検証済みの型は作れない (AC-NFR-505-2: 署名と配布物の検証の前に、JSON と
//! して読まず、展開しない)。
//!
//! 次の 3 つは、すべての項目を書いても、項目が非公開なので作れない (E0451)。同じ形の値をクレートの中で
//! 作れることは、各型のモジュールの試験 `the_compile_fail_literal_names_every_field` が確かめる。
//!
//! ```compile_fail,E0451
//! // 署名を確かめずに、中身を読める型を作れない。
//! use evorag_update::{SignedManifest, Signer, Version};
//! fn forge(signer: Signer) -> SignedManifest<'static> {
//!     SignedManifest { bytes: b"{}", comment_version: Version::new(0, 3, 0), signer }
//! }
//! ```
//!
//! ```compile_fail,E0451
//! // 中身を確かめずに、配布物を確かめられる型を作れない。
//! use evorag_update::{Artifact, Manifest, Signer, VerifiedRelease};
//! fn forge(manifest: Manifest, artifact: Artifact, signer: Signer) -> VerifiedRelease {
//!     VerifiedRelease { manifest, artifact, signer }
//! }
//! ```
//!
//! ```compile_fail,E0451
//! // 大きさと SHA-256 を確かめずに、展開できる型を作れない。
//! use evorag_update::{ArchiveFormat, Sha256Digest, VerifiedArchive};
//! fn forge() -> VerifiedArchive {
//!     VerifiedArchive {
//!         bytes: Vec::new(),
//!         format: ArchiveFormat::Zip,
//!         exe: String::new(),
//!         exe_sha256: Sha256Digest::of(b""),
//!     }
//! }
//! ```

mod archive;
mod base64;
mod date;
mod digest;
mod error;
mod keys;
mod manifest;
mod notes;
mod signed;
pub mod swap;
mod version;

#[cfg(feature = "unverified-dist")]
pub mod unverified_dist;

pub use archive::{Executable, MAX_ARCHIVE_BYTES, MAX_EXE_BYTES, VerifiedArchive};
pub use date::{ParseDateError, ReleaseDate};
pub use digest::{ParseDigestError, Sha256Digest};
pub use error::{Severity, SizedInput, UpdateError, VersionSource};
pub use keys::{KeyFormatError, KeyId, KeySource, Keyring, Signer, TrustedKey, VerifiedSignature};
pub use manifest::{ArchiveFormat, Artifact, Manifest, NAME, SCHEMA};
pub use notes::{DISPLAY_MAX_BYTES, sanitize_for_display};
pub use signed::{
    Expectations, MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES, SignedManifest, VerifiedRelease,
    ZIP_CHANNEL, release_date_of_build, trusted_comment, verify_manifest, verify_signature,
};
pub use version::{ParseVersionError, Version};
