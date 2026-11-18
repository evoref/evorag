//! 通信の前の確かめ (12 §7.5): CPU の機能と、証明書のルート。

use std::sync::Arc;

use super::NetError;

/// x86_64 で graviola が要る機能 (graviola 0.4.5 の `is_cpu_supported` が見るもの、S-06 §4)。
/// 判定は [`graviola::util::is_cpu_supported`] で行い、この一覧は足りない機能の名前を示すためだけに使う。
/// graviola の版を上げるときは、この一覧を確かめ直す (12 §7.5)。
pub const X86_64_FEATURES: [&str; 6] = ["aes", "pclmulqdq", "bmi1", "adx", "avx", "avx2"];

/// aarch64 で graviola が要る機能 (同上)。
pub const AARCH64_FEATURES: [&str; 4] = ["neon", "aes", "pmull", "sha2"];

/// `required` のうち、`has` が無いと答えたもの (順は `required` のまま)。
#[must_use]
pub fn missing_features(
    required: &[&'static str],
    has: impl Fn(&str) -> bool,
) -> Vec<&'static str> {
    required.iter().copied().filter(|f| !has(f)).collect()
}

/// 動いている CPU に無い、graviola の要る機能の名前 (手で持つ一覧から、実行時に調べる)。
#[cfg(target_arch = "x86_64")]
fn host_missing_features() -> Vec<&'static str> {
    missing_features(&X86_64_FEATURES, |f| match f {
        "aes" => std::arch::is_x86_feature_detected!("aes"),
        "pclmulqdq" => std::arch::is_x86_feature_detected!("pclmulqdq"),
        "bmi1" => std::arch::is_x86_feature_detected!("bmi1"),
        "adx" => std::arch::is_x86_feature_detected!("adx"),
        "avx" => std::arch::is_x86_feature_detected!("avx"),
        "avx2" => std::arch::is_x86_feature_detected!("avx2"),
        _ => false,
    })
}

/// 動いている CPU に無い、graviola の要る機能の名前 (同上)。
#[cfg(target_arch = "aarch64")]
fn host_missing_features() -> Vec<&'static str> {
    missing_features(&AARCH64_FEATURES, |f| match f {
        "neon" => std::arch::is_aarch64_feature_detected!("neon"),
        "aes" => std::arch::is_aarch64_feature_detected!("aes"),
        "pmull" => std::arch::is_aarch64_feature_detected!("pmull"),
        "sha2" => std::arch::is_aarch64_feature_detected!("sha2"),
        _ => false,
    })
}

/// 動いている CPU で graviola が動くか (実行時の判定)。足りないと graviola は最初の暗号の処理で panic
/// するので、通信の前に確かめる (12 §7.5)。
///
/// 動くかどうかは graviola 自身の判定 ([`graviola::util::is_cpu_supported`]) で決める。断るときに示す
/// 足りない機能の名前は、手で持つ一覧 ([`X86_64_FEATURES`]・[`AARCH64_FEATURES`]) から求める (一覧から
/// 名前が見つからなければ空)。
///
/// # Errors
///
/// graviola が動かない ([`NetError::CpuUnsupported`])、x86_64 と aarch64 のどちらでもない
/// ([`NetError::UnsupportedArch`])。
pub fn check_cpu() -> Result<(), NetError> {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        if graviola::util::is_cpu_supported() {
            Ok(())
        } else {
            Err(NetError::CpuUnsupported {
                missing: host_missing_features(),
            })
        }
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Err(NetError::UnsupportedArch {
            arch: std::env::consts::ARCH,
        })
    }
}

/// 証明書のルートがあるか (12 §7.5)。OS の検証器を、通信せずに作ってみる。
///
/// Linux では OS の証明書ストア (`ca-certificates` など。`SSL_CERT_FILE`・`SSL_CERT_DIR` を含む) を読み、
/// 1 つも無ければ作れない。Windows と macOS は OS が検証するので、ここでは断らない。
/// [`check_cpu`] の後に呼ぶ (暗号の実装を組むため)。
///
/// # Errors
///
/// 検証器を作れない ([`NetError::NoCertRoots`])。
pub fn check_cert_roots() -> Result<(), NetError> {
    let provider = Arc::new(rustls_graviola::default_provider());
    rustls_platform_verifier::Verifier::new(provider)
        .map(drop)
        .map_err(|e| NetError::NoCertRoots {
            detail: crate::sanitize_for_display(&e.to_string(), crate::DISPLAY_MAX_BYTES),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_features_keep_the_required_order() {
        let all = |_: &str| true;
        assert!(missing_features(&X86_64_FEATURES, all).is_empty());
        let no_avx = |f: &str| !f.starts_with("avx");
        assert_eq!(missing_features(&X86_64_FEATURES, no_avx), ["avx", "avx2"]);
        let none = |_: &str| false;
        assert_eq!(missing_features(&AARCH64_FEATURES, none), AARCH64_FEATURES);
        assert_eq!(
            missing_features(&AARCH64_FEATURES, |f| f != "pmull"),
            ["pmull"]
        );
    }

    /// 試験を走らせる CPU で、graviola の判定と手で持つ一覧の判定が一致する (graviola の版を上げて
    /// 要る機能が変わったら、ここで気づく)。
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn graviola_and_the_hand_list_agree_on_the_host_cpu() {
        let missing = host_missing_features();
        assert_eq!(
            graviola::util::is_cpu_supported(),
            missing.is_empty(),
            "graviola and the list disagree; missing by the list: {missing:?}"
        );
        match check_cpu() {
            Ok(()) => assert!(missing.is_empty()),
            Err(NetError::CpuUnsupported { missing: m }) => assert_eq!(m, missing),
            Err(e) => panic!("unexpected: {e}"),
        }
    }
}
