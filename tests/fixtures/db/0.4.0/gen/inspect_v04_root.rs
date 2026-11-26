//! `gen_v04_root` で作ったデータ根を、何も書かずに v0.4.0 の点検 (`evorag_store::inspect`) で読み、
//! 見本の README に書く数と中身の要約を出す。
//!
//! ```text
//! cargo run -p evorag-core --example inspect_v04_root --locked -- <データ根>
//! ```

use std::path::PathBuf;

use evorag_store::inspect::{DbState, inspect_collect};
use serde_json::Value;

const KINDS: &[&str] = &[
    "session",
    "turn",
    "fact",
    "fact_directive",
    "signal",
    "recall_feedback",
    "setting",
    "model_binding",
    "threshold",
    "note",
    "job",
];

fn field<'a>(v: &'a Value, name: &str) -> &'a Value {
    v.get(name).unwrap_or(&Value::Null)
}

fn main() {
    let root: PathBuf = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: inspect_v04_root <data root>");
    let ins = inspect_collect(&root, evorag_memory::kinds(), None, KINDS)
        .expect("the data root is inspected");
    assert_eq!(ins.db, DbState::Opened, "{:?}", ins.db);
    println!("## meta");
    for (k, v) in &ins.meta {
        let shown = if k == evorag_store::meta::FORGET_KEY {
            "(64 hex)"
        } else {
            v.as_str()
        };
        println!("{k} = {shown}");
    }
    println!("## census");
    for (k, c) in &ins.census.known {
        println!("{k}: {c:?}");
    }
    println!(
        "unknown: {:?} bad_keys: {}",
        ins.census.unknown, ins.census.bad_keys
    );
    println!("## journal\n{:?}", ins.journal);
    for kind in KINDS {
        println!("## {kind}");
        for e in ins.collected.get(*kind).into_iter().flatten() {
            let b: Value = e.body_as().expect("a body decodes");
            let line = match *kind {
                "turn" => format!(
                    "session={} role={} state={} rev={} tz={} reply_to={} attached={} text={}",
                    field(&b, "session"),
                    field(&b, "role"),
                    field(&b, "state"),
                    field(&b, "rev"),
                    b.pointer("/obs/tz_offset").unwrap_or(&Value::Null),
                    field(&b, "reply_to"),
                    field(&b, "attached"),
                    field(&b, "text"),
                ),
                "fact" => format!(
                    "attr={} value={} state={} private={} by={} sources={}",
                    field(&b, "attribute"),
                    field(&b, "value"),
                    field(&b, "state"),
                    field(&b, "private"),
                    field(&b, "by"),
                    field(&b, "sources"),
                ),
                "fact_directive" => format!(
                    "op={} attr={} scope={} target={} redact={} history={} turns={} redactions={} hidden={} marks={} origin={}",
                    field(&b, "op"),
                    field(&b, "attribute"),
                    field(&b, "scope"),
                    field(&b, "target"),
                    field(&b, "redact"),
                    field(&b, "history"),
                    field(&b, "turns"),
                    field(&b, "redactions").as_array().map_or(0, Vec::len),
                    field(&b, "hidden").as_array().map_or(0, Vec::len),
                    field(&b, "marks").as_array().map_or(0, Vec::len),
                    field(&b, "origin"),
                ),
                _ => b.to_string(),
            };
            println!("{} v{} {line}", e.id, e.v);
        }
    }
}
