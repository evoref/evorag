//! 本番の通信: ureq 3 と rustls と rustls-graviola、OS の証明書の検証器 (12 §7.5、S-06)。
//!
//! - 転送は辿らない (`max_redirects(0)`。辿るかは [`super::client`] が 1 つずつ決める)。
//! - `https` だけ。Cookie を持たず (ureq の `cookies` を入れない)、認証の見出しを送らず、
//!   `Accept-Encoding` を送らない (圧縮の機能を入れず、見出しも消す)。
//! - User-Agent は固定 (`evorag/<版> (<OS>)`)。
//! - 接続は 10 秒まで。合わせた時間は、要求ごとに残りの時間を渡す。
//! - プロキシは ureq の既定 (`Proxy::try_from_env`、[`super`] の説明を参照)。

use std::sync::Arc;
use std::time::Duration;

use ureq::config::AutoHeaderValue;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use url::Url;

use super::client::{Headers, Response, Transport};
use super::{CONNECT_TIMEOUT, NetError};

/// ureq の agent。作るだけでは通信しない。
#[derive(Debug)]
pub(crate) struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub(crate) fn new(user_agent: &str) -> Self {
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
            .build()
            .new_agent();
        Self { agent }
    }
}

impl Transport for UreqTransport {
    fn get(&self, url: &Url, timeout: Duration) -> Result<Response<'_>, NetError> {
        let response = self
            .agent
            .get(url.as_str())
            .config()
            .timeout_global(Some(timeout))
            .build()
            .call()
            .map_err(|e| map_error(e, timeout))?;
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
            body: Box::new(response.into_body().into_reader()),
        })
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
