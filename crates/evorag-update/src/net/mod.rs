//! 更新の確認と取得の通信 (12 §7.5)。
//!
//! - [`Fetcher`]: 取得の口。本番は [`HttpFetcher`] (ureq と rustls と graviola、S-06)、試験は偽物。
//! - [`policy`]: 固定の接続先・転送先の確かめ・User-Agent (純粋な関数)。
//! - [`platform`]: 通信の前の確かめ (CPU の機能と証明書のルート)。
//!
//! 取得したものはディスクに書かず、主記憶で扱う (12 §7.2 の apply の 2)。
//!
//! プロキシは ureq の扱い (`ureq::Proxy::try_from_env`) に従う。読む環境変数は、`ALL_PROXY`・
//! `all_proxy`・`HTTPS_PROXY`・`https_proxy`・`HTTP_PROXY`・`http_proxy` の順で、最初に読める値を使う。
//! 除く宛先は `NO_PROXY` (無ければ `no_proxy`) で、`*`・`.example.com`・`*.example.com`・`example.com` の
//! 形を読む。Windows の WinINET の設定と PAC は読まない (ureq の `win-system-proxy` を入れない)。
//! プロキシは HTTP の CONNECT で使い、TLS は evorag と GitHub の間で結ぶ。認証は URL に書いた Basic だけで、
//! NTLM などには対応しない。SOCKS は使えない (ureq の `socks-proxy` を入れない)。

mod client;
pub mod platform;
pub mod policy;
mod transport;

use std::time::Duration;

pub use client::HttpFetcher;
pub use policy::UrlRefused;

use crate::{MAX_ARCHIVE_BYTES, MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES};

/// 接続の時間の上限 (12 §7.5)。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// リリースの一覧・`release.json`・`.minisig` の、1 つにつき合わせた時間の上限 (12 §7.5)。
pub const SMALL_FILE_TIMEOUT: Duration = Duration::from_secs(30);

/// 配布物の合わせた時間の上限 (12 §7.5)。
pub const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(600);

/// リリースの一覧の大きさの上限 (12 §7.5)。
pub const MAX_LIST_BYTES: u64 = 1024 * 1024;

/// 取得するもの。大きさと時間の上限を決める (12 §7.5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resource {
    /// リリースの一覧。
    ReleaseList,
    /// `release.json`。
    Manifest,
    /// `release.json.minisig`。
    Signature,
    /// 配布物 (`size` は検証した `release.json` の値)。
    Archive {
        /// `release.json` の `size`。
        size: u64,
    },
}

impl Resource {
    /// 読むバイト数の上限。実際に読んだ数で数える (`Content-Length` には頼らない)。
    #[must_use]
    pub fn max_bytes(self) -> u64 {
        match self {
            Self::ReleaseList => MAX_LIST_BYTES,
            Self::Manifest => MAX_MANIFEST_BYTES as u64,
            Self::Signature => MAX_SIGNATURE_BYTES as u64,
            Self::Archive { size } => size.min(MAX_ARCHIVE_BYTES),
        }
    }

    /// 合わせた時間の上限 (転送を辿る間を含む)。
    #[must_use]
    pub const fn time_limit(self) -> Duration {
        match self {
            Self::Archive { .. } => ARCHIVE_TIMEOUT,
            Self::ReleaseList | Self::Manifest | Self::Signature => SMALL_FILE_TIMEOUT,
        }
    }
}

/// 取得の口。試験は偽物に差し替える。
pub trait Fetcher {
    /// 通信の前の確かめ (CPU の機能と証明書のルート、12 §7.5)。通信しない。
    ///
    /// # Errors
    ///
    /// CPU の機能か証明書のルートが無い。
    fn preflight(&self) -> Result<(), NetError>;

    /// `url` (固定の接続先、[`policy`]) を取得し、本文を返す。転送は 1 つずつ確かめてから辿る。
    /// 本文は [`Resource::max_bytes`] まで、時間は [`Resource::time_limit`] まで。
    ///
    /// # Errors
    ///
    /// 接続・時間切れ・大きさの上限・許さない転送先・回数の制限・200 でない応答 ([`NetError`])。
    fn fetch(&self, url: &str, resource: Resource) -> Result<Vec<u8>, NetError>;
}

/// 通信の失敗 (07 §5 の終了コード 1)。文言はログ向けの英語。利用者に示す文言は呼ぶ側が文言キーで作る。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NetError {
    /// CPU に graviola の要る機能が無い。
    #[error("the CPU lacks {}", missing.join(", "))]
    CpuUnsupported {
        /// 足りない機能。
        missing: Vec<&'static str>,
    },
    /// x86_64 と aarch64 のどちらでもない (graviola が対応しない)。
    #[error("the CPU architecture {arch} is not supported")]
    UnsupportedArch {
        /// アーキテクチャ。
        arch: &'static str,
    },
    /// 証明書のルートが見つからない (Linux の OS の証明書ストア)。
    #[error("no root certificates were found: {detail}")]
    NoCertRoots {
        /// 理由 (表示用に整えたもの)。
        detail: String,
    },
    /// 接続しない URL (最初の URL か転送先)。
    #[error("refused to connect to the URL: {0}")]
    Refused(#[from] UrlRefused),
    /// 転送が多すぎる。
    #[error("more than {} redirects", policy::MAX_REDIRECTS)]
    TooManyRedirects,
    /// 転送の応答に `Location` が無い。
    #[error("a redirect ({status}) without a location")]
    RedirectWithoutLocation {
        /// 状態のコード。
        status: u16,
    },
    /// 接続できない・TLS・プロキシ・読む途中の失敗。
    #[error("cannot connect: {detail}")]
    Connect {
        /// 理由 (表示用に整えたもの)。
        detail: String,
    },
    /// 時間の上限を超えた。
    #[error("did not finish within {} seconds", limit.as_secs())]
    TimedOut {
        /// 上限。
        limit: Duration,
    },
    /// 本文が大きさの上限を超えた (読むのを止めた)。
    #[error("the response is over {limit} bytes")]
    TooLarge {
        /// 上限 (バイト)。
        limit: u64,
    },
    /// GitHub の回数の制限に当たった。
    #[error("rate limited by GitHub")]
    RateLimited {
        /// 制限が解ける時刻 (UNIX 時刻の秒、分かれば)。
        reset: Option<u64>,
    },
    /// 見つからない (404)。
    #[error("not found (404)")]
    NotFound,
    /// 200 でも転送でもない応答。
    #[error("the server answered {status}")]
    Status {
        /// 状態のコード。
        status: u16,
    },
}

impl NetError {
    /// 07 §5 の終了コード (通信の失敗はどれも 1)。
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        1
    }

    /// 通信の前の確かめで断った (CPU の機能や証明書のルートが無い) か。手作業の更新を案内する。
    #[must_use]
    pub const fn is_platform(&self) -> bool {
        matches!(
            self,
            Self::CpuUnsupported { .. } | Self::UnsupportedArch { .. } | Self::NoCertRoots { .. }
        )
    }
}

/// UNIX 時刻 (秒) を `YYYY-MM-DD HH:MM:SS UTC` で示す (回数の制限が解ける時刻)。
#[must_use]
pub fn format_utc(seconds: u64) -> String {
    let date = crate::ReleaseDate::from_unix_time(seconds);
    let s = seconds % 86_400;
    format!(
        "{date} {:02}:{:02}:{:02} UTC",
        s / 3600,
        (s / 60) % 60,
        s % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_follow_the_spec() {
        assert_eq!(Resource::ReleaseList.max_bytes(), 1024 * 1024);
        assert_eq!(Resource::Manifest.max_bytes(), 64 * 1024);
        assert_eq!(Resource::Signature.max_bytes(), 4 * 1024);
        assert_eq!(Resource::Archive { size: 10 }.max_bytes(), 10);
        assert_eq!(
            Resource::Archive { size: u64::MAX }.max_bytes(),
            64 * 1024 * 1024
        );
        assert_eq!(Resource::Manifest.time_limit(), Duration::from_secs(30));
        assert_eq!(
            Resource::Archive { size: 1 }.time_limit(),
            Duration::from_secs(600)
        );
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn reset_times_are_shown_in_utc() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(1_795_000_000), "2026-11-18 11:06:40 UTC");
    }
}
