//! ノート (話題の検索の鍵) を作る sleep-time の手順 (04 §2、04 §4 の手順 2、FR-303)。
//!
//! 区切り (`evorag_memory::segments`) と記録したノートを突き合わせ、区切りがついたのにノートが無い
//! (往復の一覧か作り方の版が違う) 区切りについて、型縛りの補助の呼び出しで鍵を作って書く。進み具合は
//! 状態から求めるので job を書かない。1 つのノートは 1 回の呼び出しと 1 回の書き込みで作る。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use evorag_llm::{ChatMessage, ChatRequest, Generation, LlmClient, Role};
use evorag_memory::{
    KIND_NOTE, KIND_SESSION, NOTE_RULES, NoteBody, REL_EMBEDDING_OF, Segment, sanitize_keys,
};
use evorag_store::{Read, Store};
use evorag_types::Id;
use serde_json::json;

use crate::Inner;

/// ノートの手順の状態 (このプロセスの間だけ)。
#[derive(Default)]
pub(crate) struct State {
    /// 会話が変わるたびに増える。突き合わせ終えた版と違えば、もう一度突き合わせる。
    version: u64,
    clean: Option<u64>,
    /// 最後の区切りに区切りがつく、いちばん早い時刻 (ミリ秒)。
    due_at: Option<i64>,
    /// 型が合わずにやり直しても合わなかった区切り (往復の一覧)。このプロセスの間は飛ばす。
    skip: HashSet<Vec<Id>>,
}

/// 会話が変わった (ターンを書いた、セッションを消した)。
pub(crate) fn touch(inner: &Inner) {
    inner
        .notes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .version += 1;
}

/// 今の応答のモデルで、型縛りの補助の呼び出しを使えるか (06 §3.3 の点検で通ったか)。
/// 点検がまだなら使わない (点検を待つ)。
fn usable(inner: &Inner) -> bool {
    let (_, server, _) = inner.conn();
    inner
        .store
        .read()
        .ok()
        .and_then(|r| {
            evorag_memory::load_probe(
                &r,
                evorag_memory::ROLE_CHAT,
                &server.model_key,
                &server.build_info,
            )
            .ok()
        })
        .flatten()
        .is_some_and(|(_, p)| p.structured_ok == Some(true) && p.thinking_off != Some(false))
}

/// 要るか (状態から求める。突き合わせていない変更があるか、区切りがつく時刻が来たか)。
pub(crate) fn needed(inner: &Inner) -> bool {
    let due = {
        let st = inner.notes.lock().unwrap_or_else(|e| e.into_inner());
        st.clean != Some(st.version)
            || st
                .due_at
                .is_some_and(|t| inner.clock.now().as_millis() >= t)
    };
    due && usable(inner)
}

/// 作る (作り直す) ノート 1 つ分。
struct Work {
    session: Id,
    segment: Segment,
    /// 同じ区切りの往復を指す前のノートと、その鍵の埋め込み (書くときに消す)。
    old: Vec<Id>,
    old_embeddings: Vec<Id>,
}

struct Plan {
    work: Vec<Work>,
    /// 出典のセッションの無いノート (古い版が消したセッションなど、03 §4.2 の規則 12) と、その埋め込み。
    orphans: Vec<Id>,
    orphan_embeddings: Vec<Id>,
    due_at: Option<i64>,
}

fn embeddings_of(r: &evorag_store::ReadTxn<'_>, note: Id) -> Vec<Id> {
    r.links_to(note, REL_EMBEDDING_OF)
        .map(|l| l.into_iter().map(|(id, _)| id).collect())
        .unwrap_or_default()
}

/// 区切りと記録したノートを突き合わせる。
fn plan(store: &Store, now_ms: i64) -> Option<Plan> {
    let r = store.read().ok()?;
    let mut notes: HashMap<Id, Vec<(Id, NoteBody)>> = HashMap::new();
    r.for_each(KIND_NOTE, |x| {
        if let Read::Ok(e) = x
            && let Ok(b) = e.body_as::<NoteBody>()
        {
            notes.entry(b.session).or_default().push((e.id, b));
        }
        true
    })
    .ok()?;
    let mut p = Plan {
        work: Vec::new(),
        orphans: Vec::new(),
        orphan_embeddings: Vec::new(),
        due_at: None,
    };
    for (session, list) in &notes {
        if !matches!(r.get(KIND_SESSION, *session), Ok(Some(Read::Ok(_)))) {
            for (id, _) in list {
                p.orphans.push(*id);
                p.orphan_embeddings.extend(embeddings_of(&r, *id));
            }
        }
    }
    for s in evorag_memory::list_sessions(&r, usize::MAX).ok()? {
        let sid = s.envelope.id;
        let turns = evorag_memory::session_turns(&r, sid).unwrap_or_default();
        let have = notes.get(&sid).map(Vec::as_slice).unwrap_or_default();
        for seg in evorag_memory::segments(&turns, now_ms) {
            let ids = seg.ids();
            if have
                .iter()
                .any(|(_, n)| n.exchanges == ids && n.rules == NOTE_RULES)
            {
                continue;
            }
            if !seg.closed {
                p.due_at = match (p.due_at, seg.closes_at) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                continue;
            }
            let set: HashSet<Id> = ids.iter().copied().collect();
            let old: Vec<Id> = have
                .iter()
                .filter(|(_, n)| n.exchanges.iter().any(|x| set.contains(x)))
                .map(|(id, _)| *id)
                .collect();
            let old_embeddings = old.iter().flat_map(|n| embeddings_of(&r, *n)).collect();
            p.work.push(Work {
                session: sid,
                segment: seg,
                old,
                old_embeddings,
            });
        }
    }
    Some(p)
}

/// 鍵を作る呼び出しの答え (`{"keys": [文字列, …]}`) を型で確かめて読む。
fn parse_keys(text: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    v.get("keys")?
        .as_array()?
        .iter()
        .map(|k| k.as_str().map(str::to_owned))
        .collect()
}

enum KeysError {
    /// 繋がらない・断られた (後で試し直す)。
    Failed,
    /// 型に合わない答えが 2 回続いた (この区切りは飛ばす)。
    Invalid,
}

/// 応答のスロット (0) が処理中か (06 §4)。`/slots` が使えなければ見ない。
async fn response_slot_busy(llm: &dyn LlmClient) -> bool {
    llm.slots()
        .await
        .is_ok_and(|s| s.iter().any(|x| x.id == 0 && x.is_processing))
}

async fn make_keys(
    inner: &Inner,
    llm: &dyn LlmClient,
    slot: Option<u32>,
    seg: &Segment,
) -> Result<Vec<String>, KeysError> {
    let mut extra = inner.aux_request_extra.clone();
    extra.insert(
        "response_format".into(),
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": "note_keys",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {
                        "keys": {
                            "type": "array",
                            "items": {"type": "string"},
                            "maxItems": evorag_memory::NOTE_KEYS
                        }
                    },
                    "required": ["keys"],
                    "additionalProperties": false
                }
            }
        }),
    );
    let req = ChatRequest {
        messages: vec![
            ChatMessage::new(Role::System, crate::prompt::note_system(inner.language)),
            ChatMessage::new(Role::User, crate::prompt::note_input(inner.language, seg)),
        ],
        // 冪等にする: temperature 0・固定の seed・固定のスロット (06 §6)。
        generation: Generation {
            temperature: 0.0,
            top_p: 1.0,
            max_tokens: Some(256),
            seed: Some(1),
        },
        id_slot: slot,
        model: None,
        extra,
    };
    // 型に合わなければ 1 回だけやり直す (06 §6)。
    for _ in 0..2 {
        let c = crate::probe::run_chat(llm, req.clone())
            .await
            .map_err(|_| KeysError::Failed)?;
        if let Some(keys) = parse_keys(&c.text) {
            return Ok(sanitize_keys(&keys));
        }
    }
    Err(KeysError::Invalid)
}

/// 手順を動かす。1 つでもノートを書けたら `true`。
pub(crate) async fn run(inner: &Arc<Inner>) -> bool {
    let version = inner
        .notes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .version;
    let store = inner.store.clone();
    let now = inner.clock.now().as_millis();
    let Ok(Some(plan)) = tokio::task::spawn_blocking(move || plan(&store, now)).await else {
        return false;
    };
    inner.notes.lock().unwrap_or_else(|e| e.into_inner()).due_at = plan.due_at;
    let mut progressed = false;
    if !plan.orphans.is_empty() {
        let w = crate::writes(inner);
        let mut ops = Vec::new();
        for e in &plan.orphan_embeddings {
            ops.extend(w.delete_embedding(*e));
        }
        for n in &plan.orphans {
            ops.push(evorag_store::WriteOp::Delete {
                kind: KIND_NOTE.into(),
                id: *n,
            });
            ops.push(evorag_store::WriteOp::DeleteLinksOf(*n));
        }
        if crate::write_derived(inner, ops).await.is_err() {
            return false;
        }
        progressed = true;
    }
    let (llm, server, _) = inner.conn();
    // 裏の処理は最後のスロット (06 §4)。
    let slot = (server.total_slots > 1).then(|| server.total_slots - 1);
    for w in plan.work {
        let ids = w.segment.ids();
        if inner
            .notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .skip
            .contains(&ids)
        {
            continue;
        }
        if response_slot_busy(&*llm).await {
            return progressed;
        }
        let keys = match make_keys(inner, &*llm, slot, &w.segment).await {
            Ok(k) => k,
            Err(KeysError::Invalid) => {
                tracing::warn!(exchanges = ids.len(), "note keys did not match the schema");
                inner
                    .notes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .skip
                    .insert(ids);
                continue;
            }
            Err(KeysError::Failed) => return progressed,
        };
        let body = NoteBody {
            session: w.session,
            exchanges: ids,
            keys,
            model_key: server.model_key.clone(),
            rules: NOTE_RULES,
        };
        let Ok((_, ops)) = crate::writes(inner).note(&w.old, &w.old_embeddings, &body) else {
            return progressed;
        };
        if crate::write_derived(inner, ops).await.is_err() {
            return progressed;
        }
        progressed = true;
    }
    inner.notes.lock().unwrap_or_else(|e| e.into_inner()).clean = Some(version);
    progressed
}
