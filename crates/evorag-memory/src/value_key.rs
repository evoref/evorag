//! 値の鍵と再び覚えない印 (04 §5.6・§7.6)。
//!
//! - 正規化 v1 は **凍結する**: NFKC → 完全な case folding (CaseFolding.txt の C と F) → NFKC →
//!   White_Space の連なりを U+0020 の 1 つにまとめて両端を除く。依存の版を上げて結果が変わったら、
//!   試験 (`all_scalars_are_frozen`) が落ちる。
//! - HMAC-SHA256 は sha2 の上に組む (RFC 2104)。結果は秘密の比較に使わないので、定数時間にしない。
//! - HMAC に入れるバイト列は 04 §5.6 のとおりに凍結する (用途の印・長さを前に置く・値の鍵は生の 32 バイト)。

use std::fmt;

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// 値の鍵の正規化の版 (04 §5.6)。記録の `norm` に書く。
pub const NORM_V1: u32 = 1;

/// 値の鍵の入力の用途の印 (04 §5.6)。
const VK_TAG: &[u8] = b"evorag.vk.1\0";
/// 再び覚えない印の入力の用途の印 (04 §5.6)。
const MARK_TAG: &[u8] = b"evorag.mark.1\0";

/// 値の鍵の正規化 v1 (04 §5.6)。主語の呼び名にも同じものを当てる (04 §5.1)。
pub fn normalize_v1(s: &str) -> String {
    let nfkc: String = s.nfkc().collect();
    let folded = unicase::UniCase::unicode(nfkc.as_str()).to_folded_case();
    let mut out = String::with_capacity(folded.len());
    let mut pending_space = false;
    for c in folded.nfkc() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

/// データ根ごとの HMAC の鍵 (`meta` の `memory.forget_key` を復号した 32 バイト、03 §3.1.1)。
#[derive(Clone, PartialEq, Eq)]
pub struct ForgetKey([u8; 32]);

impl ForgetKey {
    pub fn from_bytes(b: [u8; 32]) -> Self {
        Self(b)
    }

    /// `meta` の値 (ちょうど 64 桁の小文字の 16 進) から読む。形が違えば `None`。
    pub fn from_hex(s: &str) -> Option<Self> {
        decode_hex32(s).map(Self)
    }

    /// `meta` に書く形 (64 桁の小文字の 16 進)。
    pub fn to_hex(&self) -> String {
        encode_hex(&self.0)
    }
}

impl fmt::Debug for ForgetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 秘密なので中身を出さない。
        f.write_str("ForgetKey(..)")
    }
}

/// 値の鍵 (04 §5.6)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueKey(pub [u8; 32]);

/// 再び覚えない印 (04 §7.6)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockMark(pub [u8; 32]);

macro_rules! hex32 {
    ($t:ident) => {
        impl $t {
            /// 記録に書く形 (64 桁の小文字の 16 進)。
            pub fn to_hex(&self) -> String {
                encode_hex(&self.0)
            }

            /// 記録の形から読む。形が違えば `None`。
            pub fn from_hex(s: &str) -> Option<Self> {
                decode_hex32(s).map(Self)
            }
        }

        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({}…)", stringify!($t), &self.to_hex()[..8])
            }
        }
    };
}
hex32!(ValueKey);
hex32!(BlockMark);

/// 値の鍵: HMAC-SHA256(K, "evorag.vk.1" ‖ 0x00 ‖ UTF-8 (正規化 v1 (値)))。
pub fn value_key(key: &ForgetKey, value: &str) -> ValueKey {
    let n = normalize_v1(value);
    let mut msg = Vec::with_capacity(VK_TAG.len() + n.len());
    msg.extend_from_slice(VK_TAG);
    msg.extend_from_slice(n.as_bytes());
    ValueKey(hmac_sha256(&key.0, &msg))
}

/// 再び覚えない印: HMAC-SHA256(K, "evorag.mark.1" ‖ 0x00 ‖ u32be(len S) ‖ S ‖ u32be(len A) ‖ A ‖ vk)。
/// S は正規化 v1 の後の主語、A は属性の名前。
pub fn block_mark(key: &ForgetKey, subject: &str, attribute: &str, vk: &ValueKey) -> BlockMark {
    let s = normalize_v1(subject);
    let mut msg = Vec::with_capacity(MARK_TAG.len() + 8 + s.len() + attribute.len() + 32);
    msg.extend_from_slice(MARK_TAG);
    push_with_len(&mut msg, s.as_bytes());
    push_with_len(&mut msg, attribute.as_bytes());
    msg.extend_from_slice(&vk.0);
    BlockMark(hmac_sha256(&key.0, &msg))
}

fn push_with_len(out: &mut Vec<u8>, b: &[u8]) {
    let len = u32::try_from(b.len()).expect("a field is shorter than 4 GiB");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(b);
}

/// HMAC-SHA256 (RFC 2104)。
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k0 = [0u8; BLOCK];
    if key.len() > BLOCK {
        k0[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k0[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k0.map(|b| b ^ 0x36));
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k0.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

fn encode_hex(b: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(char::from(DIGITS[usize::from(x >> 4)]));
        s.push(char::from(DIGITS[usize::from(x & 0xf)]));
    }
    s
}

fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, pair) in b.as_chunks::<2>().0.iter().enumerate() {
        out[i] = nib(pair[0])? << 4 | nib(pair[1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        encode_hex(b)
    }

    #[test]
    fn hmac_matches_rfc4231() {
        // RFC 4231 の試験の値 1・2・3・6。
        let cases: [(Vec<u8>, &[u8], &str); 4] = [
            (
                vec![0x0b; 20],
                b"Hi There",
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?",
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 20],
                &[0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
            (
                vec![0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First",
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
        ];
        for (key, msg, want) in cases {
            assert_eq!(hex(&hmac_sha256(&key, msg)), want);
        }
    }

    #[test]
    fn normalization_v1_examples() {
        let cases = [
            ("Tokyo", "tokyo"),
            ("TOKYO", "tokyo"),
            ("Straße", "strasse"),
            ("ΣΊΣΥΦΟΣ", "σίσυφοσ"),
            ("\u{212A}", "k"), // ケルビンの記号
            ("\u{0130}", "i\u{0307}"),
            ("ｶﾞｽ", "ガス"),
            ("㈱", "(株)"),
            ("  東京 \u{3000}タワー\t ", "東京 タワー"),
            ("C++", "c++"),
            ("C#", "c#"),
            ("とうきょう", "とうきょう"),
            ("トウキョウ", "トウキョウ"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_v1(input), want, "{input:?}");
        }
    }

    /// 凍結の試験 (04 §5.6): すべての Unicode のスカラー値を 1 文字ずつ正規化した結果のハッシュ。
    /// 依存 (unicode-normalization・unicase) や toolchain を上げて変わったら、ここが落ちる。
    /// そのときは値を書き換えず、古い結果を例外の表で保つ。
    #[test]
    fn all_scalars_are_frozen_and_idempotent() {
        let mut h = Sha256::new();
        let mut not_idempotent = Vec::new();
        for c in (0..=0x10FFFFu32).filter_map(char::from_u32) {
            let n = normalize_v1(c.encode_utf8(&mut [0; 4]));
            if normalize_v1(&n) != n {
                not_idempotent.push(c);
            }
            h.update(n.as_bytes());
            h.update([0xff]);
        }
        assert_eq!(
            not_idempotent,
            Vec::<char>::new(),
            "normalization v1 must be idempotent"
        );
        assert_eq!(
            hex(&h.finalize()),
            "6f28fb0aa86180219e458061d18ed859764dac233ab3bd82ff74987d482669dd"
        );
    }

    #[test]
    fn keys_and_marks_are_stable_and_separated() {
        let k = ForgetKey::from_bytes([7; 32]);
        let vk = value_key(&k, "Tokyo");
        // 正規化で同じになる表記は同じ鍵になる。
        assert_eq!(vk, value_key(&k, "  TOKYO "));
        assert_ne!(vk, value_key(&k, "Osaka"));
        // 鍵が違えば違う。
        assert_ne!(vk, value_key(&ForgetKey::from_bytes([8; 32]), "Tokyo"));
        // 印は主語と属性で分かれる。
        let m = block_mark(&k, "self", "residence", &vk);
        assert_ne!(m, block_mark(&k, "self", "hometown", &vk));
        assert_ne!(m, block_mark(&k, "妻", "residence", &vk));
        // 長さを前に置くので、主語と属性の境目をずらしても同じにならない。
        assert_ne!(
            block_mark(&k, "ab", "c", &vk),
            block_mark(&k, "a", "bc", &vk)
        );
        // 凍結した値 (バイト列の形を変えたら落ちる)。
        assert_eq!(
            vk.to_hex(),
            "89b30952b8f4e57555096033a90a912328ac61d7c9bb249861c2b5eb188dfa54"
        );
        assert_eq!(
            m.to_hex(),
            "ddabaab93d1cb62796502f31b8b14b744af0f4030ab961f84aa13c506772c77b"
        );
    }

    #[test]
    fn hex_round_trips_and_rejects_bad_input() {
        let k = ForgetKey::from_bytes([0xab; 32]);
        assert_eq!(ForgetKey::from_hex(&k.to_hex()), Some(k.clone()));
        assert_eq!(k.to_hex().len(), 64);
        assert!(ForgetKey::from_hex("AB").is_none());
        assert!(ForgetKey::from_hex(&"AB".repeat(32)).is_none()); // 大文字は受けない
        assert!(ForgetKey::from_hex(&"zz".repeat(32)).is_none());
        assert_eq!(format!("{k:?}"), "ForgetKey(..)");
    }
}
