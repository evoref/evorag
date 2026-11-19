//! プロキシの決まり (12 §7.5): 環境変数から HTTP の CONNECT のプロキシを選び、`NO_PROXY` で除く宛先を決める。
//!
//! - 読む環境変数は `HTTPS_PROXY`・`ALL_PROXY`・`HTTP_PROXY` の順で、それぞれ大文字の名前を先に、小文字の
//!   名前を後に読む ([`PROXY_VARS`])。空 (空白だけを含む) でない最初の値を使う。
//! - 使えるのは `http://` と `https://` のプロキシだけ。最初の値が SOCKS・ほかのスキーム・読めない値なら、
//!   直接の接続に切り替えずに、通信の前に断る ([`ProxyRefused`])。
//! - 除く宛先は `NO_PROXY` (無いか空なら `no_proxy`) のカンマ区切りの一覧 ([`NoProxy`])。
//! - Windows の WinINET の設定と PAC は読まない。
//!
//! ureq の `Proxy::try_from_env` は使わない (読む順・SOCKS の扱い・読めない値を飛ばす点が、上の決まりと違う)。

/// プロキシを読む環境変数 (読む順)。
pub const PROXY_VARS: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "HTTP_PROXY",
    "http_proxy",
];

/// プロキシを通さない宛先を読む環境変数 (読む順)。
pub const NO_PROXY_VARS: [&str; 2] = ["NO_PROXY", "no_proxy"];

/// 使えないプロキシの理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProxyProblem {
    /// SOCKS のプロキシ (`socks4://`・`socks5://` など)。
    Socks,
    /// `http` と `https` のほかのスキーム (スキームを書かない `host:port` を含む)。
    OtherScheme,
    /// URL として読めない (ホストが無いなど)。
    Unparseable,
}

impl std::fmt::Display for ProxyProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Socks => "SOCKS proxies are not supported",
            Self::OtherScheme => "only http:// and https:// proxies are supported",
            Self::Unparseable => "the value is not a proxy URL",
        })
    }
}

/// 使えないプロキシ (どの環境変数か、と理由)。値はパスワードを含みうるので持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("the proxy in {var} cannot be used: {problem}")]
pub struct ProxyRefused {
    /// 環境変数の名前。
    pub var: &'static str,
    /// 理由。
    pub problem: ProxyProblem,
}

/// 選んだプロキシ。
#[derive(Clone)]
pub(crate) struct ProxySetting {
    /// 値を読んだ環境変数。
    pub var: &'static str,
    /// ureq のプロキシ。
    pub proxy: ureq::Proxy,
    /// プロキシを通さない宛先。
    pub no_proxy: NoProxy,
}

impl std::fmt::Debug for ProxySetting {
    // 値 (URL) はパスワードを含みうるので示さない。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxySetting")
            .field("var", &self.var)
            .field("no_proxy", &self.no_proxy)
            .finish_non_exhaustive()
    }
}

impl ProxySetting {
    /// `host` への接続にプロキシを使うか (`NO_PROXY` に当たらなければ使う)。
    pub(crate) fn applies_to(&self, host: &str) -> bool {
        !self.no_proxy.matches(host)
    }
}

/// 環境変数からプロキシを選ぶ。`get` は環境変数の値 (無ければ `None`)。
///
/// # Errors
///
/// 空でない最初の値が、`http://`・`https://` のプロキシとして使えない ([`ProxyRefused`])。
pub(crate) fn choose(
    get: impl Fn(&str) -> Option<String>,
) -> Result<Option<ProxySetting>, ProxyRefused> {
    let Some((var, value)) = first_non_empty(&PROXY_VARS, &get) else {
        return Ok(None);
    };
    let refuse = |problem| ProxyRefused { var, problem };
    match classify(&value) {
        Some(problem) => Err(refuse(problem)),
        None => {
            let proxy = ureq::Proxy::new(&value).map_err(|_| refuse(ProxyProblem::Unparseable))?;
            let no_proxy = first_non_empty(&NO_PROXY_VARS, &get)
                .map(|(_, v)| NoProxy::parse(&v))
                .unwrap_or_default();
            Ok(Some(ProxySetting {
                var,
                proxy,
                no_proxy,
            }))
        }
    }
}

/// 環境変数の値を読む ([`choose`] に渡す)。Unicode でない値は無いものとして扱う。
pub(crate) fn from_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// `names` の順で、空白を除いて空でない最初の値。
fn first_non_empty(
    names: &[&'static str],
    get: &impl Fn(&str) -> Option<String>,
) -> Option<(&'static str, String)> {
    names.iter().find_map(|&name| {
        get(name)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .map(|v| (name, v))
    })
}

/// プロキシの値を確かめる。使えれば `None`。
fn classify(value: &str) -> Option<ProxyProblem> {
    let socks = |scheme: &str| {
        matches!(
            scheme.to_ascii_lowercase().as_str(),
            "socks" | "socks4" | "socks4a" | "socks5" | "socks5h"
        )
    };
    match url::Url::parse(value) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => {
            if u.host_str().is_some_and(|h| !h.is_empty()) {
                None
            } else {
                Some(ProxyProblem::Unparseable)
            }
        }
        Ok(u) if socks(u.scheme()) => Some(ProxyProblem::Socks),
        Ok(_) => Some(ProxyProblem::OtherScheme),
        Err(_) => match value.split_once("://") {
            Some((scheme, _)) if socks(scheme) => Some(ProxyProblem::Socks),
            _ => Some(ProxyProblem::Unparseable),
        },
    }
}

/// プロキシを通さない宛先 (`NO_PROXY`)。
///
/// カンマ区切りの一覧で、各項目の前後の空白を除き、大文字と小文字を区別しない。
/// - `*`: すべての宛先。
/// - `example.com`・`.example.com`・`*.example.com`: `example.com` と、その下のすべてのホスト
///   (先頭の点と `*.` の有無を問わない、末尾の一致)。
/// - 項目の末尾の `:<ポート>` は除いて比べる (接続先はどれも 443)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NoProxy {
    all: bool,
    domains: Vec<String>,
}

impl NoProxy {
    /// 一覧を読む。読めない項目 (空など) は飛ばす。
    pub(crate) fn parse(list: &str) -> Self {
        let mut out = Self::default();
        for entry in list.split(',') {
            let entry = entry.trim().to_ascii_lowercase();
            if entry == "*" {
                out.all = true;
                continue;
            }
            let entry = entry.strip_prefix('*').unwrap_or(&entry);
            let entry = entry.trim_start_matches('.');
            let entry = match entry.rsplit_once(':') {
                Some((host, port))
                    if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    host
                }
                _ => entry,
            };
            let entry = entry.trim_end_matches('.');
            if !entry.is_empty() {
                out.domains.push(entry.to_owned());
            }
        }
        out
    }

    /// `host` がプロキシを通さない宛先か。
    pub(crate) fn matches(&self, host: &str) -> bool {
        if self.all {
            return true;
        }
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.domains.iter().any(|d| {
            host == *d
                || host
                    .strip_suffix(d.as_str())
                    .is_some_and(|rest| rest.ends_with('.'))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn chosen(pairs: &[(&str, &str)]) -> Option<(&'static str, String, u16)> {
        choose(env(pairs))
            .expect("usable")
            .map(|p| (p.var, p.proxy.host().to_owned(), p.proxy.port()))
    }

    #[test]
    fn no_variable_means_a_direct_connection() {
        assert!(chosen(&[]).is_none());
        // 空と空白だけの値は無いものとして扱う。
        assert!(chosen(&[("HTTPS_PROXY", ""), ("ALL_PROXY", "  ")]).is_none());
        // NO_PROXY だけでは何もしない。
        assert!(chosen(&[("NO_PROXY", "*")]).is_none());
    }

    #[test]
    fn https_proxy_then_all_proxy_then_http_proxy_each_upper_case_first() {
        let all = [
            ("HTTPS_PROXY", "http://a:1"),
            ("https_proxy", "http://b:2"),
            ("ALL_PROXY", "http://c:3"),
            ("all_proxy", "http://d:4"),
            ("HTTP_PROXY", "http://e:5"),
            ("http_proxy", "http://f:6"),
        ];
        for skip in 0..all.len() {
            let rest = &all[skip..];
            let (var, host, port) = chosen(rest).expect("a proxy");
            assert_eq!(var, rest[0].0);
            assert_eq!(
                (host.as_str(), port),
                (["a", "b", "c", "d", "e", "f"][skip], 1 + skip as u16)
            );
        }
        // 空の値は飛ばして、次の変数を読む。
        assert_eq!(
            chosen(&[
                ("HTTPS_PROXY", " "),
                ("http_proxy", "https://p.example:3128")
            ]),
            Some(("http_proxy", "p.example".to_owned(), 3128))
        );
    }

    #[test]
    fn only_http_and_https_proxies_are_used_and_others_are_refused_by_name() {
        for ok in [
            "http://proxy.example.com:8080",
            "https://proxy.example.com",
            "HTTP://Proxy.Example.com:8080/",
            "http://user:pa%40ss@proxy.example.com:8080",
        ] {
            assert!(chosen(&[("HTTPS_PROXY", ok)]).is_some(), "{ok}");
        }
        for (value, problem) in [
            ("socks5://proxy:1080", ProxyProblem::Socks),
            ("socks5h://proxy:1080", ProxyProblem::Socks),
            ("SOCKS4://proxy:1080", ProxyProblem::Socks),
            ("socks://proxy:1080", ProxyProblem::Socks),
            ("ftp://proxy:21", ProxyProblem::OtherScheme),
            ("proxy.example.com:8080", ProxyProblem::OtherScheme),
            ("http://", ProxyProblem::Unparseable),
            ("http://[::1", ProxyProblem::Unparseable),
            ("not a url", ProxyProblem::Unparseable),
        ] {
            // 最初の値が使えなければ、後の変数に使える値があっても、直接の接続にもせずに断る。
            let e = choose(env(&[("ALL_PROXY", value), ("HTTP_PROXY", "http://ok:1")]))
                .expect_err(value);
            assert_eq!(
                e,
                ProxyRefused {
                    var: "ALL_PROXY",
                    problem
                },
                "{value}"
            );
        }
    }

    #[test]
    fn no_proxy_is_read_from_upper_then_lower_case() {
        let p = choose(env(&[
            ("HTTPS_PROXY", "http://p:1"),
            ("NO_PROXY", "github.com"),
            ("no_proxy", "*"),
        ]))
        .expect("ok")
        .expect("proxy");
        assert!(!p.applies_to("github.com"));
        assert!(p.applies_to("objects.githubusercontent.com"));
        let p = choose(env(&[
            ("HTTPS_PROXY", "http://p:1"),
            ("NO_PROXY", " "),
            ("no_proxy", "*"),
        ]))
        .expect("ok")
        .expect("proxy");
        assert!(!p.applies_to("api.github.com"));
        let p = choose(env(&[("HTTPS_PROXY", "http://p:1")]))
            .expect("ok")
            .expect("proxy");
        assert!(p.applies_to("github.com"));
    }

    #[test]
    fn no_proxy_matches_domains_by_suffix() {
        let n =
            NoProxy::parse(" GitHub.com , .githubusercontent.com,*.example.org,  ,internal:8443");
        for host in [
            "github.com",
            "api.github.com",
            "API.GitHub.com",
            "github.com.",
            "objects.githubusercontent.com",
            "githubusercontent.com",
            "a.b.example.org",
            "example.org",
            "internal",
        ] {
            assert!(n.matches(host), "{host}");
        }
        for host in [
            "evilgithub.com",
            "github.com.evil",
            "notgithubusercontent.com",
            "example.com",
            "internal.example",
        ] {
            assert!(!n.matches(host), "{host}");
        }
        assert!(NoProxy::parse("*").matches("github.com"));
        assert!(NoProxy::parse("a.com, *").matches("api.github.com"));
        assert!(!NoProxy::parse("").matches("github.com"));
        assert!(!NoProxy::parse(",,").matches("github.com"));
        // `*` を含む別の形 (`*github.com`) は、点の有無を問わない末尾の一致として扱う。
        assert!(NoProxy::parse("*github.com").matches("api.github.com"));
    }
}
