//! B2 のプロンプトと出力の型の golden fixture (04 §5.2・§5.2.1)。
//!
//! 見本は `tests/fixtures/b2/`。プロンプト (システムと利用者のメッセージ) か JSON Schema が変わると落ちる。プロンプトを
//! 変えたら変種の版 (`b2::VARIANT`) を上げ、出力の型を変えたら `LEDGER_SCHEMA` も上げる (04 §5.2.1 の「プロンプトを
//! 変えるとき」)。作り直すときは `EVORAG_BLESS=1 cargo nextest run -p evorag-memory --test b2_golden` を流す。
//! 例の会話はすべて架空のもの。

use std::ops::Range;
use std::path::PathBuf;

use evorag_memory::b2::{Call, Candidate, Input, Layout, Pair, Text, VARIANT};
use evorag_types::{Id, PromptLanguage};

fn fixtures() -> PathBuf {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    dir.join("../../tests/fixtures/b2")
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
        "{name} changed (raise b2::VARIANT for a prompt change and re-bless)\n--- want\n{want}\n--- got\n{got}"
    );
}

fn id(n: u8) -> Id {
    let mut b = [0u8; 16];
    b[15] = n;
    Id::from_bytes(b)
}

fn range_of(hay: &str, needle: &str) -> Range<usize> {
    let s = hay.find(needle).expect("needle");
    s..s + needle.len()
}

/// 例の区切り (3 往復: 言い切り・相槌・訂正) を 1 回の呼び出しにする。
fn call(
    language: PromptLanguage,
    texts: [&str; 5],
    hide: &str,
    old: &str,
    new: &str,
    pattern: &str,
) -> Call {
    let [u1, r1, u2, r2, u3] = texts;
    let hidden = [range_of(u1, hide)];
    let reply_hidden = [range_of(r1, hide)];
    let pairs = [
        Pair {
            utterance: Text {
                turn: id(1),
                rev: 0,
                text: u1,
                hidden: &hidden,
            },
            reply: Some(Text {
                turn: id(2),
                rev: 0,
                text: r1,
                hidden: &reply_hidden,
            }),
        },
        Pair {
            utterance: Text {
                turn: id(3),
                rev: 0,
                text: u2,
                hidden: &[],
            },
            reply: Some(Text {
                turn: id(4),
                rev: 0,
                text: r2,
                hidden: &[],
            }),
        },
        Pair {
            utterance: Text {
                turn: id(5),
                rev: 0,
                text: u3,
                hidden: &[],
            },
            reply: None,
        },
    ];
    let candidates = [Candidate {
        turn: id(5),
        pattern,
        new: range_of(u3, new),
        old: Some(range_of(u3, old)),
        previous: Some(Text {
            turn: id(4),
            rev: 0,
            text: r2,
            hidden: &[],
        }),
    }];
    let layout = Layout::new(&Input {
        language,
        pairs: &pairs,
        candidates: &candidates,
        correction_version: 1,
    })
    .unwrap();
    let counts: Vec<usize> = layout.pieces().map(str::len).collect();
    let plan = layout.plan(&counts, usize::MAX / 4).unwrap();
    layout.render(&plan)
}

fn ja() -> Call {
    call(
        PromptLanguage::Ja,
        [
            "去年の春から大阪に住んでいます。猫のミナトと暮らしていて、週末は将棋を指します。",
            "大阪での暮らしはいかがですか？ミナトちゃんとの週末も楽しそうですね。",
            "ありがとう",
            "どういたしまして。ほかにも気になることがあれば聞いてください。",
            "あ、大阪じゃなくて京都でした",
        ],
        "ミナト",
        "大阪",
        "京都でした",
        "{old}じゃなくて{new}",
    )
}

fn en() -> Call {
    call(
        PromptLanguage::En,
        [
            "I have lived in Osaka since last spring. I live with my cat Minato and play shogi on weekends.",
            "How do you like life in Osaka? Weekends with Minato sound fun.",
            "thanks",
            "You're welcome. Ask me anything else you'd like to know.",
            "Kyoto, not Osaka, sorry",
        ],
        "Minato",
        "Osaka",
        "Kyoto",
        "{new}, not {old}",
    )
}

#[test]
fn the_prompts_are_frozen() {
    assert_eq!(VARIANT, "sr1");
    for (lang, c) in [("ja", ja()), ("en", en())] {
        assert_eq!(c.requests.len(), 1, "{lang}");
        let r = &c.requests[0];
        check(&format!("system.{lang}.txt"), r.system);
        check(&format!("user.{lang}.txt"), &r.user);
    }
}

#[test]
fn the_schema_is_frozen() {
    let c = ja();
    let schema = serde_json::to_string_pretty(&c.requests[0].response_format()).unwrap() + "\n";
    check("response_format.json", &schema);
    // 文番号と候補の番号の列挙だけが区切りで変わる。英語の区切りも同じ数なら同じ型になる。
    assert_eq!(en().requests[0].schema(), c.requests[0].schema());
}
