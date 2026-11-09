//! 封筒 (03 §3.2)。すべての記録の共通の形と、`records` の値のバイト列。
//!
//! 値 = 配置の版 (1 バイト) + xxh3-64 (8 バイト、リトルエンディアン) + 封筒の CBOR。
//! チェックサムは、表のキーと、保存した CBOR のバイト列そのもの (符号化し直さない) を覆う。

use ciborium::Value;
use evorag_types::{Id, Timestamp};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// 値の配置の版。
pub const LAYOUT_V1: u8 = 1;
const HEADER_LEN: usize = 1 + 8;

/// 封筒の既知の項目の名前 (CBOR の map のキー)。
mod field {
    pub const ID: &str = "id";
    pub const KIND: &str = "kind";
    pub const V: &str = "v";
    pub const CREATED_AT: &str = "created_at";
    pub const UPDATED_AT: &str = "updated_at";
    pub const WRITER: &str = "writer";
    pub const BODY: &str = "body";
    pub const ALL: [&str; 7] = [ID, KIND, V, CREATED_AT, UPDATED_AT, WRITER, BODY];
}

/// 1 件の記録。
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub id: Id,
    pub kind: String,
    /// その種類の形式の版。
    pub v: u32,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// 書いた evorag の版。
    pub writer: String,
    /// 本体 (項目名つきの CBOR の map)。知らない項目もこの中にそのまま残る (03 §4.2 の規則 7)。
    pub body: Value,
    /// 封筒の知らない項目。書き戻すときに同じ名前のまま残す。
    pub unknown: Vec<(Value, Value)>,
}

/// 値のバイト列を封筒として読めない理由。分類によって扱いが変わる (03 §8)。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// 壊れている (短すぎる、チェックサムが合わない)。読み直して確かめてから quarantine へ移す。
    #[error("corrupt record: {0}")]
    Corrupt(&'static str),
    /// チェックサムは合っているが読めない (知らない配置・新しい形)。新しい版の記録として数える。
    #[error("record written in a newer or unknown form: {0}")]
    Newer(String),
}

/// 本体を型に読み書きできない。
#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("body does not match the type: {0}")]
    Deserialize(String),
    #[error("body could not be serialized: {0}")]
    Serialize(String),
    #[error("body must serialize to a map")]
    NotAMap,
}

fn checksum(key: &[u8], cbor: &[u8]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(key);
    h.update(cbor);
    h.digest()
}

impl Envelope {
    /// 新しい記録 (作成と更新の時刻は同じ)。本体は空の map。
    pub fn new(id: Id, kind: &str, v: u32, now: Timestamp, writer: &str) -> Self {
        Self {
            id,
            kind: kind.to_owned(),
            v,
            created_at: now,
            updated_at: now,
            writer: writer.to_owned(),
            body: Value::Map(Vec::new()),
            unknown: Vec::new(),
        }
    }

    /// 本体を型として読む。知らない項目は無視する。
    pub fn body_as<T: DeserializeOwned>(&self) -> Result<T, BodyError> {
        self.body
            .deserialized()
            .map_err(|e| BodyError::Deserialize(e.to_string()))
    }

    /// 本体を型から書く (03 §4.2 の規則 7)。
    ///
    /// 型は項目を省略せずに書くこと (値の無い項目は null。`skip_serializing_if` を使わない)。
    /// 元の本体のうち、書いた項目に無い名前 (この版が知らない項目) だけを後ろに足して残す。
    pub fn set_body<T: Serialize>(&mut self, body: &T) -> Result<(), BodyError> {
        let Value::Map(mut new) =
            Value::serialized(body).map_err(|e| BodyError::Serialize(e.to_string()))?
        else {
            return Err(BodyError::NotAMap);
        };
        if let Value::Map(old) = std::mem::replace(&mut self.body, Value::Null) {
            for (k, v) in old {
                if !new.iter().any(|(nk, _)| *nk == k) {
                    new.push((k, v));
                }
            }
        }
        self.body = Value::Map(new);
        Ok(())
    }

    /// `records` の値のバイト列にする。`key` は表のキー (チェックサムが覆う)。
    pub fn encode(&self, key: &[u8]) -> Vec<u8> {
        let mut map: Vec<(Value, Value)> = vec![
            (field::ID.into(), self.id.to_string().into()),
            (field::KIND.into(), self.kind.clone().into()),
            (field::V.into(), u64::from(self.v).into()),
            (field::CREATED_AT.into(), self.created_at.as_millis().into()),
            (field::UPDATED_AT.into(), self.updated_at.as_millis().into()),
            (field::WRITER.into(), self.writer.clone().into()),
            (field::BODY.into(), self.body.clone()),
        ];
        map.extend(self.unknown.iter().cloned());
        let mut cbor = Vec::new();
        ciborium::into_writer(&Value::Map(map), &mut cbor)
            .expect("writing CBOR into a Vec cannot fail");
        let mut out = Vec::with_capacity(HEADER_LEN + cbor.len());
        out.push(LAYOUT_V1);
        out.extend_from_slice(&checksum(key, &cbor).to_le_bytes());
        out.extend_from_slice(&cbor);
        out
    }

    /// `records` の値のバイト列を読む。チェックサムは復号の前に確かめる。
    pub fn decode(key: &[u8], value: &[u8]) -> Result<Self, DecodeError> {
        let (&layout, rest) = value
            .split_first()
            .ok_or(DecodeError::Corrupt("empty value"))?;
        if layout != LAYOUT_V1 {
            // 配置の版が変わると、チェックサムの位置も変わりうるので、ここでは確かめられない。
            return Err(DecodeError::Newer(format!("unknown layout {layout}")));
        }
        let (sum, cbor) = rest
            .split_first_chunk::<8>()
            .ok_or(DecodeError::Corrupt("value shorter than its header"))?;
        if u64::from_le_bytes(*sum) != checksum(key, cbor) {
            return Err(DecodeError::Corrupt("checksum mismatch"));
        }
        let newer = |why: &str| DecodeError::Newer(why.to_owned());
        let Value::Map(map) = ciborium::from_reader::<Value, _>(cbor)
            .map_err(|e| DecodeError::Newer(format!("unreadable CBOR: {e}")))?
        else {
            return Err(newer("envelope is not a map"));
        };

        let mut known: [Option<Value>; 7] = Default::default();
        let mut unknown = Vec::new();
        for (k, v) in map {
            match k
                .as_text()
                .and_then(|t| field::ALL.iter().position(|f| *f == t))
            {
                Some(i) if known[i].is_none() => known[i] = Some(v),
                Some(_) => return Err(newer("duplicate envelope field")),
                None => unknown.push((k, v)),
            }
        }
        let [id, kind, v, created_at, updated_at, writer, body] = known;
        let text = |v: Option<Value>, name: &str| match v {
            Some(Value::Text(t)) => Ok(t),
            _ => Err(DecodeError::Newer(format!(
                "envelope field `{name}` missing or not text"
            ))),
        };
        let int = |v: Option<Value>, name: &str| -> Result<i128, DecodeError> {
            match v {
                Some(Value::Integer(i)) => Ok(i128::from(i)),
                _ => Err(DecodeError::Newer(format!(
                    "envelope field `{name}` missing or not an integer"
                ))),
            }
        };
        let id: Id = text(id, field::ID)?
            .parse()
            .map_err(|_| newer("envelope `id` is not an id"))?;
        let ts = |n: i128, name: &str| {
            i64::try_from(n)
                .map(Timestamp::from_millis)
                .map_err(|_| DecodeError::Newer(format!("envelope `{name}` out of range")))
        };
        Ok(Self {
            id,
            kind: text(kind, field::KIND)?,
            v: u32::try_from(int(v, field::V)?).map_err(|_| newer("envelope `v` out of range"))?,
            created_at: ts(int(created_at, field::CREATED_AT)?, field::CREATED_AT)?,
            updated_at: ts(int(updated_at, field::UPDATED_AT)?, field::UPDATED_AT)?,
            writer: text(writer, field::WRITER)?,
            body: body.ok_or_else(|| newer("envelope `body` missing"))?,
            unknown,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn sample() -> Envelope {
        let mut e = Envelope::new(
            Id::from_bytes([1; 16]),
            "turn",
            1,
            Timestamp::from_millis(1_762_650_000_000),
            "0.0.1",
        );
        e.set_body(&OldBody {
            text: "こんにちは".into(),
            note: None,
        })
        .unwrap();
        e
    }

    /// 古い版が知る本体。
    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct OldBody {
        text: String,
        note: Option<String>,
    }

    /// 新しい版の本体 (項目を足した)。
    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct NewBody {
        text: String,
        note: Option<String>,
        lang: String,
    }

    #[test]
    fn encode_decode_round_trip() {
        let e = sample();
        let key = b"k1";
        let back = Envelope::decode(key, &e.encode(key)).unwrap();
        assert_eq!(back, e);
        assert_eq!(back.body_as::<OldBody>().unwrap().text, "こんにちは");
    }

    #[test]
    fn any_flipped_byte_is_detected() {
        let e = sample();
        let key = b"k1";
        let bytes = e.encode(key);
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0x01;
            let r = Envelope::decode(key, &b);
            if i == 0 {
                // 配置の版の破損は、知らない配置に見える (チェックサムの位置が決まらない)。
                assert!(matches!(r, Err(DecodeError::Newer(_))), "byte {i}");
            } else {
                assert_eq!(
                    r,
                    Err(DecodeError::Corrupt("checksum mismatch")),
                    "byte {i}"
                );
            }
        }
    }

    #[test]
    fn value_under_another_key_is_detected() {
        let e = sample();
        let bytes = e.encode(b"k1");
        assert_eq!(
            Envelope::decode(b"k2", &bytes),
            Err(DecodeError::Corrupt("checksum mismatch"))
        );
    }

    #[test]
    fn short_values_are_corrupt_and_unknown_layouts_are_newer() {
        assert!(matches!(
            Envelope::decode(b"k", &[]),
            Err(DecodeError::Corrupt(_))
        ));
        assert!(matches!(
            Envelope::decode(b"k", &[1, 2, 3]),
            Err(DecodeError::Corrupt(_))
        ));
        assert!(matches!(
            Envelope::decode(b"k", &[2, 0, 0]),
            Err(DecodeError::Newer(_))
        ));
    }

    #[test]
    fn valid_checksum_over_unreadable_cbor_is_newer_not_corrupt() {
        let key = b"k";
        let cbor = [0xA1, 0x61, b'x']; // 値の欠けた map
        let mut v = vec![LAYOUT_V1];
        v.extend_from_slice(&checksum(key, &cbor).to_le_bytes());
        v.extend_from_slice(&cbor);
        assert!(matches!(
            Envelope::decode(key, &v),
            Err(DecodeError::Newer(_))
        ));
    }

    #[test]
    fn unknown_body_fields_survive_write_back_by_an_older_reader() {
        // 新しい版が書いた記録。
        let mut e = sample();
        e.set_body(&NewBody {
            text: "a".into(),
            note: Some("n".into()),
            lang: "ja".into(),
        })
        .unwrap();
        e.unknown.push(("future".into(), Value::Bool(true)));
        let key = b"k";
        let stored = e.encode(key);

        // 古い版が読み、既知の項目だけを直して書き戻す。
        let mut old = Envelope::decode(key, &stored).unwrap();
        let mut body: OldBody = old.body_as().unwrap();
        body.text = "b".into();
        body.note = None; // 既知の項目を消す
        old.set_body(&body).unwrap();
        let rewritten = old.encode(key);

        // 新しい版が読み直すと、知らない項目は残り、消した既知の項目は戻っていない。
        let back = Envelope::decode(key, &rewritten).unwrap();
        let nb: NewBody = back.body_as().unwrap();
        assert_eq!(
            nb,
            NewBody {
                text: "b".into(),
                note: None,
                lang: "ja".into()
            }
        );
        assert_eq!(back.unknown, vec![("future".into(), Value::Bool(true))]);
    }

    #[test]
    fn set_body_rejects_non_maps() {
        let mut e = sample();
        assert!(matches!(e.set_body(&5u32), Err(BodyError::NotAMap)));
    }
}
