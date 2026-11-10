//! キーの符号化 (03 §3.3)。
//!
//! キーを要素の並び (tuple) として、各要素の先頭に型の印 (1 バイト) を付け、順序を保つ符号で並べる。
//! バイト列の辞書順の比較が、要素ごとの意味の順と一致する。
//!
//! | 型 | 印 | 符号 |
//! |---|---|---|
//! | 文字列 | `0x01` | UTF-8。`0x00` を `0x00 0xFF` に置き換え、終端に `0x00` |
//! | バイト列 | `0x02` | 文字列と同じ |
//! | 整数 (時刻を含む) | `0x03` | 符号ビットを反転した 8 バイトのビッグエンディアン |
//! | ID | `0x04` | 16 バイト |

use evorag_types::Id;

/// キーの符号化の版 (`meta` の `key_encoding.version`、03 §3.1.1)。
pub const KEY_ENCODING_VERSION: u32 = 1;

const TAG_STR: u8 = 0x01;
const TAG_BYTES: u8 = 0x02;
const TAG_INT: u8 = 0x03;
const TAG_ID: u8 = 0x04;

/// キーの要素。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part<'a> {
    Str(&'a str),
    Bytes(&'a [u8]),
    Int(i64),
    Id(Id),
}

/// 読み戻した要素。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum OwnedPart {
    Str(String),
    Bytes(Vec<u8>),
    Int(i64),
    Id(Id),
}

/// キーとして読めないバイト列。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed key at byte {offset}: {reason}")]
pub struct KeyError {
    pub offset: usize,
    pub reason: &'static str,
}

/// キーを組み立てる。
#[derive(Debug, Default, Clone)]
pub struct KeyBuf(Vec<u8>);

impl KeyBuf {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, part: &Part<'_>) -> &mut Self {
        match part {
            Part::Str(s) => self.escaped(TAG_STR, s.as_bytes(), true),
            Part::Bytes(b) => self.escaped(TAG_BYTES, b, true),
            Part::Int(i) => {
                self.0.push(TAG_INT);
                self.0
                    .extend_from_slice(&((*i as u64) ^ (1 << 63)).to_be_bytes());
            }
            Part::Id(id) => {
                self.0.push(TAG_ID);
                self.0.extend_from_slice(id.as_bytes());
            }
        }
        self
    }

    pub fn str(&mut self, s: &str) -> &mut Self {
        self.push(&Part::Str(s))
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.push(&Part::Bytes(b))
    }

    pub fn int(&mut self, i: i64) -> &mut Self {
        self.push(&Part::Int(i))
    }

    pub fn id(&mut self, id: Id) -> &mut Self {
        self.push(&Part::Id(id))
    }

    /// 文字列の前方一致の走査に使う。終端を置かないので、この文字列で始まるすべての文字列の要素を含む。
    pub fn str_prefix(&mut self, s: &str) -> &mut Self {
        self.escaped(TAG_STR, s.as_bytes(), false);
        self
    }

    fn escaped(&mut self, tag: u8, raw: &[u8], terminate: bool) {
        self.0.push(tag);
        for &b in raw {
            self.0.push(b);
            if b == 0x00 {
                self.0.push(0xFF);
            }
        }
        if terminate {
            self.0.push(0x00);
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

/// 要素の並びを符号化する。
pub fn encode(parts: &[Part<'_>]) -> Vec<u8> {
    let mut k = KeyBuf::new();
    for p in parts {
        k.push(p);
    }
    k.into_bytes()
}

/// キーを要素の並びに戻す。
pub fn decode(mut bytes: &[u8]) -> Result<Vec<OwnedPart>, KeyError> {
    let total = bytes.len();
    let mut out = Vec::new();
    while let Some((&tag, rest)) = bytes.split_first() {
        let offset = total - bytes.len();
        let err = |reason| KeyError { offset, reason };
        bytes = rest;
        match tag {
            TAG_STR | TAG_BYTES => {
                let mut raw = Vec::new();
                loop {
                    match bytes {
                        [0x00, 0xFF, rest @ ..] => {
                            raw.push(0x00);
                            bytes = rest;
                        }
                        [0x00, rest @ ..] => {
                            bytes = rest;
                            break;
                        }
                        [b, rest @ ..] => {
                            raw.push(*b);
                            bytes = rest;
                        }
                        [] => return Err(err("unterminated string")),
                    }
                }
                out.push(if tag == TAG_STR {
                    OwnedPart::Str(String::from_utf8(raw).map_err(|_| err("invalid utf-8"))?)
                } else {
                    OwnedPart::Bytes(raw)
                });
            }
            TAG_INT => {
                let (n, rest) = bytes.split_first_chunk::<8>().ok_or(err("short integer"))?;
                out.push(OwnedPart::Int((u64::from_be_bytes(*n) ^ (1 << 63)) as i64));
                bytes = rest;
            }
            TAG_ID => {
                let (id, rest) = bytes.split_first_chunk::<16>().ok_or(err("short id"))?;
                out.push(OwnedPart::Id(Id::from_bytes(*id)));
                bytes = rest;
            }
            _ => return Err(err("unknown tag")),
        }
    }
    Ok(out)
}

/// `prefix` で始まるすべてのキーの範囲の上端 (含まない)。`None` は上端なし。
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xFF {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> Id {
        Id::from_bytes([n; 16])
    }

    #[test]
    fn round_trip() {
        let parts = [
            Part::Str("turn"),
            Part::Str("a\0b"),
            Part::Bytes(&[0, 0xFF, 0, 1]),
            Part::Int(-5),
            Part::Int(i64::MAX),
            Part::Id(id(7)),
            Part::Str(""),
        ];
        let owned: Vec<OwnedPart> = decode(&encode(&parts)).unwrap();
        assert_eq!(
            owned,
            [
                OwnedPart::Str("turn".into()),
                OwnedPart::Str("a\0b".into()),
                OwnedPart::Bytes(vec![0, 0xFF, 0, 1]),
                OwnedPart::Int(-5),
                OwnedPart::Int(i64::MAX),
                OwnedPart::Id(id(7)),
                OwnedPart::Str(String::new()),
            ]
        );
    }

    #[test]
    fn byte_order_matches_meaning_for_strings_of_different_lengths() {
        // 長さを前に置く符号では "b" < "aa" になってしまう (03 §3.3 の改訂の理由)。
        let words = ["", "\0", "\0\0", "a", "a\0", "aa", "ab", "b", "あ", "い"];
        for w in words.windows(2) {
            assert!(
                encode(&[Part::Str(w[0]), Part::Int(0)]) < encode(&[Part::Str(w[1]), Part::Int(0)]),
                "{:?} < {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn byte_order_matches_meaning_for_integers() {
        let nums = [i64::MIN, -1_000, -1, 0, 1, 1_000, i64::MAX];
        for w in nums.windows(2) {
            assert!(encode(&[Part::Int(w[0])]) < encode(&[Part::Int(w[1])]));
        }
    }

    #[test]
    fn string_prefix_covers_exactly_the_strings_that_start_with_it() {
        let mut p = KeyBuf::new();
        p.str("fact").str_prefix("ab");
        let start = p.as_bytes().to_vec();
        let end = prefix_end(&start).unwrap();
        let inside = |s: &str| {
            let k = encode(&[Part::Str("fact"), Part::Str(s), Part::Id(id(1))]);
            start.as_slice() <= k.as_slice() && k < end
        };
        assert!(inside("ab"));
        assert!(inside("abc"));
        assert!(inside("ab\0x"));
        assert!(!inside("a"));
        assert!(!inside("ac"));
        assert!(!inside("b"));
    }

    #[test]
    fn rejects_malformed_keys() {
        assert!(decode(&[TAG_STR, b'a']).is_err());
        assert!(decode(&[TAG_INT, 1, 2]).is_err());
        assert!(decode(&[TAG_ID, 0]).is_err());
        assert!(decode(&[0x7f]).is_err());
        assert!(decode(&[TAG_STR, 0xC3, 0x00]).is_err());
    }

    #[test]
    fn prefix_end_handles_ff() {
        assert_eq!(prefix_end(&[1, 2]), Some(vec![1, 3]));
        assert_eq!(prefix_end(&[1, 0xFF]), Some(vec![2]));
        assert_eq!(prefix_end(&[0xFF, 0xFF]), None);
    }
}
