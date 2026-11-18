//! 文言のカタログ (07 §6)。`locales/<言語>.toml` を本体に埋め込み、`{名前}` に引数を差し込む。
//!
//! 言語は設定の `ui.language` で選ぶ (`ja` と、v0.3 から `en`)。キーとカタログの差分と、言語の間の
//! 引数の名前の差は `cargo xtask i18n` が検査する (NFR-602)。それでもカタログに無いキーが来たときは、
//! キーの名前をそのまま出す (落ちない)。

use std::collections::HashMap;

use evorag_types::{Message, MessageKey};

const JA: &str = include_str!("../../../locales/ja.toml");
const EN: &str = include_str!("../../../locales/en.toml");

/// 属性の表示名の表の名前 (文言キーではない。`cargo xtask i18n` が属性の一覧と突き合わせる)。
pub const ATTR_TABLE: &str = "attr";

/// UI の言語 (設定の `ui.language`、07 §4.2・§6)。プロンプトの言語 (07 §6.1) とは別に選ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Language {
    /// 日本語 (既定)。
    #[default]
    Ja,
    /// 英語 (v0.3 から)。
    En,
}

impl Language {
    /// 同梱するすべての言語。
    pub const ALL: [Language; 2] = [Language::Ja, Language::En];

    /// 設定の値から読む。知らない値は `None`。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ja" => Some(Self::Ja),
            "en" => Some(Self::En),
            _ => None,
        }
    }

    /// 設定に書く値 (`locales/<言語>.toml` の名前)。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ja => "ja",
            Self::En => "en",
        }
    }

    /// 埋め込んだカタログの本文。
    pub const fn source(self) -> &'static str {
        match self {
            Self::Ja => JA,
            Self::En => EN,
        }
    }
}

impl std::str::FromStr for Language {
    type Err = ParseLanguageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| ParseLanguageError(s.to_owned()))
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 知らない UI の言語の名前 (`ja`・`en` 以外)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseLanguageError(String);

impl std::fmt::Display for ParseLanguageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown UI language {:?} (expected \"ja\" or \"en\")",
            self.0
        )
    }
}

impl std::error::Error for ParseLanguageError {}

#[derive(Debug, Clone)]
pub struct Catalog {
    language: Language,
    entries: HashMap<String, String>,
}

fn flatten(prefix: &str, t: &toml::Table, out: &mut HashMap<String, String>) {
    for (k, v) in t {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            toml::Value::String(s) => {
                out.insert(key, s.clone());
            }
            toml::Value::Table(t) => flatten(&key, t, out),
            _ => {}
        }
    }
}

impl Catalog {
    /// 設定の `ui.language` の値のカタログ。知らない値は `ja` (設定の読み込みが先に誤りとして止める)。
    pub fn load(language: &str) -> Self {
        Self::new(Language::parse(language).unwrap_or_default())
    }

    /// その言語のカタログ。
    pub fn new(language: Language) -> Self {
        let mut entries = HashMap::new();
        if let Ok(t) = toml::from_str::<toml::Table>(language.source()) {
            flatten("", &t, &mut entries);
        }
        Self { language, entries }
    }

    /// このカタログの言語。
    pub fn language(&self) -> Language {
        self.language
    }

    pub fn key(&self, key: MessageKey) -> String {
        self.entries
            .get(key.as_str())
            .cloned()
            .unwrap_or_else(|| key.as_str().to_owned())
    }

    pub fn message(&self, m: &Message) -> String {
        let mut s = self.key(m.key);
        for (name, value) in &m.args {
            s = s.replace(&format!("{{{name}}}"), value);
        }
        s
    }

    /// 属性の表示名 (カタログの `[attr]` の表、07 §3)。知らない属性は名前のまま出す。表と属性の一覧
    /// (`evorag-memory/attributes.toml`) の差は `cargo xtask i18n` が検査する。
    pub fn attribute(&self, name: &str) -> String {
        self.entries
            .get(&format!("{ATTR_TABLE}.{name}"))
            .cloned()
            .unwrap_or_else(|| name.to_owned())
    }

    /// 引数つきのキー (画面の側で作る文言)。
    pub fn with(&self, key: MessageKey, args: &[(&str, &str)]) -> String {
        let mut m = Message::new(key);
        for (n, v) in args {
            m = m.arg(n, *v);
        }
        self.message(&m)
    }

    /// 並べた語 (`ui.sep.comma` でつなぐ)。
    pub fn join_comma(&self, items: &[String]) -> String {
        items.join(&self.key(MessageKey::UiSepComma))
    }

    /// 事実の行の「主語・属性」(`ui.facts.label`)。
    pub fn fact_label(&self, subject: &str, attribute: &str) -> String {
        self.with(
            MessageKey::UiFactsLabel,
            &[("subject", subject), ("attr", attribute)],
        )
    }
}

#[cfg(test)]
mod tests {
    use evorag_types::has_cjk;

    use super::*;

    #[test]
    fn every_key_has_a_text_in_every_language_and_arguments_are_filled() {
        for lang in Language::ALL {
            let c = Catalog::new(lang);
            assert_eq!(c.language(), lang);
            for k in MessageKey::ALL {
                assert_ne!(
                    c.key(*k),
                    k.as_str(),
                    "missing {} text for {}",
                    lang.as_str(),
                    k.as_str()
                );
            }
            let m = Message::new(MessageKey::LlmHttp)
                .arg("status", "503")
                .arg("detail", "busy");
            let s = c.message(&m);
            assert!(
                s.contains("503") && s.contains("busy") && !s.contains('{'),
                "{s}"
            );
        }
    }

    #[test]
    fn the_language_is_chosen_by_name_and_unknown_names_fall_back_to_ja() {
        assert_eq!(Catalog::load("en").language(), Language::En);
        assert_eq!(Catalog::load("ja").language(), Language::Ja);
        assert_eq!(Catalog::load("fr").language(), Language::Ja);
        assert_eq!(
            Catalog::load("en").key(MessageKey::UiYou),
            "You",
            "the English catalog is used"
        );
        for lang in Language::ALL {
            assert_eq!(Language::parse(lang.as_str()), Some(lang));
            assert_eq!(lang.as_str().parse::<Language>(), Ok(lang));
            assert_eq!(lang.to_string(), lang.as_str());
        }
        let e = "fr".parse::<Language>().unwrap_err();
        assert!(e.to_string().contains("\"fr\""), "{e}");
    }

    /// verifies: AC-FR-908-1 (カタログの層: 英語のカタログの文言と属性の表示名に、漢字・仮名・全角の文字が無い)
    #[test]
    fn the_english_catalog_has_no_japanese_text() {
        let t: toml::Table = toml::from_str(EN).expect("locales/en.toml parses");
        let mut entries = HashMap::new();
        flatten("", &t, &mut entries);
        let mut bad: Vec<&String> = entries
            .iter()
            .filter(|(_, v)| has_cjk(v))
            .map(|(k, _)| k)
            .collect();
        bad.sort();
        assert!(bad.is_empty(), "Japanese text in en.toml: {bad:?}");
    }

    #[test]
    fn attribute_labels_fall_back_to_the_name() {
        let c = Catalog::load("ja");
        assert_eq!(c.attribute("residence"), "住んでいる所");
        assert_eq!(c.attribute("someday_attr"), "someday_attr");
        assert_eq!(Catalog::load("en").attribute("residence"), "Residence");
    }

    #[test]
    fn separators_and_fact_labels_follow_the_language() {
        let items = ["a".to_owned(), "b".to_owned()];
        assert_eq!(Catalog::load("ja").join_comma(&items), "a、b");
        assert_eq!(Catalog::load("en").join_comma(&items), "a, b");
        assert_eq!(
            Catalog::load("ja").fact_label("あなた", "年齢"),
            "あなた・年齢"
        );
        assert_eq!(Catalog::load("en").fact_label("You", "Age"), "You · Age");
    }
}
