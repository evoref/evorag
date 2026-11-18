//! 取得の流れ (12 §7.5): 転送を 1 つずつ確かめて辿り、本文を上限まで読み、応答を理由に分ける。
//!
//! 通信そのものは [`Transport`] (本番は ureq) に任せ、ここは通信を知らない。試験は偽の [`Transport`] で
//! 確かめる。

use std::io::Read;
use std::time::{Duration, Instant, SystemTime};

use url::Url;

use super::transport::UreqTransport;
use super::{Fetcher, NetError, Resource, platform, policy};
use crate::Version;

/// 応答のうち、ここが読む見出し。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Headers {
    /// `Location`。
    pub location: Option<String>,
    /// `Retry-After`。
    pub retry_after: Option<String>,
    /// `X-RateLimit-Remaining`。
    pub ratelimit_remaining: Option<String>,
    /// `X-RateLimit-Reset` (UNIX 時刻の秒)。
    pub ratelimit_reset: Option<String>,
}

/// 1 回の要求の応答 (転送を辿らない)。
pub(crate) struct Response<'a> {
    pub status: u16,
    pub headers: Headers,
    pub body: Box<dyn Read + 'a>,
}

/// 1 回の GET を送る。転送を辿らず、どの状態のコードも応答として返す。
pub(crate) trait Transport {
    /// `url` へ GET を送る。`timeout` は、本文を読み終えるまでを含めた残りの時間。
    fn get(&self, url: &Url, timeout: Duration) -> Result<Response<'_>, NetError>;
}

/// 本番の取得 (ureq と rustls と graviola、OS の証明書の検証器)。
#[derive(Debug)]
pub struct HttpFetcher {
    transport: UreqTransport,
}

impl HttpFetcher {
    /// User-Agent に `version` を入れる取得 (12 §7.5)。通信しない。
    ///
    /// 暗号の実装は最初の通信で組むので、通信の前に [`Fetcher::preflight`] を呼ぶ。
    #[must_use]
    pub fn new(version: Version) -> Self {
        Self {
            transport: UreqTransport::new(&policy::user_agent(version)),
        }
    }
}

impl Fetcher for HttpFetcher {
    fn preflight(&self) -> Result<(), NetError> {
        platform::check_cpu()?;
        platform::check_cert_roots()
    }

    fn fetch(&self, url: &str, resource: Resource) -> Result<Vec<u8>, NetError> {
        fetch_with(
            &self.transport,
            url,
            resource.max_bytes(),
            resource.time_limit(),
        )
    }
}

/// 転送を辿りながら取得する。
pub(crate) fn fetch_with(
    transport: &dyn Transport,
    url: &str,
    max_bytes: u64,
    time_limit: Duration,
) -> Result<Vec<u8>, NetError> {
    let deadline = Instant::now() + time_limit;
    let mut url = policy::check_url(url)?;
    let mut redirects = 0;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(NetError::TimedOut { limit: time_limit })?;
        let response = transport.get(&url, remaining).map_err(|e| match e {
            NetError::TimedOut { .. } => NetError::TimedOut { limit: time_limit },
            e => e,
        })?;
        match response.status {
            200 => return read_limited(response.body, max_bytes, deadline, time_limit),
            301 | 302 | 303 | 307 | 308 => {
                let location =
                    response
                        .headers
                        .location
                        .ok_or(NetError::RedirectWithoutLocation {
                            status: response.status,
                        })?;
                if redirects == policy::MAX_REDIRECTS {
                    return Err(NetError::TooManyRedirects);
                }
                redirects += 1;
                url = policy::redirect_target(&url, &location)?;
            }
            status => {
                if let Some(reset) = rate_limit(status, &response.headers, unix_now()) {
                    return Err(NetError::RateLimited { reset });
                }
                return Err(if status == 404 {
                    NetError::NotFound
                } else {
                    NetError::Status { status }
                });
            }
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// 回数の制限に当たったか (12 §7.5)。当たれば、制限が解ける時刻 (分かれば) を返す。
///
/// - 403 か 429 で、`X-RateLimit-Remaining` が 0: 匿名の呼び出しの制限。時刻は `X-RateLimit-Reset`。
/// - 403 か 429 で、`Retry-After` がある: 二次の制限。時刻は今から `Retry-After` の秒の後 (日時の形の
///   `Retry-After` は読まず、時刻を示さない)。
pub(crate) fn rate_limit(status: u16, headers: &Headers, now: u64) -> Option<Option<u64>> {
    if !matches!(status, 403 | 429) {
        return None;
    }
    let number = |v: &Option<String>| v.as_deref().and_then(|s| s.trim().parse::<u64>().ok());
    if headers
        .ratelimit_remaining
        .as_deref()
        .is_some_and(|r| r.trim() == "0")
    {
        let reset = number(&headers.ratelimit_reset)
            .or_else(|| number(&headers.retry_after).map(|s| now.saturating_add(s)));
        return Some(reset);
    }
    headers
        .retry_after
        .as_ref()
        .map(|_| number(&headers.retry_after).map(|s| now.saturating_add(s)))
}

/// 本文を `max_bytes` まで読む。実際に読んだバイト数で数え、超えたら読むのを止める。
fn read_limited(
    mut body: impl Read,
    max_bytes: u64,
    deadline: Instant,
    time_limit: Duration,
) -> Result<Vec<u8>, NetError> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if Instant::now() >= deadline {
            return Err(NetError::TimedOut { limit: time_limit });
        }
        let n = match body.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Err(NetError::TimedOut { limit: time_limit });
            }
            Err(e) => {
                return Err(NetError::Connect {
                    detail: crate::sanitize_for_display(&e.to_string(), crate::DISPLAY_MAX_BYTES),
                });
            }
        };
        if n == 0 {
            return Ok(out);
        }
        if (out.len() + n) as u64 > max_bytes {
            return Err(NetError::TooLarge { limit: max_bytes });
        }
        out.extend_from_slice(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    /// 決めた応答を順に返す偽の通信。送った URL を記録する。
    #[derive(Default)]
    struct FakeTransport {
        responses: RefCell<VecDeque<(u16, Headers, Vec<u8>)>>,
        sent: RefCell<Vec<String>>,
        delay: Duration,
    }

    impl FakeTransport {
        fn with(responses: Vec<(u16, Headers, Vec<u8>)>) -> Self {
            Self {
                responses: RefCell::new(responses.into()),
                ..Self::default()
            }
        }
    }

    impl Transport for FakeTransport {
        fn get(&self, url: &Url, _timeout: Duration) -> Result<Response<'_>, NetError> {
            self.sent.borrow_mut().push(url.to_string());
            std::thread::sleep(self.delay);
            let (status, headers, body) = self
                .responses
                .borrow_mut()
                .pop_front()
                .expect("a scripted response");
            Ok(Response {
                status,
                headers,
                body: Box::new(std::io::Cursor::new(body)),
            })
        }
    }

    fn redirect(to: &str) -> (u16, Headers, Vec<u8>) {
        (
            302,
            Headers {
                location: Some(to.to_owned()),
                ..Headers::default()
            },
            Vec::new(),
        )
    }

    fn ok(body: &[u8]) -> (u16, Headers, Vec<u8>) {
        (200, Headers::default(), body.to_vec())
    }

    const START: &str = "https://github.com/evoref/evorag/releases/download/v0.4.0/a.zip";
    const LONG: Duration = Duration::from_secs(30);

    #[test]
    fn redirects_are_followed_one_by_one_up_to_five() {
        let t = FakeTransport::with(vec![
            redirect("https://objects.githubusercontent.com/1"),
            redirect("/2"),
            ok(b"body"),
        ]);
        assert_eq!(fetch_with(&t, START, 100, LONG).expect("ok"), b"body");
        assert_eq!(
            *t.sent.borrow(),
            [
                START,
                "https://objects.githubusercontent.com/1",
                "https://objects.githubusercontent.com/2"
            ]
        );

        let five: Vec<_> = (0..5)
            .map(|i| redirect(&format!("https://github.com/{i}")))
            .chain([ok(b"x")])
            .collect();
        assert!(fetch_with(&FakeTransport::with(five), START, 100, LONG).is_ok());
        let six: Vec<_> = (0..6)
            .map(|i| redirect(&format!("https://github.com/{i}")))
            .collect();
        let t = FakeTransport::with(six);
        assert!(matches!(
            fetch_with(&t, START, 100, LONG),
            Err(NetError::TooManyRedirects)
        ));
        assert_eq!(t.sent.borrow().len(), 6);
    }

    #[test]
    fn a_refused_redirect_target_is_not_requested() {
        for target in [
            "http://objects.githubusercontent.com/x",
            "https://evil.example/x",
            "https://user@github.com/x",
            "https://github.com:8443/x",
        ] {
            let t = FakeTransport::with(vec![redirect(target), ok(b"never")]);
            let e = fetch_with(&t, START, 100, LONG).expect_err(target);
            assert!(matches!(e, NetError::Refused(_)), "{target}: {e}");
            assert_eq!(t.sent.borrow().len(), 1, "{target}");
        }
        let t = FakeTransport::with(vec![(301, Headers::default(), Vec::new())]);
        assert!(matches!(
            fetch_with(&t, START, 100, LONG),
            Err(NetError::RedirectWithoutLocation { status: 301 })
        ));
        // 最初の URL も同じ規則で確かめ、合わなければ送らない。
        let t = FakeTransport::default();
        assert!(matches!(
            fetch_with(&t, "https://example.org/", 100, LONG),
            Err(NetError::Refused(_))
        ));
        assert!(t.sent.borrow().is_empty());
    }

    #[test]
    fn bodies_are_capped_by_the_bytes_actually_read() {
        let t = FakeTransport::with(vec![ok(&[7; 100])]);
        assert_eq!(fetch_with(&t, START, 100, LONG).expect("exact").len(), 100);
        let t = FakeTransport::with(vec![ok(&[7; 101])]);
        assert!(matches!(
            fetch_with(&t, START, 100, LONG),
            Err(NetError::TooLarge { limit: 100 })
        ));
        // 大きな本文も、上限を超えたところで止める (64 KiB ずつ読む)。
        let t = FakeTransport::with(vec![ok(&vec![0; 300 * 1024])]);
        assert!(matches!(
            fetch_with(&t, START, 1024, LONG),
            Err(NetError::TooLarge { limit: 1024 })
        ));
    }

    #[test]
    fn the_time_limit_covers_all_hops() {
        let t = FakeTransport {
            delay: Duration::from_millis(60),
            ..FakeTransport::with(vec![redirect("/1"), redirect("/2"), ok(b"late")])
        };
        let limit = Duration::from_millis(100);
        assert!(matches!(
            fetch_with(&t, START, 100, limit),
            Err(NetError::TimedOut { limit: l }) if l == limit
        ));
        assert!(t.sent.borrow().len() < 3);
    }

    #[test]
    fn statuses_are_told_apart() {
        let t = FakeTransport::with(vec![(404, Headers::default(), Vec::new())]);
        assert!(matches!(
            fetch_with(&t, START, 100, LONG),
            Err(NetError::NotFound)
        ));
        let t = FakeTransport::with(vec![(500, Headers::default(), Vec::new())]);
        assert!(matches!(
            fetch_with(&t, START, 100, LONG),
            Err(NetError::Status { status: 500 })
        ));
        let t = FakeTransport::with(vec![(
            403,
            Headers {
                ratelimit_remaining: Some("0".into()),
                ratelimit_reset: Some("1795000000".into()),
                ..Headers::default()
            },
            Vec::new(),
        )]);
        assert!(matches!(
            fetch_with(&t, START, 100, LONG),
            Err(NetError::RateLimited {
                reset: Some(1_795_000_000)
            })
        ));
    }

    #[test]
    fn rate_limits_are_recognized() {
        let h = |remaining: Option<&str>, reset: Option<&str>, retry: Option<&str>| Headers {
            ratelimit_remaining: remaining.map(str::to_owned),
            ratelimit_reset: reset.map(str::to_owned),
            retry_after: retry.map(str::to_owned),
            ..Headers::default()
        };
        let now = 1_000;
        // 匿名の制限: 403 か 429 で、残りが 0。
        assert_eq!(
            rate_limit(403, &h(Some("0"), Some("5000"), None), now),
            Some(Some(5000))
        );
        assert_eq!(rate_limit(429, &h(Some("0"), None, None), now), Some(None));
        assert_eq!(
            rate_limit(429, &h(Some("0"), None, Some("60")), now),
            Some(Some(1060))
        );
        // 二次の制限: Retry-After がある。
        assert_eq!(
            rate_limit(403, &h(None, None, Some("120")), now),
            Some(Some(1120))
        );
        assert_eq!(
            rate_limit(
                403,
                &h(Some("12"), None, Some("Wed, 21 Oct 2026 07:28:00 GMT")),
                now
            ),
            Some(None)
        );
        // 当たらない: 残りがある 403、ほかの状態のコード。
        assert_eq!(
            rate_limit(403, &h(Some("12"), Some("5000"), None), now),
            None
        );
        assert_eq!(rate_limit(403, &h(None, None, None), now), None);
        assert_eq!(
            rate_limit(500, &h(Some("0"), Some("5000"), Some("1")), now),
            None
        );
        assert_eq!(rate_limit(404, &h(Some("0"), None, None), now), None);
    }
}
