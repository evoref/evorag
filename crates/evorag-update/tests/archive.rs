//! 配布物の検証と、実行ファイルの取り出し (12 §7.2 の apply の 6・7、09 §3.6)。

mod common;

use common::{
    LINUX, LINUX_EXE, LINUX_TGZ, Release, TarEntry, WIN_EXE, WIN_ZIP, WINDOWS, archives, exe_bytes,
    key, keyring, tar_gz, tar_gz_declaring, tar_gz_raw_name, v, zip_archive, zip_with_symlink,
};
use evorag_update::{
    ArchiveFormat, Expectations, MAX_ARCHIVE_BYTES, MAX_EXE_BYTES, Severity, SizedInput,
    UpdateError, VerifiedRelease, verify_manifest,
};

/// 配布物を差し替えたリリースを、署名して確かめる。
fn release_with(win_zip: &[u8], linux_tgz: &[u8], target: &str) -> VerifiedRelease {
    let r = Release::new(win_zip, linux_tgz);
    let k = key();
    let (json, sig) = r.signed(&k);
    verify_manifest(
        &json,
        &sig,
        &keyring(&[&k]),
        &Expectations::new(v("0.2.1"), target),
    )
    .expect("the manifest verifies")
}

fn refused(r: Result<impl std::fmt::Debug, UpdateError>, severity: Severity) -> UpdateError {
    let e = r.expect_err("must be refused");
    assert_eq!(e.severity(), severity, "{e}");
    e
}

#[test]
fn the_exe_is_taken_out_of_a_zip_and_a_tar_gz() {
    let (zip, tgz) = archives();
    let win = release_with(&zip, &tgz, WINDOWS);
    let archive = win
        .verify_archive(zip.clone())
        .expect("size and sha256 match");
    assert_eq!(archive.format(), ArchiveFormat::Zip);
    let exe = archive.extract_exe().expect("the exe entry");
    assert_eq!(exe.as_bytes(), exe_bytes("win"));
    assert_eq!(exe.sha256(), win.artifact().exe_sha256());

    let linux = release_with(&zip, &tgz, LINUX);
    let archive = linux.verify_archive(tgz).expect("size and sha256 match");
    assert_eq!(archive.format(), ArchiveFormat::TarGz);
    assert_eq!(
        archive.extract_exe().expect("exe").into_bytes(),
        exe_bytes("linux")
    );
}

/// verifies: AC-NFR-505-2 (大きさ・`sha256` の合わない配布物は 5 で、展開しない)
#[test]
fn an_archive_with_the_wrong_size_or_sha256_is_refused_before_it_is_unpacked() {
    let (zip, tgz) = archives();
    let win = release_with(&zip, &tgz, WINDOWS);

    let mut longer = zip.clone();
    longer.push(0);
    let e = refused(win.verify_archive(longer), Severity::Tampered);
    assert!(matches!(e, UpdateError::ArchiveSizeMismatch { .. }), "{e}");

    // 同じ大きさで中身の違うもの。zip として読めないバイト列でも、展開の前に SHA-256 で断る。
    let garbage = vec![0xa5u8; zip.len()];
    let e = refused(win.verify_archive(garbage), Severity::Tampered);
    assert!(matches!(e, UpdateError::ArchiveDigestMismatch), "{e}");
    let mut flipped = zip.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    let e = refused(win.verify_archive(flipped), Severity::Tampered);
    assert!(matches!(e, UpdateError::ArchiveDigestMismatch), "{e}");
}

#[test]
fn an_archive_over_64_mib_is_refused_before_it_is_hashed() {
    let (zip, tgz) = archives();
    let win = release_with(&zip, &tgz, WINDOWS);
    let huge = vec![0u8; usize::try_from(MAX_ARCHIVE_BYTES).expect("fits") + 1];
    let e = refused(win.verify_archive(huge), Severity::Unusable);
    assert!(
        matches!(e, UpdateError::TooLarge { what: SizedInput::Archive, limit } if limit == MAX_ARCHIVE_BYTES),
        "{e}"
    );
}

/// verifies: AC-NFR-505-2 (`exe_sha256` の合わない配布物は 5)
#[test]
fn an_exe_with_the_wrong_sha256_is_refused() {
    let other_zip = zip_archive(&[(WIN_EXE, b"another program")]);
    let (_, tgz) = archives();
    // release.json は other_zip の sha256 を持つが、exe_sha256 は exe_bytes("win") のもの。
    let win = release_with(&other_zip, &tgz, WINDOWS);
    let archive = win
        .verify_archive(other_zip)
        .expect("the archive itself matches");
    let e = refused(archive.extract_exe(), Severity::Tampered);
    assert!(matches!(e, UpdateError::ExeDigestMismatch), "{e}");

    let other_tgz = tar_gz(&[TarEntry::File(LINUX_EXE, b"another program")]);
    let (zip, _) = archives();
    let linux = release_with(&zip, &other_tgz, LINUX);
    let e = refused(
        linux
            .verify_archive(other_tgz)
            .expect("matches")
            .extract_exe(),
        Severity::Tampered,
    );
    assert!(matches!(e, UpdateError::ExeDigestMismatch), "{e}");
}

/// 配布物を確かめた後に、取り出しで断る (使えない理由、1)。
fn extract_err(win_zip: Vec<u8>, linux_tgz: Vec<u8>, target: &str) -> UpdateError {
    let r = release_with(&win_zip, &linux_tgz, target);
    let bytes = if target == WINDOWS {
        win_zip
    } else {
        linux_tgz
    };
    let archive = r.verify_archive(bytes).expect("size and sha256 match");
    refused(archive.extract_exe(), Severity::Unusable)
}

#[test]
fn a_link_in_place_of_the_exe_is_refused() {
    let (zip, tgz) = archives();
    for entries in [
        vec![TarEntry::Symlink(LINUX_EXE, "/bin/sh")],
        vec![
            TarEntry::File("evorag-0.3.0-x86_64-unknown-linux-musl/real", b"x"),
            TarEntry::Hardlink(LINUX_EXE, "evorag-0.3.0-x86_64-unknown-linux-musl/real"),
        ],
        vec![TarEntry::Dir(LINUX_EXE)],
    ] {
        let e = extract_err(zip.clone(), tar_gz(&entries), LINUX);
        assert!(matches!(e, UpdateError::ExeNotRegularFile), "{e}");
    }
    let e = extract_err(
        zip_with_symlink(WIN_EXE, "C:/Windows/notepad.exe"),
        tgz,
        WINDOWS,
    );
    assert!(matches!(e, UpdateError::ExeNotRegularFile), "{e}");
}

#[test]
fn duplicate_entries_are_refused() {
    let (zip, tgz) = archives();
    let dup_tgz = tar_gz(&[
        TarEntry::File(LINUX_EXE, &exe_bytes("linux")),
        TarEntry::File(LINUX_EXE, b"second"),
    ]);
    let e = extract_err(zip.clone(), dup_tgz, LINUX);
    assert!(matches!(e, UpdateError::DuplicateEntries), "{e}");

    // zip の書き手は同じ名前を拒むので、同じ長さの別の名前で作ってから名前を書き換える。
    let twin = WIN_EXE.replace("evorag.exe", "evorag.exf");
    let mut dup_zip = zip_archive(&[(WIN_EXE, &exe_bytes("win")), (&twin, b"second")]);
    let (from, to) = (twin.as_bytes(), WIN_EXE.as_bytes());
    let mut i = 0;
    while i + from.len() <= dup_zip.len() {
        if &dup_zip[i..i + from.len()] == from {
            dup_zip[i..i + from.len()].copy_from_slice(to);
        }
        i += 1;
    }
    let e = extract_err(dup_zip, tgz, WINDOWS);
    assert!(matches!(e, UpdateError::DuplicateEntries), "{e}");
}

#[test]
fn a_missing_exe_is_refused() {
    let (zip, tgz) = archives();
    let e = extract_err(
        zip_archive(&[("other/evorag.exe", b"x")]),
        tgz.clone(),
        WINDOWS,
    );
    assert!(matches!(e, UpdateError::ExeMissing), "{e}");
    // 名前は完全に一致するものだけ (`./` の付くものは別の名前)。
    let dotted = tar_gz_raw_name(&format!("./{LINUX_EXE}"), &exe_bytes("linux"));
    let e = extract_err(zip, dotted, LINUX);
    assert!(matches!(e, UpdateError::ExeMissing), "{e}");
}

#[test]
fn an_exe_over_256_mib_is_refused() {
    let (zip, _) = archives();
    let big = tar_gz_declaring(LINUX_EXE, MAX_EXE_BYTES + 1);
    let e = extract_err(zip, big, LINUX);
    assert!(
        matches!(e, UpdateError::ExeTooLarge { limit } if limit == MAX_EXE_BYTES),
        "{e}"
    );
}

#[test]
fn an_archive_that_is_not_its_format_is_refused_after_verification() {
    // release.json の sha256 と合うが、名前の末尾の形式として読めない。
    let (_, tgz) = archives();
    let not_zip = b"this is not a zip archive".to_vec();
    let e = extract_err(not_zip, tgz, WINDOWS);
    assert!(matches!(e, UpdateError::ArchiveUnreadable { .. }), "{e}");
    let names = [WIN_ZIP, LINUX_TGZ];
    assert!(names.iter().all(|n| ArchiveFormat::from_name(n).is_some()));
}
