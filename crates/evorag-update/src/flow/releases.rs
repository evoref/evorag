//! リリースの一覧から更新の先の版を選ぶ (12 §7.2 の apply の 1)。

use crate::Version;

/// 一覧を読めない (JSON の配列でない)。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the release list is not a JSON array")]
pub struct ListUnreadable;

/// 一覧 (GitHub の API の応答) から、いちばん大きい版を選ぶ。
///
/// `tag_name` が `v<semver>` にちょうど一致しないもの・`draft`・`prerelease`・semver の pre-release
/// (`-` の付く版、[`Version`] が読まない) を除き、semver で比べる (一覧の並びの順には頼らない)。
/// `draft`・`prerelease` が真偽でないものも除く。どれも残らなければ `None`。一覧の値は版を選ぶため
/// だけに使い、示さない。
///
/// # Errors
///
/// JSON の配列でない。
pub fn latest_version(list: &[u8]) -> Result<Option<Version>, ListUnreadable> {
    let value: serde_json::Value = serde_json::from_slice(list).map_err(|_| ListUnreadable)?;
    let items = value.as_array().ok_or(ListUnreadable)?;
    Ok(items
        .iter()
        .filter(|r| {
            r.get("draft").and_then(serde_json::Value::as_bool) == Some(false)
                && r.get("prerelease").and_then(serde_json::Value::as_bool) == Some(false)
        })
        .filter_map(|r| r.get("tag_name")?.as_str())
        .filter_map(|tag| Version::from_tag(tag).ok())
        .max())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[(&str, bool, bool)]) -> Vec<u8> {
        serde_json::to_vec(
            &items
                .iter()
                .map(|(tag, draft, pre)| {
                    serde_json::json!({ "tag_name": tag, "draft": draft, "prerelease": pre })
                })
                .collect::<Vec<_>>(),
        )
        .expect("json")
    }

    #[test]
    fn the_largest_semver_is_chosen_regardless_of_order() {
        let l = list(&[
            ("v0.3.0", false, false),
            ("v0.10.0", false, false),
            ("v0.9.9", false, false),
        ]);
        assert_eq!(latest_version(&l), Ok(Some(Version::new(0, 10, 0))));
    }

    #[test]
    fn drafts_prereleases_and_odd_tags_are_skipped() {
        let l = list(&[
            ("v0.3.0", false, false),
            ("v0.9.0", true, false),
            ("v0.8.0", false, true),
            ("v0.7.0-rc.1", false, false),
            ("0.6.0", false, false),
            ("v0.5.0 ", false, false),
            ("v0.5", false, false),
            ("v01.0.0", false, false),
            ("release-v0.4.0", false, false),
        ]);
        assert_eq!(latest_version(&l), Ok(Some(Version::new(0, 3, 0))));
        // draft・prerelease が無い・真偽でないものも除く。
        let odd = br#"[{"tag_name":"v1.0.0"},{"tag_name":"v2.0.0","draft":"no","prerelease":false},{"tag_name":7,"draft":false,"prerelease":false}]"#;
        assert_eq!(latest_version(odd), Ok(None));
        assert_eq!(latest_version(b"[]"), Ok(None));
    }

    #[test]
    fn a_list_that_is_not_an_array_is_refused() {
        assert_eq!(latest_version(b"{\"message\":\"x\"}"), Err(ListUnreadable));
        assert_eq!(latest_version(b"<html>"), Err(ListUnreadable));
    }
}
