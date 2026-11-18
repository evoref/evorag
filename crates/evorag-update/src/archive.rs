//! 配布物の検証と、実行ファイルの 1 項目だけの取り出し (12 §7.2 の apply の 6・7)。

use std::collections::BTreeSet;
use std::io::{self, Cursor, Read};

use crate::manifest::{ArchiveFormat, Artifact};
use crate::{Sha256Digest, UpdateError};

/// 取得する配布物の大きさの上限 (12 §7.5。`size` と小さいほうを使う)。
pub const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

/// 取り出す実行ファイルの大きさの上限 (12 §7.2 の apply の 7)。
pub const MAX_EXE_BYTES: u64 = 256 * 1024 * 1024;

/// 展開する配布物の全体の上限。tar.gz は同じ名前の項目を見つけるために終わりまで読むので、
/// 圧縮の比の大きい配布物で止まらないように限る。実行ファイルの上限に、同梱の文書の分を足したもの。
const MAX_UNPACKED_BYTES: u64 = MAX_EXE_BYTES + 64 * 1024 * 1024;

/// 展開の上限。試験では小さくする。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) exe: u64,
    pub(crate) unpacked: u64,
}

pub(crate) const LIMITS: Limits = Limits {
    exe: MAX_EXE_BYTES,
    unpacked: MAX_UNPACKED_BYTES,
};

/// 大きさと SHA-256 を確かめた配布物。作れるのは [`crate::VerifiedRelease::verify_archive`] だけなので、
/// 圧縮の形式として読むのは、確かめた後に限られる。
#[derive(Clone)]
pub struct VerifiedArchive {
    bytes: Vec<u8>,
    format: ArchiveFormat,
    exe: String,
    exe_sha256: Sha256Digest,
}

impl std::fmt::Debug for VerifiedArchive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedArchive")
            .field("len", &self.bytes.len())
            .field("format", &self.format)
            .field("exe", &self.exe)
            .finish_non_exhaustive()
    }
}

impl VerifiedArchive {
    pub(crate) fn check(artifact: &Artifact, bytes: Vec<u8>) -> Result<Self, UpdateError> {
        let actual = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if actual > MAX_ARCHIVE_BYTES {
            return Err(UpdateError::TooLarge {
                what: crate::SizedInput::Archive,
                limit: MAX_ARCHIVE_BYTES,
            });
        }
        if actual != artifact.size() {
            return Err(UpdateError::ArchiveSizeMismatch {
                expected: artifact.size(),
                actual,
            });
        }
        if Sha256Digest::of(&bytes) != artifact.sha256() {
            return Err(UpdateError::ArchiveDigestMismatch);
        }
        let format = artifact.format().ok_or_else(|| UpdateError::NoArtifact {
            target: artifact.target().to_owned(),
        })?;
        Ok(Self {
            bytes,
            format,
            exe: artifact.exe().to_owned(),
            exe_sha256: artifact.exe_sha256(),
        })
    }

    /// 配布物の形式 (名前の末尾で決めたもの)。
    #[must_use]
    pub const fn format(&self) -> ArchiveFormat {
        self.format
    }

    /// 名前が `exe` と同じ項目を 1 つだけ主記憶に取り出し、SHA-256 が `exe_sha256` と合うことを
    /// 確かめる。ほかの項目は展開しない (tar.gz は同じ名前を見つけるために読み飛ばす)。
    ///
    /// # Errors
    ///
    /// 読めない・`exe` が無い・通常のファイルでない・同じ名前の項目が 2 つ以上・大きすぎる (使えない
    /// 理由)、取り出したものの SHA-256 が違う (改ざんを疑う理由)。
    pub fn extract_exe(&self) -> Result<Executable, UpdateError> {
        let bytes = read_exe(self.format, &self.bytes, &self.exe, LIMITS)?;
        let sha256 = Sha256Digest::of(&bytes);
        if sha256 != self.exe_sha256 {
            return Err(UpdateError::ExeDigestMismatch);
        }
        Ok(Executable { bytes, sha256 })
    }
}

/// 取り出して `exe_sha256` を確かめた実行ファイル。
#[derive(Clone)]
pub struct Executable {
    bytes: Vec<u8>,
    sha256: Sha256Digest,
}

impl std::fmt::Debug for Executable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executable")
            .field("len", &self.bytes.len())
            .field("sha256", &self.sha256)
            .finish()
    }
}

impl Executable {
    /// 実行ファイルのバイト列。
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 実行ファイルのバイト列を取り出す。
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// SHA-256 (`exe_sha256` と同じ)。
    #[must_use]
    pub const fn sha256(&self) -> Sha256Digest {
        self.sha256
    }
}

fn unreadable(e: impl std::fmt::Display) -> UpdateError {
    UpdateError::ArchiveUnreadable {
        reason: crate::sanitize_for_display(&e.to_string(), crate::DISPLAY_MAX_BYTES),
    }
}

/// 項目を `limit` まで読む。超えれば断る。
fn read_capped(entry: impl Read, limit: u64) -> Result<Vec<u8>, UpdateError> {
    let mut out = Vec::new();
    entry
        .take(limit.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(unreadable)?;
    if u64::try_from(out.len()).unwrap_or(u64::MAX) > limit {
        return Err(UpdateError::ExeTooLarge { limit });
    }
    Ok(out)
}

/// 名前が `exe` と同じ項目のバイト列を返す。配布物を確かめた後 ([`VerifiedArchive`]) か、
/// `cargo xtask` が自分で作った配布物 (`unverified-dist` の機能) だけを渡す。
pub(crate) fn read_exe(
    format: ArchiveFormat,
    bytes: &[u8],
    exe: &str,
    limits: Limits,
) -> Result<Vec<u8>, UpdateError> {
    match format {
        ArchiveFormat::Zip => read_zip(bytes, exe, limits),
        ArchiveFormat::TarGz => read_tar_gz(bytes, exe, limits),
    }
}

fn read_zip(bytes: &[u8], exe: &str, limits: Limits) -> Result<Vec<u8>, UpdateError> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(unreadable)?;
    if zip.has_duplicate_names() {
        return Err(UpdateError::DuplicateEntries);
    }
    let index = zip
        .index_for_name_raw(exe.as_bytes())
        .ok_or(UpdateError::ExeMissing)?;
    let options = zip::read::ZipReadOptions::new().enforce_header_consistency(true);
    let file = zip
        .by_index_with_options(index, options)
        .map_err(unreadable)?;
    if !file.is_file() {
        return Err(UpdateError::ExeNotRegularFile);
    }
    read_capped(file, limits.exe)
}

/// 読んだバイト数を数え、上限を超えたら止める。
struct Capped<R> {
    inner: R,
    left: u64,
    hit: bool,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            // ちょうど上限で終わるのか、まだ続くのかを 1 バイト読んで決める。
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => {
                    self.hit = true;
                    Err(io::Error::other("the unpacked archive is over the limit"))
                }
            };
        }
        let max = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..max])?;
        self.left -= u64::try_from(n).expect("read returns at most the buffer length");
        Ok(n)
    }
}

fn read_tar_gz(bytes: &[u8], exe: &str, limits: Limits) -> Result<Vec<u8>, UpdateError> {
    let mut stream = Capped {
        inner: flate2::read::GzDecoder::new(bytes),
        left: limits.unpacked,
        hit: false,
    };
    let result = scan_tar(&mut stream, exe, limits.exe);
    match result {
        Err(_) if stream.hit => Err(UpdateError::UnpackedTooLarge {
            limit: limits.unpacked,
        }),
        Err(e) => Err(e),
        Ok(None) => Err(UpdateError::ExeMissing),
        Ok(Some(exe)) => Ok(exe),
    }
}

/// tar を終わりまで読み、名前の重なりを確かめながら、`exe` の項目だけを読む。
fn scan_tar(stream: impl Read, exe: &str, limit: u64) -> Result<Option<Vec<u8>>, UpdateError> {
    let mut archive = tar::Archive::new(stream);
    let mut names = BTreeSet::new();
    let mut found = None;
    for entry in archive.entries().map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let name = entry.path_bytes().into_owned();
        let is_exe = name == exe.as_bytes();
        if !names.insert(name) {
            return Err(UpdateError::DuplicateEntries);
        }
        if !is_exe {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(UpdateError::ExeNotRegularFile);
        }
        if entry.size() > limit {
            return Err(UpdateError::ExeTooLarge { limit });
        }
        found = Some(read_capped(entry, limit)?);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const EXE: &str = "evorag-0.3.0-x86_64-pc-windows-msvc/evorag.exe";

    fn zip_with(exe: &[u8]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file(EXE, options).expect("start");
        w.write_all(exe).expect("write");
        w.finish().expect("finish").into_inner()
    }

    fn tar_gz_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut b = tar::Builder::new(gz);
        for (name, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            b.append_data(&mut h, name, *data).expect("append");
        }
        b.into_inner().expect("tar").finish().expect("gz")
    }

    const SMALL: Limits = Limits {
        exe: 1000,
        unpacked: 100_000,
    };

    #[test]
    fn the_exe_entry_has_a_size_limit() {
        let zip = zip_with(&[7u8; 1000]);
        assert_eq!(
            read_exe(ArchiveFormat::Zip, &zip, EXE, SMALL)
                .map(|v| v.len())
                .ok(),
            Some(1000)
        );
        let zip = zip_with(&[7u8; 1001]);
        assert!(matches!(
            read_exe(ArchiveFormat::Zip, &zip, EXE, SMALL),
            Err(UpdateError::ExeTooLarge { limit: 1000 })
        ));
        let tgz = tar_gz_with(&[(EXE, &[7u8; 1001])]);
        assert!(matches!(
            read_exe(ArchiveFormat::TarGz, &tgz, EXE, SMALL),
            Err(UpdateError::ExeTooLarge { limit: 1000 })
        ));
    }

    #[test]
    fn the_whole_tar_gz_has_a_size_limit() {
        let filler = vec![0u8; 200_000];
        let tgz = tar_gz_with(&[(EXE, b"exe"), ("doc", &filler)]);
        assert!(matches!(
            read_exe(ArchiveFormat::TarGz, &tgz, EXE, SMALL),
            Err(UpdateError::UnpackedTooLarge { limit: 100_000 })
        ));
    }

    /// lib.rs の compile_fail の例が、すべての項目を書いていること (項目の非公開だけで失敗すること)。
    #[test]
    fn the_compile_fail_literal_names_every_field() {
        fn forge() -> VerifiedArchive {
            VerifiedArchive {
                bytes: Vec::new(),
                format: ArchiveFormat::Zip,
                exe: String::new(),
                exe_sha256: Sha256Digest::of(b""),
            }
        }
        assert_eq!(forge().format(), ArchiveFormat::Zip);
    }

    #[test]
    fn the_limits_are_the_specified_ones() {
        assert_eq!(MAX_EXE_BYTES, 256 * 1024 * 1024);
        assert_eq!(MAX_ARCHIVE_BYTES, 64 * 1024 * 1024);
        const { assert!(LIMITS.unpacked > LIMITS.exe) };
    }
}
