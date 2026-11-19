//! 接続先の決まり (12 §7.5): 固定の URL・転送先の確かめ・User-Agent。どれも通信しない純粋な関数。

use url::Url;

use crate::Version;

/// リリースの一覧 (12 §7.5)。1 回の操作で 1 回だけ引く。
pub const RELEASES_URL: &str = "https://api.github.com/repos/evoref/evorag/releases?per_page=20";

/// Releases のページ (手作業の更新の案内、12 §7.1)。
pub const RELEASES_PAGE: &str = "https://github.com/evoref/evorag/releases";

/// 転送 (redirect) を辿る回数の上限 (12 §7.5)。
pub const MAX_REDIRECTS: usize = 5;

/// 転送を辿ってよいホスト (12 §7.5)。`*.githubusercontent.com` は [`is_allowed_host`] が見る。
const ALLOWED_HOSTS: [&str; 2] = ["github.com", "api.github.com"];

/// `*.githubusercontent.com` の末尾。
const CONTENT_HOST_SUFFIX: &str = ".githubusercontent.com";

/// `release.json`・`release.json.minisig`・配布物を取る URL (12 §7.5)。
/// `name` は `release.json`・`release.json.minisig`・検証した `artifacts[].name`。
#[must_use]
pub fn download_url(version: Version, name: &str) -> String {
    format!(
        "https://github.com/evoref/evorag/releases/download/{}/{name}",
        version.tag()
    )
}

/// 版のリリースのページ (`notes` が無いときに示す、12 §7.2 の apply の 5)。
#[must_use]
pub fn release_page_url(version: Version) -> String {
    format!("{RELEASES_PAGE}/tag/{}", version.tag())
}

/// 通信に含める OS の種類 (12 §7.5)。
#[must_use]
pub const fn os_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    }
}

/// 固定の User-Agent: `evorag/<版> (<OS>)` (12 §7.5)。
#[must_use]
pub fn user_agent(version: Version) -> String {
    format!("evorag/{version} ({})", os_name())
}

/// 接続しない URL の理由。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum UrlRefused {
    /// URL として読めない。
    #[error("not a URL")]
    NotUrl,
    /// `https` でない。
    #[error("not https")]
    NotHttps,
    /// 利用者の情報 (userinfo) を含む。
    #[error("it carries user information")]
    UserInfo,
    /// ポートが 443 でない。
    #[error("the port {0} is not 443")]
    Port(u16),
    /// 許したホストでない。
    #[error("the host {0} is not github.com, api.github.com or *.githubusercontent.com")]
    Host(String),
}

/// 接続してよい URL か (12 §7.5): `https`、userinfo を含まない、ポートが 443 (省略を含む)、ホストが
/// `github.com`・`api.github.com`・`*.githubusercontent.com` のどれか。
///
/// # Errors
///
/// 当たらない条件 ([`UrlRefused`])。
pub fn check_url(url: &str) -> Result<Url, UrlRefused> {
    let url = Url::parse(url).map_err(|_| UrlRefused::NotUrl)?;
    check_parsed(url)
}

/// 転送の応答の `Location` を、今の URL に対して解決してから [`check_url`] と同じ条件で確かめる
/// (相対の参照を許す。解決した先が条件に合わなければ辿らない)。
///
/// # Errors
///
/// 解決できない、または当たらない条件 ([`UrlRefused`])。
pub fn redirect_target(current: &Url, location: &str) -> Result<Url, UrlRefused> {
    let next = current
        .join(location.trim())
        .map_err(|_| UrlRefused::NotUrl)?;
    check_parsed(next)
}

fn check_parsed(url: Url) -> Result<Url, UrlRefused> {
    if url.scheme() != "https" {
        return Err(UrlRefused::NotHttps);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UrlRefused::UserInfo);
    }
    // `url` は https の既定のポート (443) を `None` にする。
    if let Some(port) = url.port() {
        return Err(UrlRefused::Port(port));
    }
    match url.host() {
        Some(url::Host::Domain(host)) if is_allowed_host(host) => Ok(url),
        Some(host) => Err(UrlRefused::Host(crate::sanitize_for_display(
            &host.to_string(),
            256,
        ))),
        None => Err(UrlRefused::Host(String::new())),
    }
}

/// 許したホストか。`url` が小文字にして IDNA で整えた名前を受け取る。
fn is_allowed_host(host: &str) -> bool {
    ALLOWED_HOSTS.contains(&host)
        || host.strip_suffix(CONTENT_HOST_SUFFIX).is_some_and(|label| {
            !label.is_empty()
                && !label.starts_with('.')
                && !label.ends_with('.')
                && !label.contains("..")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_urls_and_user_agent() {
        let v = Version::new(0, 4, 0);
        assert_eq!(
            download_url(v, "release.json"),
            "https://github.com/evoref/evorag/releases/download/v0.4.0/release.json"
        );
        assert_eq!(
            release_page_url(v),
            "https://github.com/evoref/evorag/releases/tag/v0.4.0"
        );
        let ua = user_agent(v);
        assert!(ua.starts_with("evorag/0.4.0 ("), "{ua}");
        assert!(
            ["(windows)", "(linux)", "(macos)", "(other)"]
                .iter()
                .any(|os| ua.ends_with(os)),
            "{ua}"
        );
        for url in [RELEASES_URL, &download_url(v, "x.zip")] {
            assert!(check_url(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn only_https_on_443_to_github_hosts_without_userinfo_is_allowed() {
        for ok in [
            "https://github.com/a",
            "https://api.github.com/repos",
            "https://objects.githubusercontent.com/x?y=1",
            "https://release-assets.githubusercontent.com/x",
            "https://GitHub.COM:443/a",
            "https://a.b.githubusercontent.com/x",
        ] {
            assert!(check_url(ok).is_ok(), "{ok}");
        }
        let refused = |u: &str| check_url(u).expect_err(u);
        assert_eq!(refused("http://github.com/a"), UrlRefused::NotHttps);
        assert_eq!(refused("ftp://github.com/a"), UrlRefused::NotHttps);
        assert_eq!(refused("https://u@github.com/a"), UrlRefused::UserInfo);
        assert_eq!(refused("https://u:p@github.com/a"), UrlRefused::UserInfo);
        assert_eq!(refused("https://:p@github.com/a"), UrlRefused::UserInfo);
        assert_eq!(refused("https://github.com:8443/a"), UrlRefused::Port(8443));
        assert_eq!(refused("https://github.com:80/a"), UrlRefused::Port(80));
        for host in [
            "https://evil.com/a",
            "https://github.com.evil.com/a",
            "https://githubusercontent.com/a",
            "https://evilgithubusercontent.com/a",
            "https://raw.githubusercontent.com.evil.com/a",
            "https://140.82.112.3/a",
            "https://[::1]/a",
            "https://gist.github.com/a",
        ] {
            assert!(
                matches!(check_url(host), Err(UrlRefused::Host(_))),
                "{host}"
            );
        }
        assert_eq!(refused("not a url"), UrlRefused::NotUrl);
        assert_eq!(refused("/relative"), UrlRefused::NotUrl);
    }

    #[test]
    fn redirect_targets_are_resolved_then_checked() {
        let base = check_url("https://github.com/evoref/evorag/releases/download/v0.4.0/a.zip")
            .expect("base");
        let to = |loc: &str| redirect_target(&base, loc);
        assert_eq!(
            to("https://objects.githubusercontent.com/x?sig=1")
                .expect("ok")
                .as_str(),
            "https://objects.githubusercontent.com/x?sig=1"
        );
        // 相対の参照は今の URL に対して解決する。
        assert_eq!(
            to("/other/path").expect("ok").as_str(),
            "https://github.com/other/path"
        );
        assert_eq!(
            to("b.zip").expect("ok").as_str(),
            "https://github.com/evoref/evorag/releases/download/v0.4.0/b.zip"
        );
        assert_eq!(
            to("  https://api.github.com/x  ").expect("ok").as_str(),
            "https://api.github.com/x"
        );
        // スキーム相対の参照も、解決した先で確かめる。
        assert!(matches!(to("//evil.com/x"), Err(UrlRefused::Host(_))));
        assert_eq!(
            to("http://objects.githubusercontent.com/x").expect_err("http"),
            UrlRefused::NotHttps
        );
        assert_eq!(
            to("https://user@objects.githubusercontent.com/x").expect_err("userinfo"),
            UrlRefused::UserInfo
        );
        assert_eq!(
            to("https://objects.githubusercontent.com:444/x").expect_err("port"),
            UrlRefused::Port(444)
        );
        assert!(matches!(
            to("https://example.org/"),
            Err(UrlRefused::Host(_))
        ));
        assert!(matches!(to("data:text/plain,x"), Err(UrlRefused::NotHttps)));
    }
}
