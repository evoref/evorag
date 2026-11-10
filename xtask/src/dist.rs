//! 配布物を作る (`cargo xtask dist`) と、配布物を確かめる (`cargo xtask verify-release`)。
//!
//! 仕様: 12 §5 (配布物・署名)、09 §9.2 (リリース手順)。
//!
//! - `dist`: release でビルドし (入れ方は `zip`、Windows は C ランタイムを静的にリンク)、実行ファイル・
//!   `README.md` (利用者向け)・`LICENSE`・`NOTICE`・`THIRD-PARTY-NOTICES.md` (`cargo about`)・
//!   `config.example.toml` を 1 つのフォルダにまとめて圧縮し、`SHA256SUMS` と `release.json` を書く。
//!   署名は人間が手元で行う (秘密鍵を CI に置かない)。署名の命令の例を最後に示す。
//! - `verify-release`: `SHA256SUMS` と各配布物の SHA-256 を照らし、`.minisig` があれば `minisign.pub`
//!   で署名を確かめる。`release.json` の署名の trusted comment には、版と `release.json` の SHA-256 が
//!   入っていること (12 §5.2)。

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// 配布する実行ファイルの大きさの上限 (NFR-204: 30 MB)。
const MAX_EXE_BYTES: u64 = 30 * 1024 * 1024;

fn sha256_hex(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let shown = format!("{cmd:?}");
    let status = cmd.status().map_err(|e| format!("{shown}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{shown}: {status}"))
    }
}

fn host_target() -> Result<String, String> {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_owned))
        .ok_or_else(|| "rustc -vV did not show the host".into())
}

pub fn workspace_version(root: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).map_err(|e| e.to_string())?;
    let v: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    v.get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "no workspace.package.version".into())
}

/// UNIX 時刻の日付 (`YYYY-MM-DD`、UTC)。
fn today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

struct DistArgs {
    target: String,
    out: PathBuf,
    skip_build: bool,
    /// リリースのタグ (`v0.1.0`)。`v<workspace の版>` と同じであることを確かめる (プレリリースは出さない、09 §9.1)。
    tag: Option<String>,
}

fn parse_args(root: &Path, args: &[String]) -> Result<DistArgs, String> {
    let mut target = None;
    let mut out = None;
    let mut skip_build = false;
    let mut tag = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--target" => target = it.next().cloned(),
            "--out" => out = it.next().map(PathBuf::from),
            "--skip-build" => skip_build = true,
            "--tag" => tag = it.next().cloned(),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(DistArgs {
        target: match target {
            Some(t) => t,
            None => host_target()?,
        },
        out: out.unwrap_or_else(|| root.join("target").join("dist")),
        skip_build,
        tag,
    })
}

/// `cargo xtask dist [--target T] [--out DIR] [--skip-build]`
pub fn dist(root: &Path, args: &[String]) -> Result<(), String> {
    let a = parse_args(root, args)?;
    let version = workspace_version(root)?;
    if let Some(tag) = &a.tag
        && !crate::verify_tag::tag_matches(tag, &version)
    {
        return Err(format!(
            "the tag {tag} must be exactly v{version} (pre-releases are not used, 09 §9.1)"
        ));
    }
    let windows = a.target.contains("windows");
    let exe = if windows { "evorag.exe" } else { "evorag" };

    if !a.skip_build {
        let mut flags = std::env::var("RUSTFLAGS").unwrap_or_default();
        if a.target.contains("msvc") {
            // C ランタイムを静的にリンクする (12 §5.1)。
            flags.push_str(" -C target-feature=+crt-static");
        }
        run(Command::new("cargo")
            .current_dir(root)
            .args(["build", "--release", "--locked", "-p", "evorag", "--target"])
            .arg(&a.target)
            .env("EVORAG_CHANNEL", "zip")
            .env("RUSTFLAGS", flags.trim()))?;
    }
    let built = root
        .join("target")
        .join(&a.target)
        .join("release")
        .join(exe);
    if !built.is_file() {
        return Err(format!("{} is missing (build first)", built.display()));
    }
    // 実行ファイルの大きさ (NFR-204、AC-NFR-204-1)。
    let size = std::fs::metadata(&built).map_err(|e| e.to_string())?.len();
    if size > MAX_EXE_BYTES {
        return Err(format!(
            "{} is {size} bytes, over the limit of {MAX_EXE_BYTES} (NFR-204)",
            built.display()
        ));
    }

    let name = format!("evorag-{version}-{}", a.target);
    let stage = a.out.join(&name);
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
    let copy = |from: PathBuf, to: &str| -> Result<(), String> {
        std::fs::copy(&from, stage.join(to))
            .map(|_| ())
            .map_err(|e| format!("{}: {e}", from.display()))
    };
    copy(built, exe)?;
    copy(root.join("dist").join("README.md"), "README.md")?;
    copy(
        root.join("dist").join("config.example.toml"),
        "config.example.toml",
    )?;
    copy(root.join("LICENSE"), "LICENSE")?;
    copy(root.join("NOTICE"), "NOTICE")?;
    // 依存のライセンスの表示 (12 §2)。
    run(Command::new("cargo")
        .current_dir(root)
        .args(["about", "generate", "--fail", "-m"])
        .arg(root.join("bins").join("evorag").join("Cargo.toml"))
        .arg("-o")
        .arg(stage.join("THIRD-PARTY-NOTICES.md"))
        .arg(root.join("about.hbs")))
    .map_err(|e| format!("{e}\n(install cargo-about: cargo install --locked cargo-about)"))?;

    let archive = if windows {
        let file = format!("{name}.zip");
        let _ = std::fs::remove_file(a.out.join(&file));
        // Windows の tar (bsdtar) は -a で拡張子から zip を選ぶ。
        run(Command::new("tar")
            .current_dir(&a.out)
            .args(["-a", "-c", "-f", &file, &name]))?;
        file
    } else {
        let file = format!("{name}.tar.gz");
        run(Command::new("tar")
            .current_dir(&a.out)
            .args(["-c", "-z", "-f", &file, &name]))?;
        file
    };
    let bytes = std::fs::read(a.out.join(&archive)).map_err(|e| e.to_string())?;
    let sum = sha256_hex(&bytes);

    let release = serde_json::json!({
        "name": "evorag",
        "version": version,
        "date": today(),
        "artifacts": [{
            "target": a.target,
            "name": archive,
            "sha256": sum,
            "size": bytes.len(),
        }],
        // 移行の情報 (03 §12)。
        "db_format": evorag_store::FORMAT_VERSION,
        // 対応する llama-server の build 番号の下限 (06 §10)。決めたらリリースノートと合わせて書く。
        "llama_server_min_build": serde_json::Value::Null,
    });
    let release_text = serde_json::to_string_pretty(&release).map_err(|e| e.to_string())? + "\n";
    std::fs::write(a.out.join("release.json"), &release_text).map_err(|e| e.to_string())?;
    let sums = format!("{sum}  {archive}\n");
    std::fs::write(a.out.join("SHA256SUMS"), &sums).map_err(|e| e.to_string())?;

    let release_sum = sha256_hex(release_text.as_bytes());
    println!("dist: {}", a.out.join(&archive).display());
    println!("  sha256 {sum}");
    println!("  release.json sha256 {release_sum}");
    println!("sign on your own machine (12 §5.2):");
    println!(
        "  minisign -S -s <secret key> -t \"evorag {version} release.json sha256 {release_sum}\" -m SHA256SUMS release.json"
    );
    println!("then: cargo xtask verify-release {}", a.out.display());
    Ok(())
}

/// `SHA256SUMS` の行 (`<hex>  <名前>`) を読む。
fn read_sums(text: &str) -> Result<Vec<(String, String)>, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (h, n) = l
                .split_once("  ")
                .or_else(|| l.split_once(" *"))
                .ok_or_else(|| format!("bad SHA256SUMS line: {l}"))?;
            if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("bad hash: {l}"));
            }
            if n.contains(['/', '\\']) || n.starts_with('.') {
                return Err(format!("bad file name: {n}"));
            }
            Ok((h.to_ascii_lowercase(), n.to_owned()))
        })
        .collect()
}

/// 署名を確かめ、trusted comment を返す。
fn verify_signature(pubkey: &str, data: &[u8], sig: &str) -> Result<String, String> {
    let pk = minisign_verify::PublicKey::decode(pubkey)
        .or_else(|_| minisign_verify::PublicKey::from_base64(pubkey.trim()))
        .map_err(|e| format!("public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(sig).map_err(|e| format!("signature: {e}"))?;
    pk.verify(data, &sig, false)
        .map_err(|e| format!("signature does not verify: {e}"))?;
    Ok(sig.trusted_comment().to_owned())
}

/// `cargo xtask verify-release <DIR> [--pubkey FILE] [--require-signatures]`
pub fn verify_release(root: &Path, args: &[String]) -> Result<(), String> {
    let mut dir = None;
    let mut pubkey = root.join("minisign.pub");
    let mut require = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pubkey" => {
                pubkey = it
                    .next()
                    .map(PathBuf::from)
                    .ok_or("--pubkey needs a file")?
            }
            "--require-signatures" => require = true,
            other if dir.is_none() => dir = Some(PathBuf::from(other)),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let dir = dir.ok_or("usage: cargo xtask verify-release <DIR>")?;
    let read = |n: &str| std::fs::read(dir.join(n)).map_err(|e| format!("{n}: {e}"));

    let sums_bytes = read("SHA256SUMS")?;
    let sums = read_sums(&String::from_utf8_lossy(&sums_bytes))?;
    if sums.is_empty() {
        return Err("SHA256SUMS lists nothing".into());
    }
    for (want, name) in &sums {
        let got = sha256_hex(&read(name)?);
        if &got != want {
            return Err(format!("{name}: sha256 {got}, SHA256SUMS says {want}"));
        }
        println!("ok  {name}");
    }
    let release_bytes = read("release.json")?;
    let release: serde_json::Value =
        serde_json::from_slice(&release_bytes).map_err(|e| format!("release.json: {e}"))?;
    let version = release["version"]
        .as_str()
        .ok_or("release.json has no version")?;
    for a in release["artifacts"]
        .as_array()
        .ok_or("release.json has no artifacts")?
    {
        let (name, sum) = (a["name"].as_str(), a["sha256"].as_str());
        if !sums
            .iter()
            .any(|(h, n)| Some(n.as_str()) == name && Some(h.as_str()) == sum)
        {
            return Err(format!("release.json and SHA256SUMS disagree on {name:?}"));
        }
    }

    let sig_sums = dir.join("SHA256SUMS.minisig");
    let sig_release = dir.join("release.json.minisig");
    if !sig_sums.is_file() || !sig_release.is_file() {
        if require {
            return Err("SHA256SUMS.minisig or release.json.minisig is missing".into());
        }
        println!("signatures: not yet signed (sign on your own machine, 12 §5.2)");
        return Ok(());
    }
    let key = std::fs::read_to_string(&pubkey).map_err(|e| format!("{}: {e}", pubkey.display()))?;
    let sig = |p: &Path| std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
    verify_signature(&key, &sums_bytes, &sig(&sig_sums)?)?;
    println!("ok  SHA256SUMS.minisig");
    let comment = verify_signature(&key, &release_bytes, &sig(&sig_release)?)?;
    let release_sum = sha256_hex(&release_bytes);
    if !comment.contains(version) || !comment.contains(&release_sum) {
        return Err(format!(
            "the trusted comment of release.json.minisig must contain the version {version} and sha256 {release_sum}: {comment}"
        ));
    }
    println!("ok  release.json.minisig ({comment})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_are_read_strictly() {
        let h = "a".repeat(64);
        assert_eq!(
            read_sums(&format!("{h}  evorag.zip\n")).unwrap(),
            [(h.clone(), "evorag.zip".to_owned())]
        );
        assert!(read_sums(&format!("{h}  ../x\n")).is_err());
        assert!(read_sums("abc  x\n").is_err());
    }

    /// minisign の署名の検証 (minisign-verify の試験と同じ鍵と署名)。
    #[test]
    fn signatures_are_checked() {
        let key = "untrusted comment: minisign public key\nRWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n";
        let sig = "untrusted comment: signature from minisign secret key
RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=
trusted comment: timestamp:1556193335\tfile:test
y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==";
        assert_eq!(
            verify_signature(key, b"test", sig).unwrap(),
            "timestamp:1556193335\tfile:test"
        );
        assert!(verify_signature(key, b"tampered", sig).is_err());
    }

    #[test]
    fn the_date_is_well_formed() {
        let d = today();
        assert_eq!(d.len(), 10);
        assert!(d.starts_with("20"));
    }
}
