//! 接続先の検査 (06 §9、FR-509)。既定ではループバックだけを許す。

use reqwest::Url;

use crate::LlmError;

/// 検査を通った接続先。
#[derive(Clone)]
pub struct Endpoint {
    url: Url,
    api_key: Option<String>,
}

impl std::fmt::Debug for Endpoint {
    /// API キーは出さない (06 §9)。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("url", &self.url.as_str())
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .finish()
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

impl Endpoint {
    /// `url` を検査する。ループバック以外は、`allow_remote` かつ `https` のときだけ許す。
    pub fn new(url: &str, api_key: Option<String>, allow_remote: bool) -> Result<Self, LlmError> {
        let parsed = Url::parse(url).map_err(|e| LlmError::BadEndpoint(format!("{url}: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(LlmError::BadEndpoint(format!(
                "{url}: only http and https are supported"
            )));
        }
        if !is_loopback(&parsed) {
            if !allow_remote {
                return Err(LlmError::RemoteNotAllowed(
                    parsed.host_str().unwrap_or("").to_owned(),
                ));
            }
            if parsed.scheme() != "https" {
                return Err(LlmError::BadEndpoint(format!(
                    "{url}: a non-loopback server needs https"
                )));
            }
        }
        Ok(Self {
            url: parsed,
            api_key: api_key.filter(|k| !k.is_empty()),
        })
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub(crate) fn join(&self, path: &str) -> Url {
        let mut u = self.url.clone();
        let base = u.path().trim_end_matches('/').to_owned();
        u.set_path(&format!("{base}{path}"));
        u.set_query(None);
        u
    }

    pub(crate) fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_are_allowed_by_default() {
        for u in [
            "http://127.0.0.1:8080",
            "http://localhost:8080",
            "http://LOCALHOST",
            "http://[::1]:8080",
            "http://127.1.2.3",
        ] {
            Endpoint::new(u, None, false).unwrap_or_else(|e| panic!("{u}: {e}"));
        }
    }

    /// verifies: AC-FR-509-1
    #[test]
    fn other_hosts_need_allow_remote_and_https() {
        for u in [
            "http://192.168.1.10:8080",
            "http://example.com",
            "https://example.com",
        ] {
            assert!(matches!(
                Endpoint::new(u, None, false),
                Err(LlmError::RemoteNotAllowed(_))
            ));
        }
        assert!(matches!(
            Endpoint::new("http://example.com", None, true),
            Err(LlmError::BadEndpoint(_))
        ));
        Endpoint::new("https://example.com", None, true).unwrap();
        assert!(Endpoint::new("ftp://127.0.0.1", None, false).is_err());
        assert!(Endpoint::new("not a url", None, false).is_err());
    }

    #[test]
    fn join_keeps_a_base_path() {
        let e = Endpoint::new("http://127.0.0.1:8080/llm/", None, false).unwrap();
        assert_eq!(
            e.join("/v1/models").as_str(),
            "http://127.0.0.1:8080/llm/v1/models"
        );
    }

    #[test]
    fn debug_hides_the_api_key() {
        let e = Endpoint::new("http://127.0.0.1", Some("secret".into()), false).unwrap();
        assert!(!format!("{e:?}").contains("secret"));
    }
}
