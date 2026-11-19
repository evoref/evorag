//! 試験の道具: 試験だけの鍵での minisign の署名と、試験の `release.json`・配布物。
//!
//! 鍵は決まった種 (seed) から作る試験だけのもの。本体には埋め込まず、`update.trusted_keys` で足す鍵
//! として使う (09 §3.6)。

#![allow(
    dead_code,
    reason = "each test file uses a different part of the helpers"
)]

use std::io::{Cursor, Write};

use blake2::{Blake2b512, Digest};
use ed25519_compact::{KeyPair, Seed};
use evorag_update::{Keyring, Sha256Digest, TrustedKey, Version};

pub const WINDOWS: &str = "x86_64-pc-windows-msvc";
pub const LINUX: &str = "x86_64-unknown-linux-musl";
pub const WIN_EXE: &str = "evorag-0.3.0-x86_64-pc-windows-msvc/evorag.exe";
pub const LINUX_EXE: &str = "evorag-0.3.0-x86_64-unknown-linux-musl/evorag";
pub const WIN_ZIP: &str = "evorag-0.3.0-x86_64-pc-windows-msvc.zip";
pub const LINUX_TGZ: &str = "evorag-0.3.0-x86_64-unknown-linux-musl.tar.gz";

fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let v = chunk
            .iter()
            .enumerate()
            .fold(0u32, |v, (i, &b)| v | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(A[(v >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// 試験だけの minisign の鍵。
pub struct TestKey {
    pair: KeyPair,
    id: [u8; 8],
}

impl TestKey {
    /// 決まった種と鍵の ID から作る。
    pub fn new(seed: u8, id: [u8; 8]) -> Self {
        Self {
            pair: KeyPair::from_seed(Seed::new([seed; 32])),
            id,
        }
    }

    /// `minisign.pub` の 2 行目 (`update.trusted_keys` の値)。
    pub fn public_line(&self) -> String {
        let mut bin = b"Ed".to_vec();
        bin.extend_from_slice(&self.id);
        bin.extend_from_slice(&*self.pair.pk);
        b64(&bin)
    }

    /// 設定で足した鍵として読んだもの。
    pub fn trusted(&self) -> TrustedKey {
        TrustedKey::from_config(&self.public_line()).expect("a test key")
    }

    /// prehash の形 (`minisign -S` の既定) の署名。
    pub fn sign(&self, data: &[u8], trusted_comment: &str) -> Vec<u8> {
        let hash = Blake2b512::digest(data);
        self.minisig(*b"ED", &hash, trusted_comment)
    }

    /// prehash でない古い形 (legacy) の署名。
    pub fn sign_legacy(&self, data: &[u8], trusted_comment: &str) -> Vec<u8> {
        self.minisig(*b"Ed", data, trusted_comment)
    }

    fn minisig(&self, alg: [u8; 2], message: &[u8], trusted_comment: &str) -> Vec<u8> {
        let sig = self.pair.sk.sign(message, None);
        let mut global_msg = sig.to_vec();
        global_msg.extend_from_slice(trusted_comment.as_bytes());
        let global = self.pair.sk.sign(&global_msg, None);
        let mut line = alg.to_vec();
        line.extend_from_slice(&self.id);
        line.extend_from_slice(&*sig);
        format!(
            "untrusted comment: signature from minisign secret key\n{}\ntrusted comment: {trusted_comment}\n{}\n",
            b64(&line),
            b64(&*global)
        )
        .into_bytes()
    }
}

/// 試験の鍵 (ID `0101…`)。
pub fn key() -> TestKey {
    TestKey::new(1, [1; 8])
}

/// 埋め込んだ鍵に試験の鍵を足した集まり。
pub fn keyring(keys: &[&TestKey]) -> Keyring {
    let mut ring = Keyring::embedded();
    ring.extend(keys.iter().map(|k| k.trusted()));
    ring
}

/// 決まった trusted comment。
pub fn comment_for(version: &str, json: &[u8]) -> String {
    format!(
        "evorag {version} release.json sha256 {}",
        Sha256Digest::of(json)
    )
}

/// 試験の実行ファイルの中身。
pub fn exe_bytes(tag: &str) -> Vec<u8> {
    format!("MZ test executable {tag}\n")
        .repeat(100)
        .into_bytes()
}

pub fn zip_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        w.start_file(*name, options).expect("start a zip entry");
        w.write_all(data).expect("write a zip entry");
    }
    w.finish().expect("finish the zip").into_inner()
}

pub fn zip_with_symlink(name: &str, target: &str) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    w.add_symlink(name, target, options).expect("a zip symlink");
    w.finish().expect("finish the zip").into_inner()
}

/// tar の項目の種類。
pub enum TarEntry<'a> {
    File(&'a str, &'a [u8]),
    Symlink(&'a str, &'a str),
    Hardlink(&'a str, &'a str),
    Dir(&'a str),
}

pub fn tar_gz(entries: &[TarEntry<'_>]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut b = tar::Builder::new(gz);
    for e in entries {
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o755);
        match e {
            TarEntry::File(name, data) => {
                h.set_size(data.len() as u64);
                b.append_data(&mut h, name, *data).expect("append a file");
            }
            TarEntry::Symlink(name, target) | TarEntry::Hardlink(name, target) => {
                h.set_entry_type(if matches!(e, TarEntry::Symlink(..)) {
                    tar::EntryType::Symlink
                } else {
                    tar::EntryType::Link
                });
                h.set_size(0);
                b.append_link(&mut h, name, target).expect("append a link");
            }
            TarEntry::Dir(name) => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_size(0);
                b.append_data(&mut h, name, std::io::empty())
                    .expect("append a dir");
            }
        }
    }
    b.into_inner()
        .expect("finish the tar")
        .finish()
        .expect("finish the gzip")
}

/// 名前を書き換えずにそのまま書いた項目 1 つの tar.gz (`./` の付く名前など)。
pub fn tar_gz_raw_name(name: &str, data: &[u8]) -> Vec<u8> {
    let mut h = tar::Header::new_old();
    h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
    h.set_size(data.len() as u64);
    h.set_mode(0o755);
    h.set_entry_type(tar::EntryType::Regular);
    h.set_cksum();
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut b = tar::Builder::new(gz);
    b.append(&h, data).expect("append");
    b.into_inner()
        .expect("finish the tar")
        .finish()
        .expect("finish the gzip")
}

/// 項目の大きさだけを `size` と書き、中身を持たない tar.gz (大きすぎる項目の試験)。
pub fn tar_gz_declaring(name: &str, size: u64) -> Vec<u8> {
    let mut h = tar::Header::new_gnu();
    h.set_path(name).expect("path");
    h.set_size(size);
    h.set_mode(0o755);
    h.set_entry_type(tar::EntryType::Regular);
    h.set_cksum();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(h.as_bytes()).expect("header");
    gz.finish().expect("gzip")
}

/// 試験の `release.json` の値。
pub struct Release {
    pub json: serde_json::Value,
}

impl Release {
    /// 0.3.0 の、Windows の zip と Linux の tar.gz を持つリリース。
    pub fn new(win_zip: &[u8], linux_tgz: &[u8]) -> Self {
        let artifact = |target: &str, name: &str, bytes: &[u8], exe: &str, exe_bytes: &[u8]| {
            serde_json::json!({
                "target": target,
                "name": name,
                "sha256": Sha256Digest::of(bytes).to_string(),
                "size": bytes.len(),
                "exe": exe,
                "exe_sha256": Sha256Digest::of(exe_bytes).to_string(),
            })
        };
        Self {
            json: serde_json::json!({
                "schema": 1,
                "name": "evorag",
                "version": "0.3.0",
                "date": "2026-12-01",
                "upgrade_from": "0.2.0",
                "notes": "更新の要点",
                "artifacts": [
                    artifact(WINDOWS, WIN_ZIP, win_zip, WIN_EXE, &exe_bytes("win")),
                    artifact(LINUX, LINUX_TGZ, linux_tgz, LINUX_EXE, &exe_bytes("linux")),
                ],
                "db_format": "0.3",
                "db_min_write": "0.1",
                "llama_server_min_build": null,
            }),
        }
    }

    pub fn bytes(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(&self.json).expect("json")
    }

    /// 試験の鍵で、決まった trusted comment の署名を付けた `release.json` と `.minisig`。
    pub fn signed(&self, key: &TestKey) -> (Vec<u8>, Vec<u8>) {
        let json = self.bytes();
        let version = self.json["version"].as_str().unwrap_or("?").to_owned();
        let sig = key.sign(&json, &comment_for(&version, &json));
        (json, sig)
    }
}

/// 試験の配布物 (Windows の zip と Linux の tar.gz) と、それを持つリリース。
pub fn archives() -> (Vec<u8>, Vec<u8>) {
    let zip = zip_archive(&[
        ("evorag-0.3.0-x86_64-pc-windows-msvc/README.md", b"readme"),
        (WIN_EXE, &exe_bytes("win")),
    ]);
    let tgz = tar_gz(&[
        TarEntry::Dir("evorag-0.3.0-x86_64-unknown-linux-musl/"),
        TarEntry::File(
            "evorag-0.3.0-x86_64-unknown-linux-musl/README.md",
            b"readme",
        ),
        TarEntry::File(LINUX_EXE, &exe_bytes("linux")),
    ]);
    (zip, tgz)
}

pub fn v(s: &str) -> Version {
    s.parse().expect("a test version")
}
