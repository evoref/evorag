//! スラッシュコマンドの読み取り (07 §3)。後の版のコマンドも名前は予約し、別の意味で使わない。

/// 読み取ったコマンド。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slash {
    Help,
    New(Option<String>),
    Sessions,
    Rename(String),
    Delete,
    Retry,
    Good,
    Bad(Option<String>),
    Quit,
    /// 名前は予約してあるが、この版ではまだ使えない (`/sources`・`/model`・`/memory` など)。
    NotYet(String),
    /// 知らないコマンド・引数の誤り。
    Invalid(String),
}

/// 後の版で入るコマンド (07 §3)。
const RESERVED: &[&str] = &[
    "sources", "model", "memory", "forget", "private", "precepts",
];

/// `/` で始まる入力を読む。`/` で始まらなければ `None` (普通の発話)。
/// `//` で始めると、先頭の `/` を 1 つ取った普通の発話として送る。
pub fn parse(input: &str) -> Option<Slash> {
    let s = input.trim();
    let rest = s.strip_prefix('/')?;
    if rest.starts_with('/') {
        return None;
    }
    let (name, arg) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n, Some(a.trim()).filter(|a| !a.is_empty())),
        None => (rest, None),
    };
    Some(match (name, arg) {
        ("help", _) => Slash::Help,
        ("new", a) => Slash::New(a.map(str::to_owned)),
        ("sessions", None) => Slash::Sessions,
        ("rename", Some(a)) => Slash::Rename(a.to_owned()),
        ("delete", None) => Slash::Delete,
        ("retry", None) => Slash::Retry,
        ("good", None) => Slash::Good,
        ("bad", a) => Slash::Bad(a.map(str::to_owned)),
        ("quit", None) => Slash::Quit,
        (n, _) if RESERVED.contains(&n) => Slash::NotYet(format!("/{n}")),
        _ => Slash::Invalid(format!("/{name}")),
    })
}

/// `//…` で始まる発話から、先頭の `/` を 1 つ取る。
pub fn unescape(input: &str) -> &str {
    match input.trim_start().strip_prefix("//") {
        Some(_) => &input.trim_start()[1..],
        None => input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_v0_1_commands() {
        assert_eq!(parse("/help"), Some(Slash::Help));
        assert_eq!(parse("/new"), Some(Slash::New(None)));
        assert_eq!(
            parse("/new  旅行の計画 "),
            Some(Slash::New(Some("旅行の計画".into())))
        );
        assert_eq!(
            parse("/rename 新しい名前"),
            Some(Slash::Rename("新しい名前".into()))
        );
        assert_eq!(parse("/bad 違う"), Some(Slash::Bad(Some("違う".into()))));
        assert_eq!(parse("/bad"), Some(Slash::Bad(None)));
        assert_eq!(parse(" /quit "), Some(Slash::Quit));
    }

    #[test]
    fn reserved_unknown_and_wrong_arguments() {
        assert_eq!(parse("/model"), Some(Slash::NotYet("/model".into())));
        assert_eq!(
            parse("/frobnicate"),
            Some(Slash::Invalid("/frobnicate".into()))
        );
        assert_eq!(parse("/rename"), Some(Slash::Invalid("/rename".into())));
        assert_eq!(parse("/delete now"), Some(Slash::Invalid("/delete".into())));
    }

    #[test]
    fn plain_text_and_escaped_slashes_are_utterances() {
        assert_eq!(parse("こんにちは"), None);
        assert_eq!(parse("//etc/hosts を見て"), None);
        assert_eq!(unescape("//etc/hosts を見て"), "/etc/hosts を見て");
        assert_eq!(unescape("普通"), "普通");
    }
}
