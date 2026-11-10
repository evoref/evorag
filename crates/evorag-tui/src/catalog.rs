//! 文言のカタログ (07 §6)。`locales/<言語>.toml` を本体に埋め込み、`{名前}` に引数を差し込む。
//!
//! キーとカタログの差分は `cargo xtask i18n` が検査する (NFR-602)。それでもカタログに無いキーが
//! 来たときは、キーの名前をそのまま出す (落ちない)。

use std::collections::HashMap;

use evorag_types::{Message, MessageKey};

const JA: &str = include_str!("../../../locales/ja.toml");

pub struct Catalog {
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
    /// UI の言語のカタログ。v0.1 は `ja` だけ (07 §6)。
    pub fn load(language: &str) -> Self {
        // `en` は v0.3 で足す (OQ-08)。それまではどの言語でも `ja`。
        let _ = language;
        let text = JA;
        let mut entries = HashMap::new();
        if let Ok(t) = toml::from_str::<toml::Table>(text) {
            flatten("", &t, &mut entries);
        }
        Self { entries }
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

    /// 引数つきのキー (画面の側で作る文言)。
    pub fn with(&self, key: MessageKey, args: &[(&str, &str)]) -> String {
        let mut m = Message::new(key);
        for (n, v) in args {
            m = m.arg(n, *v);
        }
        self.message(&m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_has_a_japanese_text_and_arguments_are_filled() {
        let c = Catalog::load("ja");
        for k in MessageKey::ALL {
            assert_ne!(c.key(*k), k.as_str(), "missing ja text for {}", k.as_str());
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
