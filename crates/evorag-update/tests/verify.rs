//! `release.json` の署名と中身の検証 (12 §5.2・§5.3・§7.2 の apply の 3・4、09 §3.6)。

mod common;

use common::{LINUX, Release, TestKey, WINDOWS, archives, comment_for, key, keyring, v};
use evorag_update::{
    Expectations, KeySource, Keyring, ReleaseDate, Severity, UpdateError, VersionSource,
    verify_manifest, verify_signature,
};

fn expect() -> Expectations {
    Expectations::new(v("0.2.1"), WINDOWS).with_tag(v("0.3.0"))
}

fn release() -> Release {
    let (zip, tgz) = archives();
    Release::new(&zip, &tgz)
}

/// 署名を付けて確かめる。
fn check(r: &Release, e: &Expectations) -> Result<evorag_update::VerifiedRelease, UpdateError> {
    let k = key();
    let (json, sig) = r.signed(&k);
    verify_manifest(&json, &sig, &keyring(&[&k]), e)
}

fn tampered(r: Result<evorag_update::VerifiedRelease, UpdateError>) -> UpdateError {
    let e = r.expect_err("must be refused");
    assert_eq!(e.severity(), Severity::Tampered, "{e}");
    assert_eq!(e.exit_code(), 5);
    e
}

fn unusable(r: Result<evorag_update::VerifiedRelease, UpdateError>) -> UpdateError {
    let e = r.expect_err("must be refused");
    assert_eq!(e.severity(), Severity::Unusable, "{e}");
    assert_eq!(e.exit_code(), 1);
    e
}

#[test]
fn a_signed_release_for_this_target_is_accepted() {
    let r = check(&release(), &expect().with_requested_version(v("0.3.0"))).expect("valid");
    assert_eq!(r.version(), v("0.3.0"));
    assert_eq!(r.manifest().date().to_string(), "2026-12-01");
    assert_eq!(r.manifest().notes(), Some("更新の要点"));
    assert_eq!(r.manifest().upgrade_from(), v("0.2.0"));
    assert_eq!(r.manifest().db_format(), Some("0.3"));
    assert_eq!(r.manifest().db_min_write(), Some("0.1"));
    assert_eq!(r.manifest().llama_server_min_build(), None);
    assert_eq!(r.artifact().target(), WINDOWS);
    assert_eq!(r.artifact().name(), common::WIN_ZIP);
    // 試験の鍵は設定で足した鍵なので、鍵の ID を示して警告できる (12 §5.2)。
    assert_eq!(r.signer().source(), KeySource::Added);
    assert_eq!(r.signer().key_id(), key().trusted().id());

    let linux = check(&release(), &Expectations::new(v("0.2.1"), LINUX)).expect("valid");
    assert_eq!(linux.artifact().name(), common::LINUX_TGZ);
}

/// verifies: AC-FR-804-1 (検証の核: 署名の合わない release.json を断る)、AC-NFR-505-2 (改ざんを疑うものは 5、
/// 署名の前に JSON として読まない)
#[test]
fn a_tampered_release_json_is_refused_before_it_is_read() {
    let k = key();
    let (mut json, sig) = release().signed(&k);
    let at = json.len() / 2;
    json[at] ^= 1;
    let e = tampered(verify_manifest(&json, &sig, &keyring(&[&k]), &expect()));
    assert!(matches!(e, UpdateError::BadSignature { .. }), "{e}");

    // JSON でないバイト列でも、署名の誤りとして断る (JSON として読む前に止まる)。
    let not_json = b"\xff\xfe not json at all";
    let sig = k.sign(b"something else", &comment_for("0.3.0", b"something else"));
    let e = tampered(verify_manifest(not_json, &sig, &keyring(&[&k]), &expect()));
    assert!(matches!(e, UpdateError::BadSignature { .. }), "{e}");

    // 署名は通るが trusted comment が違えば、JSON でなくても trusted comment の誤りで止まる。
    let sig = k.sign(not_json, "evorag 0.3.0 release.json sha256 00");
    let e = tampered(verify_manifest(not_json, &sig, &keyring(&[&k]), &expect()));
    assert!(matches!(e, UpdateError::TrustedCommentMismatch), "{e}");

    // 署名も trusted comment も通って初めて JSON として読み、読めなければ使えない (1)。
    let sig = k.sign(not_json, &comment_for("0.3.0", not_json));
    let e = unusable(verify_manifest(not_json, &sig, &keyring(&[&k]), &expect()));
    assert!(matches!(e, UpdateError::Malformed { .. }), "{e}");
}

/// verifies: AC-FR-804-1、AC-NFR-505-2 (鍵の違う署名・prehash でない署名)
#[test]
fn wrong_keys_and_legacy_signatures_are_refused() {
    let k = key();
    let r = release();
    let (json, sig) = r.signed(&k);

    // 同じ ID の鍵が無い (埋め込んだ鍵だけ)。
    let e = tampered(verify_manifest(
        &json,
        &sig,
        &Keyring::embedded(),
        &expect(),
    ));
    assert!(
        matches!(e, UpdateError::UnknownKey { key_id } if key_id == k.trusted().id()),
        "{e}"
    );

    // 同じ ID だが別の鍵。
    let impostor = TestKey::new(2, [1; 8]);
    let e = tampered(verify_manifest(
        &json,
        &sig,
        &keyring(&[&impostor]),
        &expect(),
    ));
    assert!(matches!(e, UpdateError::BadSignature { .. }), "{e}");

    // prehash でない署名は、正しい鍵でも断る。
    let legacy = k.sign_legacy(&json, &comment_for("0.3.0", &json));
    let e = tampered(verify_manifest(&json, &legacy, &keyring(&[&k]), &expect()));
    assert!(matches!(e, UpdateError::LegacySignature), "{e}");

    // 署名の形でない。
    for bad in [&b""[..], b"untrusted comment: x\n", b"\xff\xfe"] {
        let e = tampered(verify_manifest(&json, bad, &keyring(&[&k]), &expect()));
        assert!(matches!(e, UpdateError::SignatureMalformed), "{e}");
    }
}

#[test]
fn every_key_with_the_signature_key_id_is_tried() {
    let k = key();
    let impostor = TestKey::new(2, [1; 8]);
    let (json, sig) = release().signed(&k);
    // 同じ ID の鍵が 2 本あり、2 本目で通る。
    let ring = keyring(&[&impostor, &k]);
    let ok = verify_manifest(&json, &sig, &ring, &expect()).expect("the second key verifies");
    assert_eq!(ok.signer().source(), KeySource::Added);
}

/// verifies: AC-NFR-505-2 (trusted comment の文字列の不一致)
#[test]
fn the_trusted_comment_must_match_exactly() {
    let k = key();
    let json = release().bytes();
    let good = comment_for("0.3.0", &json);
    let other_sha = comment_for("0.3.0", b"other bytes");
    let variants = [
        format!("{good} "),
        format!(" {good}"),
        format!("{good}\tfile:release.json"),
        good.replace("evorag ", "evorag  "),
        good.replace("release.json", "Release.json"),
        good.replace("sha256 ", "sha256: "),
        good.to_uppercase(),
        other_sha,
        good.replace("0.3.0", "0.3"),
        format!("timestamp:1\t{good}"),
    ];
    for comment in &variants {
        let sig = k.sign(&json, comment);
        let e = tampered(verify_manifest(&json, &sig, &keyring(&[&k]), &expect()));
        assert!(
            matches!(e, UpdateError::TrustedCommentMismatch),
            "{comment:?}: {e}"
        );
    }
    // trusted comment の版が release.json の版と違う。
    let sig = k.sign(&json, &comment_for("0.3.1", &json));
    let e = tampered(verify_manifest(&json, &sig, &keyring(&[&k]), &expect()));
    assert!(
        matches!(
            e,
            UpdateError::VersionMismatch {
                source_of: VersionSource::TrustedComment,
                ..
            }
        ),
        "{e}"
    );
    // version が版として読めない・文字列でないときも、trusted comment の版との不一致 (改ざんを疑う)。
    for bad in [
        serde_json::json!("0.3"),
        serde_json::json!("v0.3.0"),
        serde_json::json!("0.3.0 "),
        serde_json::json!(3),
        serde_json::json!(null),
    ] {
        let mut r = release();
        r.json["version"] = bad.clone();
        let json = r.bytes();
        let sig = k.sign(&json, &comment_for("0.3.0", &json));
        let e = tampered(verify_manifest(&json, &sig, &keyring(&[&k]), &expect()));
        assert!(
            matches!(
                e,
                UpdateError::VersionMismatch {
                    source_of: VersionSource::TrustedComment,
                    ..
                }
            ),
            "{bad}: {e}"
        );
    }
    // trusted comment を書き換えると、全体の署名が通らない。
    let sig = k.sign(&json, &good);
    let forged = String::from_utf8(sig)
        .expect("utf-8")
        .replace("0.3.0", "0.3.9")
        .into_bytes();
    let e = tampered(verify_manifest(&json, &forged, &keyring(&[&k]), &expect()));
    assert!(matches!(e, UpdateError::BadSignature { .. }), "{e}");
}

/// verifies: AC-NFR-505-2 (`name`・版・タグ・指定した版の不一致)
#[test]
fn name_tag_and_requested_version_must_match() {
    let mut r = release();
    r.json["name"] = "evorag-pro".into();
    let e = tampered(check(&r, &expect()));
    assert!(matches!(e, UpdateError::NameMismatch), "{e}");

    let e = tampered(check(&release(), &expect().with_tag(v("0.3.1"))));
    assert!(
        matches!(
            e,
            UpdateError::VersionMismatch {
                source_of: VersionSource::Tag,
                ..
            }
        ),
        "{e}"
    );

    // name は、ほかの項目の形より先に比べる (形が壊れていても、改ざんを疑う理由で断る)。
    let mut r = release();
    r.json["name"] = "evorag-pro".into();
    r.json["date"] = "not a date".into();
    r.json.as_object_mut().expect("object").remove("artifacts");
    let e = tampered(check(&r, &expect()));
    assert!(matches!(e, UpdateError::NameMismatch), "{e}");
    let mut r = release();
    r.json.as_object_mut().expect("object").remove("name");
    let e = tampered(check(&r, &expect()));
    assert!(matches!(e, UpdateError::NameMismatch), "{e}");

    let e = tampered(check(
        &release(),
        &expect().with_requested_version(v("0.3.1")),
    ));
    assert!(
        matches!(
            e,
            UpdateError::VersionMismatch {
                source_of: VersionSource::Requested,
                ..
            }
        ),
        "{e}"
    );
}

/// verifies: AC-NFR-505-1 (検証の核: 今と同じ版・古い版を断る)
#[test]
fn the_same_or_an_older_version_is_refused() {
    for current in ["0.3.0", "0.3.1", "1.0.0"] {
        let e = unusable(check(&release(), &Expectations::new(v(current), WINDOWS)));
        assert!(matches!(e, UpdateError::NotNewer { .. }), "{current}: {e}");
    }
}

/// verifies: AC-NFR-505-2 (`date` が今の版のリリースの日付より古い)
#[test]
fn a_release_older_than_the_current_release_date_is_refused() {
    let date = |s: &str| s.parse::<ReleaseDate>().expect(s);
    let e = tampered(check(
        &release(),
        &expect().with_release_date(date("2026-12-02")),
    ));
    assert!(matches!(e, UpdateError::DateTooOld { .. }), "{e}");
    assert!(check(&release(), &expect().with_release_date(date("2026-12-01"))).is_ok());
    // 開発のビルド (日付を埋め込んでいない) では確かめない。
    let mut r = release();
    r.json["date"] = "2001-01-01".into();
    assert!(check(&r, &expect()).is_ok());
}

/// verifies: AC-NFR-505-2 (今の版が `upgrade_from` を下回るときは 1 で、先に入れる版を示せる)
#[test]
fn a_current_version_below_upgrade_from_is_told_which_version_to_apply_first() {
    let e = unusable(check(&release(), &Expectations::new(v("0.1.2"), WINDOWS)));
    let UpdateError::BelowUpgradeFrom {
        upgrade_from,
        release: manifest,
        signer,
    } = e
    else {
        panic!("{e}");
    };
    assert_eq!(upgrade_from, v("0.2.0"));
    assert_eq!(manifest.version(), v("0.3.0"));
    assert_eq!(manifest.notes(), Some("更新の要点"));
    assert_eq!(signer.source(), KeySource::Added);
    assert!(check(&release(), &Expectations::new(v("0.2.0"), WINDOWS)).is_ok());
}

/// verifies: AC-NFR-505-2 (`schema` を知らないときは 1)
#[test]
fn an_unknown_schema_is_refused_before_other_fields() {
    let mut cases = Vec::new();
    for schema in [
        serde_json::json!(2),
        serde_json::json!("1"),
        serde_json::json!(1.5),
        serde_json::json!(null),
    ] {
        let mut r = release();
        r.json["schema"] = schema;
        cases.push(r);
    }
    let mut missing = release();
    missing
        .json
        .as_object_mut()
        .expect("object")
        .remove("schema");
    cases.push(missing);
    // schema 2 では、ほかの項目が無くても schema の誤りとして断る。
    let mut v2 = release();
    v2.json = serde_json::json!({ "schema": 2, "name": "evorag", "version": "0.3.0" });
    cases.push(v2);
    for r in &cases {
        let e = unusable(check(r, &expect()));
        assert!(matches!(e, UpdateError::UnknownSchema), "{}: {e}", r.json);
    }
}

/// verifies: AC-NFR-505-2 (今の対象の配布物が 0 か 2 つのときは 1)
#[test]
fn exactly_one_archive_must_match_the_target() {
    let e = unusable(check(
        &release(),
        &Expectations::new(v("0.2.1"), "aarch64-unknown-linux-musl"),
    ));
    assert!(matches!(e, UpdateError::NoArtifact { .. }), "{e}");

    // 名前の末尾が対象の形式でない配布物 (インストーラーなど) は選ばない。
    let mut r = release();
    r.json["artifacts"][0]["name"] = "evorag-0.3.0-x86_64-pc-windows-msvc.msi".into();
    let e = unusable(check(&r, &expect()));
    assert!(matches!(e, UpdateError::NoArtifact { .. }), "{e}");
    let mut r = release();
    r.json["artifacts"][0]["name"] = "evorag-0.3.0-x86_64-pc-windows-msvc.tar.gz".into();
    let e = unusable(check(&r, &expect()));
    assert!(matches!(e, UpdateError::NoArtifact { .. }), "{e}");

    let mut r = release();
    let mut extra = r.json["artifacts"][0].clone();
    extra["name"] = "evorag-0.3.0-x86_64-pc-windows-msvc-full.zip".into();
    r.json["artifacts"]
        .as_array_mut()
        .expect("array")
        .push(extra);
    let e = unusable(check(&r, &expect()));
    assert!(
        matches!(e, UpdateError::AmbiguousArtifact { count: 2, .. }),
        "{e}"
    );

    // 別の形の配布物を足しても、条件に当たらない名前なら選べる。
    let mut r = release();
    let mut installer = r.json["artifacts"][0].clone();
    installer["name"] = "evorag-0.3.0-x86_64-pc-windows-msvc.msi".into();
    r.json["artifacts"]
        .as_array_mut()
        .expect("array")
        .push(installer);
    assert!(check(&r, &expect()).is_ok());
}

/// verifies: AC-NFR-505-2 (`name` に許さない文字や `..`、`exe` に `..`・絶対パス・`\`)
#[test]
fn malformed_fields_are_refused() {
    type Edit = Box<dyn Fn(&mut serde_json::Value)>;
    let edits: Vec<(&str, Edit)> = vec![
        (
            "name with a slash",
            Box::new(|j| j["artifacts"][0]["name"] = "a/evorag.zip".into()),
        ),
        (
            "name with ..",
            Box::new(|j| j["artifacts"][0]["name"] = "evorag..zip".into()),
        ),
        (
            "name with a space",
            Box::new(|j| j["artifacts"][0]["name"] = "evorag 1.zip".into()),
        ),
        (
            "exe with ..",
            Box::new(|j| j["artifacts"][0]["exe"] = "a/../evorag.exe".into()),
        ),
        (
            "absolute exe",
            Box::new(|j| j["artifacts"][0]["exe"] = "/evorag.exe".into()),
        ),
        (
            "drive exe",
            Box::new(|j| j["artifacts"][0]["exe"] = "C:/evorag.exe".into()),
        ),
        (
            "backslash exe",
            Box::new(|j| j["artifacts"][0]["exe"] = "a\\evorag.exe".into()),
        ),
        (
            "uppercase sha",
            Box::new(|j| {
                let s = j["artifacts"][0]["sha256"]
                    .as_str()
                    .expect("sha")
                    .to_uppercase();
                j["artifacts"][0]["sha256"] = s.into();
            }),
        ),
        (
            "short exe_sha256",
            Box::new(|j| j["artifacts"][0]["exe_sha256"] = "abc".into()),
        ),
        (
            "negative size",
            Box::new(|j| j["artifacts"][0]["size"] = (-1).into()),
        ),
        (
            "size as a string",
            Box::new(|j| j["artifacts"][0]["size"] = "10".into()),
        ),
        (
            "exe with a control character",
            Box::new(|j| {
                j["artifacts"][0]["exe"] =
                    "evorag-0.3.0-x86_64-pc-windows-msvc/evorag\u{1b}[2J.exe".into();
            }),
        ),
        (
            "exe with a bidi control",
            Box::new(|j| j["artifacts"][0]["exe"] = "evorag-0.3.0/\u{202E}exe.evorag".into()),
        ),
        (
            "target with a control character",
            Box::new(|j| j["artifacts"][0]["target"] = "x86_64-pc-windows-msvc\r".into()),
        ),
        (
            "missing exe",
            Box::new(|j| {
                j["artifacts"][0]
                    .as_object_mut()
                    .expect("object")
                    .remove("exe");
            }),
        ),
        (
            "missing upgrade_from",
            Box::new(|j| {
                j.as_object_mut().expect("object").remove("upgrade_from");
            }),
        ),
        (
            "missing date",
            Box::new(|j| {
                j.as_object_mut().expect("object").remove("date");
            }),
        ),
        (
            "missing artifacts",
            Box::new(|j| {
                j.as_object_mut().expect("object").remove("artifacts");
            }),
        ),
        ("bad date", Box::new(|j| j["date"] = "2026-13-01".into())),
        (
            "prerelease upgrade_from",
            Box::new(|j| j["upgrade_from"] = "0.2.0-rc.1".into()),
        ),
        ("notes as a number", Box::new(|j| j["notes"] = 1.into())),
        (
            "llama_server_min_build as a string",
            Box::new(|j| j["llama_server_min_build"] = "1".into()),
        ),
    ];
    for (what, edit) in &edits {
        let mut r = release();
        edit(&mut r.json);
        let e = unusable(check(&r, &expect()));
        assert!(matches!(e, UpdateError::Malformed { .. }), "{what}: {e}");
    }
    // 任意の項目は無くてよい。
    let mut r = release();
    for field in [
        "notes",
        "db_format",
        "db_min_write",
        "llama_server_min_build",
    ] {
        r.json.as_object_mut().expect("object").remove(field);
    }
    let ok = check(&r, &expect()).expect("optional fields may be missing");
    assert_eq!(ok.manifest().notes(), None);
    let mut r = release();
    r.json["llama_server_min_build"] = 4000.into();
    assert_eq!(
        check(&r, &expect())
            .expect("valid")
            .manifest()
            .llama_server_min_build(),
        Some(4000)
    );
}

#[test]
fn db_fields_are_sanitized_for_display() {
    let mut r = release();
    r.json["db_format"] = "0.4\u{1b}[2J\r\u{202E}".into();
    r.json["db_min_write"] = "x".repeat(200).into();
    let ok = check(&r, &expect()).expect("valid");
    assert_eq!(ok.manifest().db_format(), Some("0.4[2J"));
    assert_eq!(ok.manifest().db_min_write().map(str::len), Some(64));
}

#[test]
fn unknown_fields_are_ignored_at_the_top_and_in_artifacts() {
    let mut r = release();
    r.json["future_field"] = serde_json::json!({ "nested": [1, 2] });
    r.json["artifacts"][0]["signature_url"] = "https://example.invalid/x".into();
    r.json["artifacts"][1]["arch_level"] = 3.into();
    assert!(check(&r, &expect()).is_ok());
}

#[test]
fn notes_are_sanitized_and_cut_to_4_kib() {
    let mut r = release();
    r.json["notes"] = "a\r\nb\u{1b}[2J\u{202E}c\u{2067}d\u{7}\n".into();
    let ok = check(&r, &expect()).expect("valid");
    assert_eq!(ok.manifest().notes(), Some("a\nb[2Jcd\n"));

    let mut r = release();
    r.json["notes"] = "変".repeat(3000).into();
    let ok = check(&r, &expect()).expect("long notes are cut, not refused");
    let notes = ok.manifest().notes().expect("notes");
    assert!(notes.len() <= 4096 && notes.len() > 4090, "{}", notes.len());
    assert!(notes.chars().all(|c| c == '変'));
}

#[test]
fn a_real_minisign_signature_verifies() {
    // minisign で作った署名 (minisign-verify の試験の値): prehash の形で、鍵の ID が同じ。
    let key = evorag_update::TrustedKey::from_config(
        "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3",
    )
    .expect("key");
    let sig = b"untrusted comment: signature from minisign secret key
RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=
trusted comment: timestamp:1556193335\tfile:test
y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==";
    let mut ring = Keyring::new();
    ring.push(key);
    let ok = ring
        .verify(b"test", sig)
        .expect("a real minisign signature");
    assert_eq!(ok.trusted_comment(), "timestamp:1556193335\tfile:test");
    assert!(ring.verify(b"tampered", sig).is_err());
    // 署名は通るが、trusted comment が決まった形でない。
    let e = verify_signature(b"test", sig, &ring).expect_err("not an evorag release comment");
    assert!(matches!(e, UpdateError::TrustedCommentMismatch), "{e}");
}

/// 各リリースの `release.json` の見本 (`tests/fixtures/release/<版>/`) を今の読み手で読めること。
/// 知らない項目を足しても読めること (12 §5.3、09 §3.6)。
///
/// 見本はビルドのときに埋め込む (no-network の確認は別の利用者で試験を走らせ、
/// リポジトリのファイルを読めない)。見本を足したら [`RELEASE_FIXTURES`] にも足す。
#[test]
fn release_fixtures_stay_readable() {
    let mut seen = 0;
    for &(name, json) in RELEASE_FIXTURES {
        let path = std::path::Path::new(name);
        let mut value: serde_json::Value = serde_json::from_slice(json).expect("json");
        let version = value["version"].as_str().expect("version").to_owned();
        let k = key();
        let ring = keyring(&[&k]);
        let sig = k.sign(json, &comment_for(&version, json));
        let manifest = verify_signature(json, &sig, &ring)
            .and_then(|s| s.parse())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(!manifest.artifacts().is_empty(), "{}", path.display());

        value["added_later"] = true.into();
        for a in value["artifacts"].as_array_mut().expect("artifacts") {
            a["added_later"] = "x".into();
        }
        let json = serde_json::to_vec(&value).expect("json");
        let sig = k.sign(&json, &comment_for(&version, &json));
        let again = verify_signature(&json, &sig, &ring)
            .and_then(|s| s.parse())
            .unwrap_or_else(|e| panic!("{} with unknown fields: {e}", path.display()));
        assert_eq!(again, manifest);
        seen += 1;
    }
    assert!(seen > 0, "no fixture");
}

/// `tests/fixtures/release/` の見本 (フォルダの名前と `release.json` の中身)。
const RELEASE_FIXTURES: &[(&str, &[u8])] = &[
    (
        "schema-1-example",
        include_bytes!("../../../tests/fixtures/release/schema-1-example/release.json"),
    ),
    (
        "0.3.0",
        include_bytes!("../../../tests/fixtures/release/0.3.0/release.json"),
    ),
    (
        "0.4.0",
        include_bytes!("../../../tests/fixtures/release/0.4.0/release.json"),
    ),
];

/// `tests/fixtures/release/` のフォルダが、すべて [`RELEASE_FIXTURES`] にあること。
/// フォルダを読めないとき (別の利用者で走らせたとき) は確かめない。
#[test]
fn every_release_fixture_is_listed() {
    let dir = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/release"
    ));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let name = entry.expect("entry").file_name();
        let name = name.to_str().expect("a UTF-8 folder name");
        assert!(
            RELEASE_FIXTURES.iter().any(|&(n, _)| n == name),
            "add tests/fixtures/release/{name} to RELEASE_FIXTURES"
        );
    }
}
