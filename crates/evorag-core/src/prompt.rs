//! システムプロンプト (05 §4.1 の順 1)。言語はデータ根ごとに固定 (07 §6.1)。
//!
//! v0.1 の中身: 固定の安全の指示、今の日時、発話の言語に合わせて答える指示。
//! 以前の会話の扱い方の指示は、語彙の想起を入れるとき (M1.6) に足す。

/// プロンプトの言語。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptLanguage {
    Ja,
    En,
}

impl PromptLanguage {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ja" => Some(Self::Ja),
            "en" => Some(Self::En),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ja => "ja",
            Self::En => "en",
        }
    }
}

const JA: &str = "あなたは、利用者の PC の中で動くアシスタントです。正確に、簡潔に答えてください。\
分からないことは分からないと答え、作り話をしないでください。\
利用者が日本語以外の言語で話しかけたときは、その言語で答えてください。";

const EN: &str = "You are an assistant running on the user's own computer. Answer accurately and concisely. \
If you do not know something, say so instead of making it up. \
Reply in the language the user writes in.";

pub fn system_prompt(lang: PromptLanguage, now_ms: i64) -> String {
    match lang {
        PromptLanguage::Ja => format!("{JA}\n今の日時: {}", crate::localtime::datetime(now_ms)),
        PromptLanguage::En => format!(
            "{EN}\nCurrent date and time: {}",
            crate::localtime::datetime(now_ms)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_the_date() {
        let p = system_prompt(PromptLanguage::Ja, 1_762_650_000_000);
        assert!(p.contains("今の日時: 2025-11-09"), "{p}");
        assert!(system_prompt(PromptLanguage::En, 0).contains("Current date and time: 19"));
    }
}
