//! 状態のファイル (`update/state.json`、12 §7.2 の「状態のファイル」)。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::names::Op;
use crate::Sha256Digest;

/// 状態のファイルの形式の版。
pub(crate) const STATE_SCHEMA: u64 = 1;

/// 役割 (`current`・`next`・`old_prev`) のファイル。役割を見分けるのは SHA-256 だけで、版は示すためだけ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    /// SHA-256 (小文字の 16 進)。
    #[serde(with = "digest_hex")]
    pub sha256: Sha256Digest,
    /// 版 (分かれば)。
    #[serde(default)]
    pub version: Option<String>,
    /// DB の形式の版 (分かれば)。
    #[serde(default)]
    pub db_format: Option<String>,
}

/// 計画、または最後に済んだ操作の記録。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Plan {
    pub op: Op,
    /// インストール先のフォルダ (正規化したパス)。
    pub dir: String,
    /// 済んだ手順の番号 (12 §7.3 の表の番号。1 から)。
    pub stage: u32,
    /// 元に戻している途中か。
    pub reverting: bool,
    pub current: Role,
    pub next: Role,
    #[serde(default)]
    pub old_prev: Option<Role>,
}

/// 状態のファイルの中身。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct State {
    pub plan: Option<Plan>,
    pub done: Option<Plan>,
}

#[derive(Serialize)]
struct Stored<'a> {
    schema: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<&'a Plan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    done: Option<&'a Plan>,
}

/// 計画が読めない理由 (表示用の英語)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unreadable(pub String);

/// 状態のファイルの場所。
#[derive(Debug, Clone)]
pub(crate) struct StatePaths {
    pub dir: PathBuf,
    pub file: PathBuf,
    pub temp: PathBuf,
    pub lock: PathBuf,
}

impl StatePaths {
    pub fn new(state_dir: &Path) -> Self {
        let dir = state_dir.join("update");
        Self {
            file: dir.join("state.json"),
            temp: dir.join("state.json.tmp"),
            lock: dir.join("lock"),
            dir,
        }
    }
}

/// 状態のファイルを読む (12 §7.2 の「読めないとき」)。
///
/// - ファイルが無い: 計画も記録も無い。
/// - `plan` が無いことが読めて、`done` が読めない・`schema` を知らない: 記録が無いものとして扱う。
/// - それ以外の読めない形 (JSON でない、`schema` を知らない版で `plan` がある、`plan` が欠ける): [`Unreadable`]。
pub(crate) fn read(path: &Path) -> Result<State, Unreadable> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
        Err(e) => return Err(Unreadable(format!("cannot read the state file: {e}"))),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| Unreadable(format!("the state file is not JSON: {e}")))?;
    let serde_json::Value::Object(map) = value else {
        return Err(Unreadable("the state file is not a JSON object".to_owned()));
    };
    let present = |key: &str| map.get(key).filter(|v| !v.is_null());
    let known_schema = map.get("schema").and_then(serde_json::Value::as_u64) == Some(STATE_SCHEMA);
    let plan = match present("plan") {
        None => None,
        Some(_) if !known_schema => {
            return Err(Unreadable(
                "the state file has a plan in an unknown schema".to_owned(),
            ));
        }
        Some(v) => Some(
            Plan::deserialize(v)
                .map_err(|e| Unreadable(format!("the plan cannot be read: {e}")))?,
        ),
    };
    let done = if known_schema {
        present("done").and_then(|v| Plan::deserialize(v).ok())
    } else {
        None
    };
    Ok(State { plan, done })
}

/// 状態のファイルを書く。一時名に書いて書き切り、改名で置く。`before_rename` は一時名を書き切った後、
/// 改名の前に呼ぶ (試験の強制終了の点)。
pub(crate) fn write(
    paths: &StatePaths,
    plan: Option<&Plan>,
    done: Option<&Plan>,
    before_rename: impl FnOnce() -> Result<(), super::SwapError>,
) -> Result<(), super::SwapError> {
    use super::SwapError;
    let io = |what: &'static str, path: &Path| {
        let path = path.to_path_buf();
        move |source| SwapError::Io { what, path, source }
    };
    let body = serde_json::to_vec_pretty(&Stored {
        schema: STATE_SCHEMA,
        plan,
        done,
    })
    .expect("the state serializes to JSON");
    match std::fs::remove_file(&paths.temp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(io("remove", &paths.temp)(e));
        }
        _ => {}
    }
    let mut file = evorag_store::fsutil::create_new_file(&paths.temp, 0o600)
        .map_err(io("create", &paths.temp))?;
    file.write_all(&body).map_err(io("write", &paths.temp))?;
    file.sync_all().map_err(io("sync", &paths.temp))?;
    drop(file);
    before_rename()?;
    evorag_store::fsutil::rename_durable(&paths.temp, &paths.file)
        .map_err(io("rename", &paths.file))?;
    Ok(())
}

mod digest_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::Sha256Digest;

    pub fn serialize<S: Serializer>(d: &Sha256Digest, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(d)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Sha256Digest, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(tag: &[u8]) -> Role {
        Role {
            sha256: Sha256Digest::of(tag),
            version: Some("0.3.0".to_owned()),
            db_format: None,
        }
    }

    fn plan() -> Plan {
        Plan {
            op: Op::Apply,
            dir: "/x".to_owned(),
            stage: 1,
            reverting: false,
            current: role(b"c"),
            next: role(b"n"),
            old_prev: None,
        }
    }

    fn read_text(text: &str) -> Result<State, Unreadable> {
        let dir = evorag_store::testutil::TempDir::new("state-read");
        let path = dir.path().join("state.json");
        std::fs::write(&path, text).expect("write");
        read(&path)
    }

    #[test]
    fn a_missing_file_has_no_plan_and_no_record() {
        let dir = evorag_store::testutil::TempDir::new("state-missing");
        assert_eq!(read(&dir.path().join("state.json")), Ok(State::default()));
    }

    #[test]
    fn plans_round_trip_and_unknown_fields_are_ignored() {
        let dir = evorag_store::testutil::TempDir::new("state-rt");
        let paths = StatePaths::new(dir.path());
        std::fs::create_dir_all(&paths.dir).expect("mkdir");
        let state = State {
            plan: Some(plan()),
            done: Some(plan()),
        };
        write(&paths, state.plan.as_ref(), state.done.as_ref(), || Ok(())).expect("write");
        assert_eq!(read(&paths.file), Ok(state.clone()));
        assert!(!paths.temp.exists());

        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&paths.file).expect("read")).expect("json");
        v["future"] = serde_json::json!({"x": 1});
        v["plan"]["future"] = serde_json::json!(true);
        std::fs::write(&paths.file, v.to_string()).expect("write");
        assert_eq!(read(&paths.file), Ok(state));
    }

    #[test]
    fn an_unreadable_done_or_an_unknown_schema_without_a_plan_is_no_record() {
        let s = read_text(r#"{"schema":1,"done":{"op":"apply"}}"#).expect("readable");
        assert_eq!(s, State::default());
        let s = read_text(r#"{"schema":2,"done":{"x":1}}"#).expect("readable");
        assert_eq!(s, State::default());
        let s = read_text(r#"{"schema":1,"plan":null}"#).expect("readable");
        assert_eq!(s, State::default());
    }

    #[test]
    fn broken_plans_are_unreadable() {
        let p = serde_json::to_value(plan()).expect("json");
        assert!(read_text("{not json").is_err());
        assert!(read_text("[]").is_err());
        assert!(read_text(&serde_json::json!({"schema": 2, "plan": p}).to_string()).is_err());
        assert!(read_text(&serde_json::json!({"plan": p}).to_string()).is_err());
        let mut missing = p.clone();
        missing.as_object_mut().expect("object").remove("current");
        assert!(read_text(&serde_json::json!({"schema": 1, "plan": missing}).to_string()).is_err());
        let mut bad = p;
        bad["next"]["sha256"] = serde_json::json!("XYZ");
        assert!(read_text(&serde_json::json!({"schema": 1, "plan": bad}).to_string()).is_err());
    }
}
