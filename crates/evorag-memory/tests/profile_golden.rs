//! プロファイルの本文の golden fixture (04 §5.4)。
//!
//! 見本は `tests/fixtures/profile/`。前置きの文言・列・行の書式・並びが変わると落ちる。変えたらプロファイルの規則の版
//! (`profile::PROFILE_VERSION`、導出の指紋の成分) を上げる。作り直すときは
//! `EVORAG_BLESS=1 cargo nextest run -p evorag-memory --test profile_golden` を流す。例の値はすべて架空のもの。

use std::path::PathBuf;

use evorag_memory::b3::{Dictionary, Rules};
use evorag_memory::profile::{self, PROFILE_BUDGET, Profile};
use evorag_memory::{Anchor, AnchorKind, NORM_V1, UnitBody, UnitLayer, UnitState, UnitTense};
use evorag_types::{Id, PromptLanguage};

fn fixtures() -> PathBuf {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    dir.join("../../tests/fixtures/profile")
}

fn bless() -> bool {
    std::env::var_os("EVORAG_BLESS").is_some_and(|v| v == "1")
}

/// 見本 `name` と同じか確かめる (`EVORAG_BLESS=1` なら書く)。
fn check(name: &str, got: &str) {
    let path = fixtures().join(name);
    if bless() {
        std::fs::create_dir_all(fixtures()).unwrap();
        std::fs::write(&path, got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).expect("fixture; run with EVORAG_BLESS=1");
    assert!(
        want == got,
        "{name} changed (raise profile::PROFILE_VERSION for a format change and re-bless)\n--- want\n{want}\n--- got\n{got}"
    );
}

fn id(n: u8) -> Id {
    let mut b = [0u8; 16];
    b[15] = n;
    Id::from_bytes(b)
}

fn unit(n: u8, entity: &str, aspect: &str, value: &str, tense: UnitTense) -> (Id, UnitBody) {
    let body = UnitBody {
        layer: UnitLayer::World,
        entity: entity.into(),
        aspect: aspect.into(),
        free: false,
        value: value.into(),
        vk: format!("{n:02x}").repeat(32),
        norm: NORM_V1,
        when: None,
        tense,
        state: Some(UnitState::Live),
        against: None,
        by: None,
        state_at: None,
        private: false,
        anchors: vec![Anchor {
            turn: id(100 + n),
            rev: 0,
            start: 0,
            end: 1,
            s_start: 0,
            s_end: 1,
            kind: AnchorKind::Said,
            when: None,
        }],
    };
    (id(n), body)
}

fn render(units: &[(Id, UnitBody)], language: PromptLanguage) -> Profile {
    let rows = profile::rows(
        units.iter().map(|(i, b)| (*i, b)),
        Dictionary::bundled(),
        &Rules::bundled(),
        language,
    );
    profile::render(&rows, language, PROFILE_BUDGET, |s| s.chars().count()).expect("a profile")
}

#[test]
fn the_rendered_profile_matches_the_fixtures() {
    let ja = [
        unit(1, "self", "dietary", "くるみ", UnitTense::Current),
        unit(2, "self", "residence", "港南台", UnitTense::Current),
        unit(3, "self", "residence", "汐見坂", UnitTense::Planned),
        unit(4, "妻", "name", "花子", UnitTense::Current),
        unit(5, "self", "name", "ケン", UnitTense::Current),
        unit(6, "self", "dietary", "落花生", UnitTense::Current),
        unit(7, "self", "hobby", "将棋", UnitTense::Current),
    ];
    let p = render(&ja, PromptLanguage::Ja);
    assert_eq!(p.version, profile::version_of(&p.text));
    check("profile.ja.txt", &p.text);

    let en = [
        unit(1, "self", "dietary", "walnuts", UnitTense::Current),
        unit(2, "self", "residence", "Riverton", UnitTense::Current),
        unit(3, "self", "residence", "Lakeside", UnitTense::Planned),
        unit(4, "wife", "name", "Hana", UnitTense::Current),
        unit(5, "self", "name", "Ken", UnitTense::Current),
        unit(6, "self", "dietary", "peanuts", UnitTense::Current),
        unit(7, "self", "hobby", "chess", UnitTense::Current),
    ];
    let p = render(&en, PromptLanguage::En);
    check("profile.en.txt", &p.text);
}
