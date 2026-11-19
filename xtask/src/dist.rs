//! 配布物を作る (`cargo xtask dist`) と、配布物を確かめる (`cargo xtask verify-release`)。
//!
//! 仕様: 12 §5 (配布物・署名・`release.json`)、12 §7.4、09 §9.2 (リリース手順)。
//!
//! - `dist`: release でビルドし (入れ方は `zip`、リリースの日付を `EVORAG_RELEASE_DATE` で埋め込む。
//!   Windows は C ランタイムを静的にリンク)、実行ファイル・`README.md` と `README.en.md` (利用者向け)・`LICENSE`・
//!   `NOTICE`・`THIRD-PARTY-NOTICES.md` (`cargo about`)・`config.example.toml` と `config.example.en.toml` を 1 つのフォルダに
//!   まとめて圧縮し、`SHA256SUMS` と `release.json` (schema 1) を書く。署名は人間が手元で行う
//!   (秘密鍵を CI に置かない)。署名の命令の例を最後に示す。
//! - `release.json` の `exe_sha256` は、できた配布物を evorag-update の読み手 (本体の更新と同じもの) で
//!   開いて求める。`--manifest-only` (別の機械で作った配布物をまとめる) も同じ。ビルドの段では、
//!   ビルドした実行ファイルと配布物の中の実行ファイルが同じことも確かめる。
//! - `verify-release`: `SHA256SUMS` と各配布物の SHA-256 を照らし、`release.json` を本体の更新と同じ
//!   読み手で読んで 12 §5.3 の規則を確かめ、配布物の中の実行ファイルが `exe_sha256` と合うことを確かめる。
//!   `.minisig` があれば、埋め込む鍵 (`minisign.pub`・`minisign.next.pub`) か `--pubkey` の鍵で署名を
//!   確かめ、`release.json` の trusted comment が決まった文字列とちょうど同じことを確かめる (12 §5.2)。

use std::path::{Path, PathBuf};
use std::process::Command;

use evorag_update::unverified_dist::{exe_in_archive, parse_unsigned};
use evorag_update::{
    ArchiveFormat, DISPLAY_MAX_BYTES, Keyring, MAX_ARCHIVE_BYTES, MAX_MANIFEST_BYTES, Manifest,
    ReleaseDate, Sha256Digest, TrustedKey, Version, sanitize_for_display, trusted_comment,
    verify_signature,
};

/// 実行ファイルと `THIRD-PARTY-NOTICES.md` のほかに配布物に入れるファイル (リポジトリの中の場所、配布物の中の
/// 名前、12 §5.1)。利用者向けの README と設定の例は、日本語 (`README.md`・`config.example.toml`) と英語
/// (`README.en.md`・`config.example.en.toml`)。
const BUNDLED: &[(&str, &str)] = &[
    ("dist/README.md", "README.md"),
    ("dist/README.en.md", "README.en.md"),
    ("dist/config.example.toml", "config.example.toml"),
    ("dist/config.example.en.toml", "config.example.en.toml"),
    ("LICENSE", "LICENSE"),
    ("NOTICE", "NOTICE"),
];

/// 配布する実行ファイルの大きさの上限 (NFR-204: 30 MB)。
const MAX_EXE_BYTES: u64 = 30 * 1024 * 1024;

/// 次の鍵 (`minisign.next.pub`) が無ければ配布物を作らない版 (12 §5.2: 次の鍵が置かれるまで v0.3.0 を
/// リリースしない)。
const NEXT_KEY_REQUIRED_FROM: Version = Version::new(0, 3, 0);

fn run(cmd: &mut Command) -> Result<(), String> {
    let shown = format!("{cmd:?}");
    let status = cmd.status().map_err(|e| format!("{shown}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{shown}: {status}"))
    }
}

fn output(cmd: &mut Command) -> Result<String, String> {
    let shown = format!("{cmd:?}");
    let out = cmd.output().map_err(|e| format!("{shown}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{shown}: {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
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

fn workspace_manifest(root: &Path) -> Result<toml::Table, String> {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).map_err(|e| e.to_string())?;
    text.parse().map_err(|e: toml::de::Error| e.to_string())
}

pub fn workspace_version(root: &Path) -> Result<String, String> {
    workspace_manifest(root)?
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "no workspace.package.version".into())
}

/// `[workspace.metadata.evorag]` の `upgrade_from` (12 §5.3)。
fn workspace_upgrade_from(root: &Path) -> Result<Version, String> {
    let value = workspace_manifest(root)?
        .get("workspace")
        .and_then(|w| w.get("metadata"))
        .and_then(|m| m.get("evorag"))
        .and_then(|e| e.get("upgrade_from"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or("no workspace.metadata.evorag.upgrade_from in Cargo.toml (12 §5.3)")?;
    value
        .parse()
        .map_err(|e| format!("workspace.metadata.evorag.upgrade_from `{value}`: {e}"))
}

/// リリースの日付: タグの指すコミット (= 作業ツリーの HEAD) の committer の日時を UTC の日付にしたもの
/// (12 §5.3)。対象ごとに別の機械で作っても同じ値になる。タグが手元にあれば、HEAD を指すことを確かめる。
fn release_date(root: &Path, tag: Option<&str>) -> Result<ReleaseDate, String> {
    let git = || {
        let mut c = Command::new("git");
        c.current_dir(root);
        c
    };
    let head = output(git().args(["rev-parse", "HEAD"]))?;
    if let Some(tag) = tag
        && let Ok(commit) = output(
            git()
                .args(["rev-parse", "--verify", "--quiet"])
                .arg(format!("{tag}^{{commit}}")),
        )
        && commit != head
    {
        return Err(format!(
            "the tag {tag} points to {commit}, but the work tree is at {head} (build from the tag)"
        ));
    }
    let seconds = output(git().args(["log", "-1", "--format=%ct", "HEAD"]))?;
    let seconds: u64 = seconds
        .parse()
        .map_err(|e| format!("git log --format=%ct gave `{seconds}`: {e}"))?;
    Ok(ReleaseDate::from_unix_time(seconds))
}

/// CHANGELOG のその版の節を、`notes` の平文 (UTF-8 で 4 KiB 以内) にする (12 §5.3)。節が無ければ `None`。
///
/// 見出しの `#`・強調の `` ` ``・リンクの URL を除き、行の境界で 4 KiB に収める (収まらなければ `…` で
/// 終える)。
fn changelog_notes(changelog: &str, version: &str) -> Option<String> {
    let head = format!("## [{version}]");
    let mut lines = changelog.lines().skip_while(|l| !l.starts_with(&head));
    lines.next()?;
    let mut plain: Vec<String> = Vec::new();
    for line in lines.take_while(|l| !l.starts_with("## ")) {
        let line = line.trim_end();
        let line = line.trim_start_matches('#').trim_start();
        let mut text = String::new();
        let mut rest = line.replace('`', "");
        // [文字](URL) → 文字
        while let Some(open) = rest.find('[') {
            let Some(close) = rest[open..].find("](").map(|i| open + i) else {
                break;
            };
            let Some(end) = rest[close..].find(')').map(|i| close + i) else {
                break;
            };
            text.push_str(&rest[..open]);
            text.push_str(&rest[open + 1..close]);
            rest = rest[end + 1..].to_owned();
        }
        text.push_str(&rest);
        if text.is_empty() && plain.last().is_none_or(String::is_empty) {
            continue;
        }
        plain.push(text);
    }
    while plain.last().is_some_and(String::is_empty) {
        plain.pop();
    }
    if plain.is_empty() {
        return None;
    }
    const MORE: &str = "…";
    let mut notes = String::new();
    for (i, line) in plain.iter().enumerate() {
        let room = if i + 1 == plain.len() {
            0
        } else {
            1 + MORE.len()
        };
        if notes.len() + line.len() + 1 + room > DISPLAY_MAX_BYTES {
            notes.push_str(MORE);
            break;
        }
        notes.push_str(line);
        notes.push('\n');
    }
    Some(sanitize_for_display(notes.trim_end(), DISPLAY_MAX_BYTES))
}

/// 配布物の中の実行ファイルの名前。
fn exe_name(target: &str) -> &'static str {
    if target.contains("windows") {
        "evorag.exe"
    } else {
        "evorag"
    }
}

/// 配布物の名前 (`evorag-<版>-<ターゲット>.zip` / `.tar.gz`) からターゲットを取る。
fn target_of(name: &str, version: &str) -> Option<String> {
    let rest = name.strip_prefix(&format!("evorag-{version}-"))?;
    let target = rest
        .strip_suffix(".zip")
        .or_else(|| rest.strip_suffix(".tar.gz"))?;
    (!target.is_empty() && !target.contains(['/', '\\'])).then(|| target.to_owned())
}

/// `release.json` に書く版ごとの値。
struct ManifestInput {
    version: Version,
    date: ReleaseDate,
    upgrade_from: Version,
    notes: Option<String>,
}

/// `out` にある、この版の配布物すべての `SHA256SUMS` と `release.json` を書く (ターゲットごとに別の機械で
/// 作った配布物を、1 つの目録にまとめる、12 §5)。`release.json` の SHA-256 を返す。
fn write_manifest(out: &Path, input: &ManifestInput) -> Result<Sha256Digest, String> {
    let version = input.version.to_string();
    let mut names: Vec<(String, String)> = std::fs::read_dir(out)
        .map_err(|e| format!("{}: {e}", out.display()))?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            target_of(&name, &version).map(|t| (name, t))
        })
        .collect();
    names.sort();
    if names.is_empty() {
        return Err(format!(
            "no archive for {version} in {} (evorag-{version}-<target>.zip or .tar.gz)",
            out.display()
        ));
    }
    let mut artifacts = Vec::new();
    let mut sums = String::new();
    for (name, target) in &names {
        let bytes = std::fs::read(out.join(name)).map_err(|e| e.to_string())?;
        check_archive_size(name, bytes.len())?;
        let sum = Sha256Digest::of(&bytes);
        sums.push_str(&format!("{sum}  {name}\n"));
        let exe = format!("evorag-{version}-{target}/{}", exe_name(target));
        let exe_bytes =
            exe_in_archive(name, &bytes, &exe).map_err(|e| format!("{name}: {exe}: {e}"))?;
        artifacts.push(serde_json::json!({
            "target": target,
            "name": name,
            "sha256": sum.to_string(),
            "size": bytes.len(),
            "exe": exe,
            "exe_sha256": Sha256Digest::of(&exe_bytes).to_string(),
        }));
    }
    let mut release = serde_json::json!({
        "schema": evorag_update::SCHEMA,
        "name": evorag_update::NAME,
        "version": version,
        "date": input.date.to_string(),
        "upgrade_from": input.upgrade_from.to_string(),
    });
    if let Some(notes) = &input.notes {
        release["notes"] = notes.clone().into();
    }
    release["artifacts"] = artifacts.into();
    // 移行の情報 (03 §3.1.1・§12)。
    release["db_format"] = evorag_store::FORMAT_VERSION.into();
    release["db_min_write"] = evorag_store::FORMAT_MIN.into();
    // 対応する llama-server の build 番号の下限 (06 §10)。決めたらリリースノートと合わせて書く。
    release["llama_server_min_build"] = serde_json::Value::Null;
    let release_text = serde_json::to_string_pretty(&release).map_err(|e| e.to_string())? + "\n";
    // 本体の更新と同じ読み手で読めて、リリースの規則に合うことを、書く前に確かめる。
    check_manifest_size(release_text.len())?;
    let manifest =
        parse_unsigned(release_text.as_bytes()).map_err(|e| format!("release.json: {e}"))?;
    check_rules(&manifest)?;
    std::fs::write(out.join("release.json"), &release_text).map_err(|e| e.to_string())?;
    std::fs::write(out.join("SHA256SUMS"), &sums).map_err(|e| e.to_string())?;
    for (name, _) in &names {
        println!("dist: {}", out.join(name).display());
    }
    Ok(Sha256Digest::of(release_text.as_bytes()))
}

/// `release.json` は、本体の更新が取得する上限 (64 KiB、12 §7.5) 以内。
fn check_manifest_size(len: usize) -> Result<(), String> {
    if len > MAX_MANIFEST_BYTES {
        return Err(format!(
            "release.json is {len} bytes, over the limit {MAX_MANIFEST_BYTES} of evorag update (12 §7.5)"
        ));
    }
    Ok(())
}

/// 配布物は、本体の更新が取得する上限 (64 MiB、12 §7.5) 以内。
fn check_archive_size(name: &str, len: usize) -> Result<(), String> {
    if u64::try_from(len).map_or(true, |l| l > MAX_ARCHIVE_BYTES) {
        return Err(format!(
            "{name} is {len} bytes, over the limit {MAX_ARCHIVE_BYTES} of evorag update (12 §7.5)"
        ));
    }
    Ok(())
}

/// 作る側の規則 (12 §5.3、09 §9.2 の手順 6): 配布物が 1 つ以上あること、`upgrade_from` の決め方、
/// 配布物の名前と `exe` と大きさ、対象ごとに配布物を 1 つに選べること。
fn check_rules(m: &Manifest) -> Result<(), String> {
    if m.artifacts().is_empty() {
        return Err("release.json lists no archive".into());
    }
    let version = m.version();
    let from = m.upgrade_from();
    if from >= version {
        return Err(format!(
            "upgrade_from {from} must be lower than the version {version} (12 §5.3)"
        ));
    }
    // v0.1〜v0.4 は、直前の機能リリースの `.0` 以上 (移行の関数を直前の版の分だけ持つ、03 §12.1)。
    if version.major() == 0 && (1..=4).contains(&version.minor()) {
        let lowest = Version::new(0, version.minor() - 1, 0);
        if from < lowest {
            return Err(format!(
                "upgrade_from {from} must be {lowest} or later for {version} (12 §5.3); \
                 fix workspace.metadata.evorag.upgrade_from in Cargo.toml"
            ));
        }
    }
    for a in m.artifacts() {
        let target = a.target();
        let format = ArchiveFormat::for_target(target)
            .ok_or_else(|| format!("{}: no archive format for the target {target}", a.name()))?;
        let name = format!("evorag-{version}-{target}{}", format.suffix());
        if a.name() != name {
            return Err(format!(
                "{}: the archive for {target} must be named {name}",
                a.name()
            ));
        }
        let exe = format!("evorag-{version}-{target}/{}", exe_name(target));
        if a.exe() != exe {
            return Err(format!("{}: exe must be {exe}, not {}", a.name(), a.exe()));
        }
        if a.size() > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "{}: size {} is over the limit {MAX_ARCHIVE_BYTES} of evorag update (12 §7.5)",
                a.name(),
                a.size()
            ));
        }
        m.artifact_for(target)
            .map_err(|e| format!("{}: {e}", a.name()))?;
    }
    Ok(())
}

struct DistArgs {
    target: String,
    out: PathBuf,
    skip_build: bool,
    /// 配布物を作らず、`out` にある配布物の目録だけを書き直す (別の機械で作った配布物をまとめる)。
    manifest_only: bool,
    /// リリースのタグ (`v0.1.0`)。`v<workspace の版>` と同じであることを確かめる (プレリリースは出さない、09 §9.1)。
    tag: Option<String>,
}

fn parse_args(root: &Path, args: &[String]) -> Result<DistArgs, String> {
    let mut target = None;
    let mut out = None;
    let mut skip_build = false;
    let mut manifest_only = false;
    let mut tag = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--target" => target = it.next().cloned(),
            "--out" => out = it.next().map(PathBuf::from),
            "--skip-build" => skip_build = true,
            "--manifest-only" => manifest_only = true,
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
        manifest_only,
        tag,
    })
}

fn print_signing(out: &Path, version: Version, release_sum: &Sha256Digest) {
    let comment = trusted_comment(version, release_sum);
    println!("  release.json sha256 {release_sum}");
    println!("sign on your own machine (12 §5.2; the trusted comment must be exactly this):");
    println!("  minisign -S -s <secret key> -t \"{comment}\" -m SHA256SUMS release.json");
    println!("then: cargo xtask verify-release {}", out.display());
}

/// `cargo xtask dist [--target T] [--out DIR] [--skip-build] [--tag vX.Y.Z] [--manifest-only]`
pub fn dist(root: &Path, args: &[String]) -> Result<(), String> {
    let a = parse_args(root, args)?;
    if a.skip_build && a.tag.is_some() {
        // 作り置きの実行ファイルに埋め込んだ EVORAG_RELEASE_DATE が release.json の date と同じとは
        // 限らない。リリースでは必ずビルドする (変わっていなければ cargo は作り直さない)。
        // ファイルを読む前に断る。
        return Err(
            "--skip-build cannot be used with --tag: a release always builds, so that the embedded \
             EVORAG_RELEASE_DATE is the date in release.json (12 §7.4)"
                .into(),
        );
    }
    let version_text = workspace_version(root)?;
    if let Some(tag) = &a.tag
        && !crate::verify_tag::tag_matches(tag, &version_text)
    {
        return Err(format!(
            "the tag {tag} must be exactly v{version_text} (pre-releases are not used, 09 §9.1)"
        ));
    }
    let version: Version = version_text
        .parse()
        .map_err(|e| format!("workspace version {version_text}: {e}"))?;
    let date = release_date(root, a.tag.as_deref())?;
    let changelog = std::fs::read_to_string(root.join("CHANGELOG.md"))
        .map_err(|e| format!("CHANGELOG.md: {e}"))?;
    let input = ManifestInput {
        version,
        date,
        upgrade_from: workspace_upgrade_from(root)?,
        notes: changelog_notes(&changelog, &version_text),
    };
    println!(
        "release date {date} (EVORAG_RELEASE_DATE), upgrade_from {}",
        input.upgrade_from
    );
    if a.manifest_only {
        let release_sum = write_manifest(&a.out, &input)?;
        print_signing(&a.out, version, &release_sum);
        return Ok(());
    }
    if version >= NEXT_KEY_REQUIRED_FROM && !root.join("minisign.next.pub").is_file() {
        return Err(format!(
            "minisign.next.pub is missing: from {NEXT_KEY_REQUIRED_FROM} the binary embeds the next \
             release key too (12 §5.2); a maintainer creates it offline"
        ));
    }
    let windows = a.target.contains("windows");
    let exe = exe_name(&a.target);

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
            .env("EVORAG_RELEASE_DATE", date.to_string())
            .env("RUSTFLAGS", flags.trim()))?;
    }
    // cargo と同じく CARGO_TARGET_DIR があればそこに作る (相対なら workspace の根から)。
    let target_dir =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), |d| root.join(d));
    let built = target_dir.join(&a.target).join("release").join(exe);
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
    let built_sum = Sha256Digest::of(&std::fs::read(&built).map_err(|e| e.to_string())?);

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
    for (from, to) in BUNDLED {
        copy(root.join(from), to)?;
    }
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
    println!("  sha256 {} ({archive})", Sha256Digest::of(&bytes));
    // 配布物の中の実行ファイルが、ビルドしたものと同じ (本体の更新と同じ読み手で開く)。
    let exe_path = format!("{name}/{exe}");
    let packed = exe_in_archive(&archive, &bytes, &exe_path)
        .map_err(|e| format!("{archive}: {exe_path}: {e}"))?;
    if Sha256Digest::of(&packed) != built_sum {
        return Err(format!(
            "{archive}: {exe_path} differs from the built executable"
        ));
    }
    let release_sum = write_manifest(&a.out, &input)?;
    print_signing(&a.out, version, &release_sum);
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

/// `cargo xtask verify-release <DIR> [--pubkey FILE] [--require-signatures]`
pub fn verify_release(_root: &Path, args: &[String]) -> Result<(), String> {
    let mut dir = None;
    let mut pubkey = None;
    let mut require = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pubkey" => {
                pubkey = Some(
                    it.next()
                        .map(PathBuf::from)
                        .ok_or("--pubkey needs a file")?,
                );
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
        let got = Sha256Digest::of(&read(name)?).to_string();
        if &got != want {
            return Err(format!("{name}: sha256 {got}, SHA256SUMS says {want}"));
        }
        println!("ok  {name}");
    }

    // release.json を本体の更新と同じ読み手で読む (schema 1 の規則、12 §5.3)。
    let release_bytes = read("release.json")?;
    check_manifest_size(release_bytes.len())?;
    let manifest = parse_unsigned(&release_bytes).map_err(|e| format!("release.json: {e}"))?;
    let raw: serde_json::Value =
        serde_json::from_slice(&release_bytes).map_err(|e| format!("release.json: {e}"))?;
    if let Some(notes) = raw.get("notes").and_then(serde_json::Value::as_str)
        && sanitize_for_display(notes, DISPLAY_MAX_BYTES) != notes
    {
        return Err(format!(
            "release.json: notes must be plain text of at most {DISPLAY_MAX_BYTES} bytes without control characters (12 §5.3)"
        ));
    }
    check_rules(&manifest)?;
    // SHA256SUMS の配布物 (.zip・.tar.gz) は、どれも release.json にちょうど 1 回だけ載る。
    for (_, name) in &sums {
        if ArchiveFormat::from_name(name).is_none() {
            continue;
        }
        let listed = manifest
            .artifacts()
            .iter()
            .filter(|a| a.name() == name)
            .count();
        if listed != 1 {
            return Err(format!(
                "{name} is in SHA256SUMS but listed {listed} times in release.json (must be once)"
            ));
        }
    }
    for a in manifest.artifacts() {
        let name = a.name();
        if !sums
            .iter()
            .any(|(h, n)| n == name && *h == a.sha256().to_string())
        {
            return Err(format!("release.json and SHA256SUMS disagree on {name}"));
        }
        let bytes = read(name)?;
        check_archive_size(name, bytes.len())?;
        if u64::try_from(bytes.len()).ok() != Some(a.size()) {
            return Err(format!(
                "{name}: {} bytes, release.json says {}",
                bytes.len(),
                a.size()
            ));
        }
        let exe = exe_in_archive(name, &bytes, a.exe())
            .map_err(|e| format!("{name}: {}: {e}", a.exe()))?;
        if Sha256Digest::of(&exe) != a.exe_sha256() {
            return Err(format!("{name}: {} does not match exe_sha256", a.exe()));
        }
        println!("ok  {name}: {} (exe_sha256)", a.exe());
    }
    println!(
        "ok  release.json: {} {} upgrade_from {}",
        manifest.version(),
        manifest.date(),
        manifest.upgrade_from()
    );

    let sig_sums = dir.join("SHA256SUMS.minisig");
    let sig_release = dir.join("release.json.minisig");
    if !sig_sums.is_file() || !sig_release.is_file() {
        if require {
            return Err("SHA256SUMS.minisig or release.json.minisig is missing".into());
        }
        println!("signatures: not yet signed (sign on your own machine, 12 §5.2)");
        return Ok(());
    }
    let keys = match &pubkey {
        Some(path) => {
            let text =
                std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let key = TrustedKey::from_minisign_pub(&text)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let mut ring = Keyring::new();
            ring.push(key);
            ring
        }
        // 本体に埋め込む鍵 (今の鍵と次の鍵)。
        None => Keyring::embedded(),
    };
    let read_sig = |p: &Path| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    let ok = keys
        .verify(&sums_bytes, &read_sig(&sig_sums)?)
        .map_err(|e| format!("SHA256SUMS.minisig: {e}"))?;
    println!("ok  SHA256SUMS.minisig (key {})", ok.signer().key_id());
    let sig = read_sig(&sig_release)?;
    let signed = verify_signature(&release_bytes, &sig, &keys)
        .map_err(|e| format!("release.json.minisig: {e}"))?;
    signed.parse().map_err(|e| format!("release.json: {e}"))?;
    println!(
        "ok  release.json.minisig (key {}, trusted comment {})",
        signed.signer().key_id(),
        trusted_comment(
            signed.trusted_comment_version(),
            &Sha256Digest::of(&release_bytes)
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use super::*;

    /// 利用者向けの README は日本語と英語の両方を同梱し、英語版は日本語版と同じ節の組み立て (見出しの深さの並び)
    /// を持つ (12 §5.1、M3.6)。
    #[test]
    fn both_user_readmes_are_bundled_with_the_same_outline() {
        let names: Vec<&str> = BUNDLED.iter().map(|(_, to)| *to).collect();
        for want in [
            "README.md",
            "README.en.md",
            "config.example.toml",
            "config.example.en.toml",
            "LICENSE",
            "NOTICE",
        ] {
            assert!(names.contains(&want), "{want} is bundled: {names:?}");
        }
        let outline = |text: &str| -> Vec<usize> {
            let mut fenced = false;
            text.lines()
                .filter(|l| {
                    if l.starts_with("```") {
                        fenced = !fenced;
                    }
                    !fenced && l.starts_with('#')
                })
                .map(|l| l.chars().take_while(|c| *c == '#').count())
                .collect()
        };
        let ja = outline(include_str!("../../dist/README.md"));
        let en = outline(include_str!("../../dist/README.en.md"));
        assert!(!ja.is_empty());
        assert_eq!(en, ja, "the English README follows the Japanese outline");
    }

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

    fn zip_with(name: &str, data: &[u8]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file(name, options).unwrap();
        w.write_all(data).unwrap();
        w.finish().unwrap().into_inner()
    }

    fn tar_gz_with(name: &str, data: &[u8]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut b = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o755);
        b.append_data(&mut h, name, data).unwrap();
        b.into_inner().unwrap().finish().unwrap()
    }

    fn input(version: &str, upgrade_from: &str) -> ManifestInput {
        ManifestInput {
            version: version.parse().unwrap(),
            date: "2026-11-12".parse().unwrap(),
            upgrade_from: upgrade_from.parse().unwrap(),
            notes: Some("要点".into()),
        }
    }

    #[test]
    fn the_manifest_lists_every_archive_of_the_version() {
        assert_eq!(
            target_of("evorag-0.2.0-x86_64-unknown-linux-musl.tar.gz", "0.2.0").as_deref(),
            Some("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            target_of("evorag-0.2.0-x86_64-pc-windows-msvc.zip", "0.2.0").as_deref(),
            Some("x86_64-pc-windows-msvc")
        );
        assert_eq!(
            target_of("evorag-0.1.2-x86_64-pc-windows-msvc.zip", "0.2.0"),
            None
        );
        assert_eq!(target_of("SHA256SUMS", "0.2.0"), None);
        let dir = std::env::temp_dir().join(format!("evorag-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let win = "evorag-0.2.0-x86_64-pc-windows-msvc";
        let linux = "evorag-0.2.0-x86_64-unknown-linux-musl";
        std::fs::write(
            dir.join(format!("{win}.zip")),
            zip_with(&format!("{win}/evorag.exe"), b"win exe"),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{linux}.tar.gz")),
            tar_gz_with(&format!("{linux}/evorag"), b"linux exe"),
        )
        .unwrap();
        std::fs::write(dir.join("evorag-0.1.2-x86_64-pc-windows-msvc.zip"), b"old").unwrap();
        write_manifest(&dir, &input("0.2.0", "0.1.0")).unwrap();
        let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap();
        assert_eq!(sums.lines().count(), 2, "{sums}");
        let bytes = std::fs::read(dir.join("release.json")).unwrap();
        let release: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(release["schema"], 1);
        assert_eq!(release["date"], "2026-11-12");
        assert_eq!(release["upgrade_from"], "0.1.0");
        assert_eq!(release["notes"], "要点");
        assert_eq!(release["db_min_write"], evorag_store::FORMAT_MIN);
        let targets: Vec<&str> = release["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["target"].as_str().unwrap())
            .collect();
        assert_eq!(
            targets,
            ["x86_64-pc-windows-msvc", "x86_64-unknown-linux-musl"]
        );
        assert_eq!(
            release["artifacts"][1]["exe_sha256"],
            Sha256Digest::of(b"linux exe").to_string()
        );
        verify_release(Path::new("."), &[dir.display().to_string()]).unwrap();

        // SHA256SUMS に載る配布物が release.json に無ければ断る。
        let release_text = std::fs::read_to_string(dir.join("release.json")).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&release_text).unwrap();
        value["artifacts"].as_array_mut().unwrap().remove(1);
        std::fs::write(
            dir.join("release.json"),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
        let e = verify_release(Path::new("."), &[dir.display().to_string()]).unwrap_err();
        assert!(e.contains("listed 0 times"), "{e}");
        // 配布物の無い release.json も断る。
        value["artifacts"] = serde_json::json!([]);
        std::fs::write(
            dir.join("release.json"),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
        assert!(verify_release(Path::new("."), &[dir.display().to_string()]).is_err());
        std::fs::write(dir.join("release.json"), &release_text).unwrap();
        verify_release(Path::new("."), &[dir.display().to_string()]).unwrap();

        // 配布物の中の実行ファイルが release.json と合わなければ、verify-release が断る。
        std::fs::write(
            dir.join(format!("{linux}.tar.gz")),
            tar_gz_with(&format!("{linux}/evorag"), b"other exe"),
        )
        .unwrap();
        assert!(verify_release(Path::new("."), &[dir.display().to_string()]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manifest_without_the_exe_is_not_written() {
        let dir =
            std::env::temp_dir().join(format!("evorag-manifest-noexe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let win = "evorag-0.2.0-x86_64-pc-windows-msvc";
        std::fs::write(
            dir.join(format!("{win}.zip")),
            zip_with(&format!("{win}/README.md"), b"no exe"),
        )
        .unwrap();
        assert!(write_manifest(&dir, &input("0.2.0", "0.1.0")).is_err());
        assert!(!dir.join("release.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    const WIN: &str = "x86_64-pc-windows-msvc";

    /// 規則を確かめるための release.json (対象ごとに配布物を 1 つ)。
    fn manifest_with(version: &str, from: &str, targets: &[&str]) -> Manifest {
        let artifacts: Vec<serde_json::Value> = targets
            .iter()
            .map(|t| {
                let suffix = ArchiveFormat::for_target(t).unwrap().suffix();
                serde_json::json!({
                    "target": t,
                    "name": format!("evorag-{version}-{t}{suffix}"),
                    "sha256": "0".repeat(64),
                    "size": 1,
                    "exe": format!("evorag-{version}-{t}/{}", exe_name(t)),
                    "exe_sha256": "0".repeat(64),
                })
            })
            .collect();
        let json = serde_json::json!({
            "schema": 1, "name": "evorag", "version": version, "date": "2026-11-12",
            "upgrade_from": from, "artifacts": artifacts,
        });
        parse_unsigned(&serde_json::to_vec(&json).unwrap()).unwrap()
    }

    #[test]
    fn a_release_lists_at_least_one_archive_within_the_size_limit() {
        assert!(check_rules(&manifest_with("0.3.0", "0.2.0", &[])).is_err());
        assert!(check_rules(&manifest_with("0.3.0", "0.2.0", &[WIN])).is_ok());
        assert!(check_manifest_size(MAX_MANIFEST_BYTES).is_ok());
        assert!(check_manifest_size(MAX_MANIFEST_BYTES + 1).is_err());
        let max = usize::try_from(MAX_ARCHIVE_BYTES).unwrap();
        assert!(check_archive_size("a.zip", max).is_ok());
        assert!(check_archive_size("a.zip", max + 1).is_err());
    }

    #[test]
    fn a_release_build_cannot_skip_the_build() {
        // ファイルを読まずに断ること (no-network の確認は別の利用者で試験を走らせ、
        // リポジトリのファイルを読めない)。
        let root = Path::new("no-such-workspace");
        let args: Vec<String> = [
            "--skip-build",
            "--tag",
            "v0.0.0",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--out",
            "unused",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let e = dist(root, &args).unwrap_err();
        assert!(e.contains("--skip-build"), "{e}");
    }

    #[test]
    fn upgrade_from_follows_the_rule_of_the_version() {
        let manifest = |version: &str, from: &str| manifest_with(version, from, &[WIN]);
        assert!(check_rules(&manifest("0.3.0", "0.2.0")).is_ok());
        assert!(check_rules(&manifest("0.3.2", "0.2.1")).is_ok());
        assert!(check_rules(&manifest("0.3.0", "0.1.0")).is_err());
        assert!(check_rules(&manifest("0.3.0", "0.3.0")).is_err());
        assert!(check_rules(&manifest("0.3.0", "0.4.0")).is_err());
        assert!(check_rules(&manifest("0.6.0", "0.4.0")).is_ok());
    }

    #[test]
    fn notes_are_the_changelog_section_as_plain_text() {
        let changelog = "# Changelog\n\n## [Unreleased]\n\n- 未公開\n\n## [0.2.0] - 2026-11-12\n\n\
                         要点の段落 ([手順](docs/x.md))。\n\n### 追加\n\n- `/style` を足した。\n\n\
                         ## [0.1.2] - 2026-11-11\n\n- 古い\n";
        assert_eq!(
            changelog_notes(changelog, "0.2.0").as_deref(),
            Some("要点の段落 (手順)。\n\n追加\n\n- /style を足した。")
        );
        assert_eq!(changelog_notes(changelog, "0.9.0"), None);

        let long = format!(
            "## [0.2.0]\n\n{}",
            "- とても長い行の変更点です。\n".repeat(400)
        );
        let notes = changelog_notes(&long, "0.2.0").unwrap();
        assert!(notes.len() <= DISPLAY_MAX_BYTES, "{}", notes.len());
        assert!(notes.ends_with("…"));
        assert_eq!(sanitize_for_display(&notes, DISPLAY_MAX_BYTES), notes);
    }

    #[test]
    fn the_real_changelog_gives_notes_within_4_kib() {
        // ビルドのときに埋め込む (no-network の確認は別の利用者で試験を走らせる)。
        let text = include_str!("../../CHANGELOG.md");
        let notes = changelog_notes(text, "0.2.0").expect("the 0.2.0 section");
        assert!(notes.len() <= DISPLAY_MAX_BYTES);
        assert!(notes.starts_with("埋め込みとリランカー"), "{notes}");
    }
}
