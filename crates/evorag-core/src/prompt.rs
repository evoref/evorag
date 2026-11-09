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

/// UTC のミリ秒を `YYYY-MM-DD HH:MM UTC` にする。
fn utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        sod / 3600,
        sod / 60 % 60
    )
}

pub fn system_prompt(lang: PromptLanguage, now_ms: i64) -> String {
    match lang {
        PromptLanguage::Ja => format!("{JA}\n今の日時: {}", utc(now_ms)),
        PromptLanguage::En => format!("{EN}\nCurrent date and time: {}", utc(now_ms)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_the_date() {
        let p = system_prompt(PromptLanguage::Ja, 1_762_650_000_000);
        assert!(p.ends_with("2025-11-09 01:00 UTC"), "{p}");
        assert!(system_prompt(PromptLanguage::En, 0).contains("1970-01-01 00:00 UTC"));
    }
}
