//! プロンプトの言語 (07 §6.1)。

/// プロンプトの言語。データ根ごとに固定し、途中で変えない (07 §6.1)。応答の経路 (`evorag-core`) と、区切りの
/// 呼び出し (B2、`evorag-memory`) で同じ型を使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptLanguage {
    Ja,
    En,
}

impl PromptLanguage {
    /// 設定の名前 (`ja`・`en`) から読む。ほかは `None`。
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ja" => Some(Self::Ja),
            "en" => Some(Self::En),
            _ => None,
        }
    }

    /// 設定の名前。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ja => "ja",
            Self::En => "en",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for l in [PromptLanguage::Ja, PromptLanguage::En] {
            assert_eq!(PromptLanguage::parse(l.as_str()), Some(l));
        }
        assert_eq!(PromptLanguage::parse("fr"), None);
    }
}
