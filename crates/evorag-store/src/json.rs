//! 封筒を JSON に写す (03 §11、§6)。書き出しとジャーナルの行で使う。
//!
//! JSON にそのまま写せない CBOR の値 (バイト列、タグ、±2^53 を超える整数、NaN と無限大、文字列でない
//! map のキー) は、その値の CBOR を base64 にした `{"$cbor": "<base64>"}` で写し、読むときに戻す。

use ciborium::Value as Cbor;
use evorag_types::{Id, Timestamp};
use serde_json::{Map, Number, Value as Json};

use crate::envelope::Envelope;

const MARK: &str = "$cbor";
const SAFE: i128 = 1 << 53;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| B64.iter().position(|&x| x == c).map(|p| p as u32);
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..chunk.len().saturating_sub(1)]);
    }
    Some(out)
}

fn wrap(v: &Cbor) -> Json {
    let mut bytes = Vec::new();
    ciborium::into_writer(v, &mut bytes).expect("writing CBOR into a Vec cannot fail");
    let mut m = Map::new();
    m.insert(MARK.into(), Json::String(b64_encode(&bytes)));
    Json::Object(m)
}

/// CBOR の値を JSON に写す。
pub fn to_json(v: &Cbor) -> Json {
    match v {
        Cbor::Null => Json::Null,
        Cbor::Bool(b) => Json::Bool(*b),
        Cbor::Text(t) => Json::String(t.clone()),
        Cbor::Integer(i) => {
            let n = i128::from(*i);
            if n.abs() <= SAFE {
                Json::Number(Number::from(n as i64))
            } else {
                wrap(v)
            }
        }
        Cbor::Float(f) => Number::from_f64(*f).map_or_else(|| wrap(v), Json::Number),
        Cbor::Array(a) => Json::Array(a.iter().map(to_json).collect()),
        Cbor::Map(entries) => {
            if entries.iter().all(|(k, _)| k.is_text()) {
                let mut m = Map::new();
                for (k, val) in entries {
                    let key = k.as_text().unwrap_or_default().to_owned();
                    if key == MARK || m.contains_key(&key) {
                        // 印と同じ名前や重複したキーは、元の形のまま運ぶ。
                        return wrap(v);
                    }
                    m.insert(key, to_json(val));
                }
                Json::Object(m)
            } else {
                wrap(v)
            }
        }
        _ => wrap(v),
    }
}

/// JSON を CBOR の値に戻す。
pub fn from_json(v: &Json) -> Result<Cbor, String> {
    Ok(match v {
        Json::Null => Cbor::Null,
        Json::Bool(b) => Cbor::Bool(*b),
        Json::String(s) => Cbor::Text(s.clone()),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Cbor::Integer(i.into())
            } else if let Some(u) = n.as_u64() {
                Cbor::Integer(u.into())
            } else {
                Cbor::Float(n.as_f64().ok_or("bad number")?)
            }
        }
        Json::Array(a) => Cbor::Array(a.iter().map(from_json).collect::<Result<_, _>>()?),
        Json::Object(m) => {
            if m.len() == 1
                && let Some(Json::String(b)) = m.get(MARK)
            {
                let bytes = b64_decode(b).ok_or("bad base64 in $cbor")?;
                return ciborium::from_reader(bytes.as_slice()).map_err(|e| e.to_string());
            }
            Cbor::Map(
                m.iter()
                    .map(|(k, v)| Ok((Cbor::Text(k.clone()), from_json(v)?)))
                    .collect::<Result<_, String>>()?,
            )
        }
    })
}

/// 封筒を JSON にする (チェックサムは写さない)。
pub fn envelope_to_json(e: &Envelope) -> Json {
    let mut m = Map::new();
    m.insert("id".into(), Json::String(e.id.to_string()));
    m.insert("kind".into(), Json::String(e.kind.clone()));
    m.insert("v".into(), Json::Number(e.v.into()));
    m.insert(
        "created_at".into(),
        Json::Number(e.created_at.as_millis().into()),
    );
    m.insert(
        "updated_at".into(),
        Json::Number(e.updated_at.as_millis().into()),
    );
    m.insert("writer".into(), Json::String(e.writer.clone()));
    m.insert("body".into(), to_json(&e.body));
    if !e.unknown.is_empty() {
        // 封筒の知らない項目は、元の形のまま運ぶ (03 §4.2 の規則 7)。
        m.insert("unknown".into(), wrap(&Cbor::Map(e.unknown.clone())));
    }
    Json::Object(m)
}

/// JSON から封筒に戻す。
pub fn envelope_from_json(v: &Json) -> Result<Envelope, String> {
    let get = |k: &str| v.get(k).ok_or_else(|| format!("missing `{k}`"));
    let text = |k: &str| -> Result<String, String> {
        get(k)?
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("`{k}` is not text"))
    };
    let int = |k: &str| -> Result<i64, String> {
        get(k)?
            .as_i64()
            .ok_or_else(|| format!("`{k}` is not an integer"))
    };
    let id: Id = text("id")?.parse().map_err(|e| format!("{e}"))?;
    let unknown = match v.get("unknown") {
        None => Vec::new(),
        Some(u) => match from_json(u)? {
            Cbor::Map(m) => m,
            _ => return Err("`unknown` is not a map".into()),
        },
    };
    Ok(Envelope {
        id,
        kind: text("kind")?,
        v: u32::try_from(int("v")?).map_err(|_| "`v` out of range")?,
        created_at: Timestamp::from_millis(int("created_at")?),
        updated_at: Timestamp::from_millis(int("updated_at")?),
        writer: text("writer")?,
        body: from_json(get("body")?)?,
        unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for n in 0..10usize {
            let data: Vec<u8> = (0..n as u8).map(|b| b.wrapping_mul(37)).collect();
            assert_eq!(b64_decode(&b64_encode(&data)).unwrap(), data);
        }
        assert_eq!(b64_encode(b"Man"), "TWFu");
        assert_eq!(b64_encode(b"Ma"), "TWE=");
    }

    #[test]
    fn values_without_a_json_form_round_trip_through_the_mark() {
        let v = Cbor::Map(vec![
            ("text".into(), "日本語".into()),
            ("bytes".into(), Cbor::Bytes(vec![0, 1, 255])),
            ("big".into(), Cbor::Integer(u64::MAX.into())),
            ("nan".into(), Cbor::Float(f64::NAN)),
            ("float".into(), Cbor::Float(0.7)),
            ("arr".into(), Cbor::Array(vec![1.into(), Cbor::Null])),
            ("intkeys".into(), Cbor::Map(vec![(1.into(), "x".into())])),
            ("tag".into(), Cbor::Tag(1, Box::new(2.into()))),
        ]);
        let j = to_json(&v);
        assert_eq!(j["text"], "日本語");
        assert!(j["bytes"].get(MARK).is_some());
        let back = from_json(&j).unwrap();
        let Cbor::Map(m) = back else { panic!() };
        let get = |k: &str| {
            m.iter()
                .find(|(kk, _)| kk.as_text() == Some(k))
                .unwrap()
                .1
                .clone()
        };
        assert_eq!(get("bytes"), Cbor::Bytes(vec![0, 1, 255]));
        assert_eq!(get("big"), Cbor::Integer(u64::MAX.into()));
        assert!(matches!(get("nan"), Cbor::Float(f) if f.is_nan()));
        assert_eq!(get("float"), Cbor::Float(0.7));
        assert_eq!(get("intkeys"), Cbor::Map(vec![(1.into(), "x".into())]));
        assert_eq!(get("tag"), Cbor::Tag(1, Box::new(2.into())));
    }

    #[test]
    fn envelopes_round_trip() {
        let mut e = Envelope::new(
            Id::from_bytes([3; 16]),
            "turn",
            1,
            Timestamp::from_millis(5),
            "0.0.4",
        );
        e.body = Cbor::Map(vec![("text".into(), "本文".into())]);
        e.unknown.push(("future".into(), Cbor::Bytes(vec![9])));
        let back = envelope_from_json(&envelope_to_json(&e)).unwrap();
        assert_eq!(back, e);
    }
}
