//! 本番の通信: ureq 3 と rustls と rustls-graviola、OS の証明書の検証器 (12 §7.5、S-06)。
//!
//! - 転送は辿らない (`max_redirects(0)`。辿るかは [`super::client`] が 1 つずつ決める)。
//! - `https` だけ。Cookie を持たず (ureq の `cookies` を入れない)、認証の見出しを送らず、
//!   `Accept-Encoding` を送らない (圧縮の機能を入れず、見出しも消す)。
//! - User-Agent は固定 (`evorag/<版> (<OS>)`)。
//! - 接続は 10 秒まで。合わせた時間は、要求ごとに残りの時間を渡す。
//! - プロキシは [`super::proxy`] が選んだものだけを使う (ureq に環境変数を読ませない)。`NO_PROXY` に当たる
//!   宛先は、要求ごとにプロキシを外す。

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use ureq::config::AutoHeaderValue;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use url::Url;

use super::client::{Headers, Response, Transport};
use super::proxy::ProxySetting;
use super::{CONNECT_TIMEOUT, NetError};

/// ureq の agent。作るだけでは通信しない。
#[derive(Debug)]
pub(crate) struct UreqTransport {
    agent: ureq::Agent,
    proxy: Option<ProxySetting>,
}

impl UreqTransport {
    pub(crate) fn new(user_agent: &str, proxy: Option<ProxySetting>) -> Self {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::PlatformVerifier)
            .unversioned_rustls_crypto_provider(Arc::new(rustls_graviola::default_provider()))
            .build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .save_redirect_history(false)
            .user_agent(AutoHeaderValue::Provided(Arc::new(user_agent.to_owned())))
            .accept_encoding(AutoHeaderValue::None)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            // 既定の設定は環境変数のプロキシを読むので、必ず上書きする。
            .proxy(proxy.as_ref().map(|p| p.proxy.clone()))
            .build()
            .new_agent();
        Self { agent, proxy }
    }

    /// `url` への要求でプロキシを外すか (`NO_PROXY` に当たる)。
    fn bypasses_proxy(&self, url: &Url) -> bool {
        self.proxy
            .as_ref()
            .is_some_and(|p| !p.applies_to(url.host_str().unwrap_or_default()))
    }
}

impl Transport for UreqTransport {
    fn get(&self, url: &Url, timeout: Duration) -> Result<Response<'_>, NetError> {
        let mut config = self
            .agent
            .get(url.as_str())
            .config()
            .timeout_global(Some(timeout));
        if self.bypasses_proxy(url) {
            config = config.proxy(None);
        }
        let response = config.build().call().map_err(|e| map_error(e, timeout))?;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let headers = Headers {
            location: header("location"),
            retry_after: header("retry-after"),
            ratelimit_remaining: header("x-ratelimit-remaining"),
            ratelimit_reset: header("x-ratelimit-reset"),
        };
        let status = response.status().as_u16();
        Ok(Response {
            status,
            headers,
            body: Box::new(TimeoutAware(response.into_body().into_reader())),
        })
    }
}

/// 本文を読む途中の ureq の時間切れ (`io::Error` に包んだ [`ureq::Error::Timeout`]) を、
/// [`std::io::ErrorKind::TimedOut`] にして返す読み手 ([`super::client`] が時間切れとして扱う)。
struct TimeoutAware<R>(R);

impl<R: Read> Read for TimeoutAware<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf).map_err(timeout_kind)
    }
}

/// 包んだ ureq の時間切れなら、種類を [`std::io::ErrorKind::TimedOut`] にする。
fn timeout_kind(e: std::io::Error) -> std::io::Error {
    let is_timeout = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<ureq::Error>())
        .is_some_and(|u| matches!(u, ureq::Error::Timeout(_)));
    if is_timeout {
        std::io::Error::new(std::io::ErrorKind::TimedOut, e)
    } else {
        e
    }
}

/// ureq の失敗を、示す理由に分ける。
fn map_error(e: ureq::Error, limit: Duration) -> NetError {
    match e {
        ureq::Error::Timeout(_) => NetError::TimedOut { limit },
        // 証明書のルートが無い (Linux の OS の検証器)。通信の前の確かめで分からなかったとき。
        ureq::Error::Rustls(rustls::Error::General(m)) if m.contains("No CA certificates") => {
            NetError::NoCertRoots {
                detail: crate::sanitize_for_display(&m, crate::DISPLAY_MAX_BYTES),
            }
        }
        ureq::Error::RequireHttpsOnly(_) => NetError::Refused(super::UrlRefused::NotHttps),
        ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::TimedOut => {
            NetError::TimedOut { limit }
        }
        e => NetError::Connect {
            detail: crate::sanitize_for_display(&e.to_string(), crate::DISPLAY_MAX_BYTES),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::proxy;

    const UA: &str = "evorag/0.4.0 (test)";

    /// 作った agent の設定を、通信せずに確かめる (12 §7.5)。
    #[test]
    fn the_agent_is_configured_without_connecting() {
        let t = UreqTransport::new(UA, None);
        let c = t.agent.config();
        assert_eq!(c.max_redirects(), 0);
        assert!(c.https_only());
        assert!(!c.http_status_as_error());
        assert!(!c.save_redirect_history());
        assert!(
            matches!(c.accept_encoding(), AutoHeaderValue::None),
            "{:?}",
            c.accept_encoding()
        );
        assert!(
            matches!(c.user_agent(), AutoHeaderValue::Provided(ua) if ua.as_str() == UA),
            "{:?}",
            c.user_agent()
        );
        let timeouts = c.timeouts();
        assert_eq!(timeouts.connect, Some(CONNECT_TIMEOUT));
        // 合わせた時間は要求ごとに渡す。
        assert_eq!(timeouts.global, None);
        // 環境変数のプロキシを ureq に読ませない。
        assert!(c.proxy().is_none());
    }

    #[test]
    fn the_chosen_proxy_is_used_except_for_no_proxy_hosts() {
        let get = |name: &str| match name {
            "HTTPS_PROXY" => Some("http://proxy.example:3128".to_owned()),
            "NO_PROXY" => Some(".githubusercontent.com".to_owned()),
            _ => None,
        };
        let setting = proxy::choose(get).expect("usable");
        let t = UreqTransport::new(UA, setting);
        let p = t.agent.config().proxy().expect("a proxy");
        assert_eq!((p.host(), p.port()), ("proxy.example", 3128));
        let url = |u: &str| Url::parse(u).expect("url");
        assert!(!t.bypasses_proxy(&url("https://github.com/x")));
        assert!(t.bypasses_proxy(&url("https://objects.githubusercontent.com/x")));
        let direct = UreqTransport::new(UA, None);
        assert!(!direct.bypasses_proxy(&url("https://github.com/x")));
    }

    #[test]
    fn a_wrapped_ureq_timeout_reads_as_timed_out() {
        let wrapped = ureq::Error::Timeout(ureq::Timeout::Global).into_io();
        assert_eq!(timeout_kind(wrapped).kind(), std::io::ErrorKind::TimedOut);
        let other = ureq::Error::ConnectionFailed.into_io();
        assert_ne!(timeout_kind(other).kind(), std::io::ErrorKind::TimedOut);
        let plain = std::io::Error::other("x");
        assert_eq!(timeout_kind(plain).kind(), std::io::ErrorKind::Other);

        /// 本文の途中で ureq の時間切れを返す読み手。
        struct Stalls;
        impl Read for Stalls {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(ureq::Error::Timeout(ureq::Timeout::RecvBody).into_io())
            }
        }
        let mut r = TimeoutAware(Stalls);
        let e = r.read(&mut [0; 4]).expect_err("stalls");
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
    }
}
