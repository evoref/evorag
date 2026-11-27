//! 保存した値の中の文字列を集める (台帳の掃除で、型の決まらない値に忘れた値の写しを残さないため、04 §7.2)。

use std::collections::BTreeMap;
use std::fmt;

use evorag_store::RawValue;
use serde::Deserialize;
use serde::de::{self, Deserializer, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};

/// 値の中のすべての文字列 (map の項目の名前・値、並びの要素、CBOR の tag の中身を辿る)。
#[derive(Debug, Default)]
pub(crate) struct AllStrings(pub(crate) Vec<String>);

impl<'de> Deserialize<'de> for AllStrings {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(StringsVisitor)
    }
}

struct StringsVisitor;

impl<'de> Visitor<'de> for StringsVisitor {
    type Value = AllStrings;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any value")
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_i128<E: de::Error>(self, _: i128) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_u128<E: de::Error>(self, _: u128) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_char<E: de::Error>(self, v: char) -> Result<AllStrings, E> {
        Ok(AllStrings(vec![v.to_string()]))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<AllStrings, E> {
        Ok(AllStrings(vec![v.to_owned()]))
    }

    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<AllStrings, E> {
        // 文字列として読めるバイト列は文字列として見る (値の写しを残さない側)。
        Ok(AllStrings(vec![String::from_utf8_lossy(v).into_owned()]))
    }

    fn visit_none<E: de::Error>(self) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_unit<E: de::Error>(self) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<AllStrings, D::Error> {
        AllStrings::deserialize(d)
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(self, d: D) -> Result<AllStrings, D::Error> {
        AllStrings::deserialize(d)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<AllStrings, A::Error> {
        let mut out = Vec::new();
        while let Some(AllStrings(x)) = seq.next_element()? {
            out.extend(x);
        }
        Ok(AllStrings(out))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<AllStrings, A::Error> {
        let mut out = Vec::new();
        while let Some((AllStrings(k), AllStrings(v))) = map.next_entry()? {
            out.extend(k);
            out.extend(v);
        }
        Ok(AllStrings(out))
    }

    /// CBOR の tag (ciborium は tag を enum として渡す) と enum の値: 中身の文字列を集める。
    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<AllStrings, A::Error> {
        let (de::IgnoredAny, variant) = data.variant()?;
        variant.newtype_variant::<AllStrings>()
    }
}

/// 要素 `raw` の文字列。map の要素は、`skip` の項目 (列挙・番号・型として読んだ項目) を除いた項目の名前と値の
/// 文字列。集められなければ `None` (呼び手は、当たったとみなして除く)。
pub(crate) fn element_strings(raw: &RawValue, skip: &[&str]) -> Option<Vec<String>> {
    if let Ok(map) = raw.to::<BTreeMap<String, AllStrings>>() {
        return Some(
            map.into_iter()
                .filter(|(k, _)| !skip.contains(&k.as_str()))
                .flat_map(|(k, v)| std::iter::once(k).chain(v.0))
                .collect(),
        );
    }
    raw.to::<AllStrings>().ok().map(|s| s.0)
}

/// 保存した値 `raw` のすべての文字列 (集められなければ `None`)。
pub(crate) fn all_strings(raw: &RawValue) -> Option<Vec<String>> {
    raw.to::<AllStrings>().ok().map(|s| s.0)
}
