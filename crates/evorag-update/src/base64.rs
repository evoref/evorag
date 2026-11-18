//! 標準の base64 (RFC 4648 §4、`=` の詰め物つき) の厳密な復号。鍵と署名の形を確かめるためだけに使う。

fn value(c: u8) -> Option<u32> {
    let v = match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    };
    Some(u32::from(v))
}

/// 復号する。空・長さが 4 の倍数でない・知らない文字・途中の `=`・使わないビットが 0 でない、
/// のどれかなら `None`。
pub(crate) fn decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.is_empty() || !b.len().is_multiple_of(4) {
        return None;
    }
    let groups = b.len() / 4;
    let mut out = Vec::with_capacity(groups * 3);
    let (chunks, _) = b.as_chunks::<4>();
    for (i, chunk) in chunks.iter().enumerate() {
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && i + 1 != groups) {
            return None;
        }
        let mut v = 0u32;
        for &c in &chunk[..4 - pad] {
            v = v << 6 | value(c)?;
        }
        v <<= 6 * pad;
        let bytes = v.to_be_bytes();
        // bytes[0] は使わない。詰め物の分の下位のビットは 0 でなければならない。
        if bytes[4 - pad..].iter().any(|&x| x != 0) {
            return None;
        }
        out.extend_from_slice(&bytes[1..4 - pad]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn standard_base64_is_decoded_strictly() {
        assert_eq!(decode("TWFu").as_deref(), Some(&b"Man"[..]));
        assert_eq!(decode("TWE=").as_deref(), Some(&b"Ma"[..]));
        assert_eq!(decode("TQ==").as_deref(), Some(&b"M"[..]));
        assert_eq!(decode("+/+/").as_deref(), Some(&[0xfb, 0xff, 0xbf][..]));
        for bad in [
            "", "TWF", "TW=u", "T===", "TQ==TWFu", "TWF\n", "TR==", "TWF=", "TW-_", " TWFu",
        ] {
            assert_eq!(decode(bad), None, "{bad:?}");
        }
    }
}
