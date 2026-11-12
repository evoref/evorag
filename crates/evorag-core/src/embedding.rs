//! 埋め込みとベクトルの索引 (05 §2.2・§2.3、04 §4 の手順 1、FR-410)。
//!
//! - 埋め込みのモデル (embed の役割、06 §3) ごとに、`index/memory/vec-<model_key>/` に索引を持つ。
//!   今と前の世代だけを残す。
//! - 裏の処理の手順: 語彙の索引にあってベクトルの索引に無いチャンクを、32 件ずつ埋め込み、記録
//!   (`embedding`、ベクトルは `blobs`) と索引に足す。進み具合は状態 (索引にある ID) から求めるので、
//!   止めても強制終了しても、次の機会に続きから作る (job は書かない)。
//! - 記録と索引への反映は、落とされない task で行う (記録だけが残って、同じチャンクを 2 度埋め込まないため)。
//! - 起動時と役割が繋がったときに、索引を DB の記録と突き合わせる (壊れていれば作り直す)。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use evorag_index::lex::Opened;
use evorag_index::vec::{Unit, VecIndex, VecInput};
use evorag_llm::LlmClient;
use evorag_memory::{ChunkBody, EmbeddingBody, KIND_CHUNK, KIND_TURN, TurnBody, UNIT_CHUNK};
use evorag_store::Read;
use evorag_types::Id;

use crate::{Inner, roles, write_derived, writes};

/// 1 回に埋め込むチャンクの数。
const BATCH: usize = 32;
/// 1 回の埋め込みの要求の締切。
const LIMIT: Duration = Duration::from_secs(60);
/// これより多く足りないときは、状態の行に「作り直し中」と出す (ターンごとの数件では出さない)。
const REBUILD_SHOWN_AT: usize = BATCH;

/// 使っているベクトルの索引。
pub(crate) struct Vectors {
    pub key: String,
    pub index: Arc<VecIndex>,
}

/// 索引の置き場所。
pub(crate) fn memory_dir(root: &std::path::Path) -> std::path::PathBuf {
    root.join("index").join("memory")
}

fn vec_dir(root: &std::path::Path, key: &str) -> std::path::PathBuf {
    memory_dir(root).join(format!("vec-{key}"))
}

/// 使える埋め込みのモデル (接続と model_key)。
pub(crate) fn embedder(inner: &Inner) -> Option<(Arc<dyn LlmClient>, String)> {
    let aux = inner.aux.lock().unwrap_or_else(|e| e.into_inner());
    let a = aux.iter().find(|a| a.role == evorag_memory::ROLE_EMBED)?;
    match (&a.state, a.ready_client()) {
        (roles::RoleState::Ready(o), Some(c)) => Some((c, o.model_key.clone())),
        _ => None,
    }
}

/// 前の世代の埋め込みのモデル (記録の履歴の、今の前のもの)。
fn previous_key(inner: &Inner, current: &str) -> Option<String> {
    let r = inner.store.read().ok()?;
    let b = evorag_memory::load_bindings(&r).ok()?;
    b.by_role
        .get(evorag_memory::ROLE_EMBED)?
        .body
        .history
        .iter()
        .rev()
        .map(|o| o.model_key.clone())
        .find(|k| k != current)
}

/// 今の埋め込みのモデルのベクトルの索引 (まだ開いていなければ `None`)。
pub(crate) fn current(inner: &Inner) -> Option<(String, Arc<VecIndex>)> {
    inner
        .vectors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|v| (v.key.clone(), v.index.clone()))
}

/// 足りないチャンク (語彙の索引にあって、今のモデルのベクトルの索引に無いもの) と、全体の数。
fn missing(inner: &Inner, key: &str) -> (Vec<Id>, usize) {
    let Some(lex) = &inner.index else {
        return (Vec::new(), 0);
    };
    let all = lex.snapshot().ids();
    let have: HashSet<Id> = match current(inner) {
        Some((k, ix)) if k == key => ix.snapshot().ids(),
        _ => HashSet::new(),
    };
    let mut out: Vec<Id> = all.difference(&have).copied().collect();
    out.sort();
    (out, all.len())
}

/// 埋め込みを作る手順が要るか。索引を DB と突き合わせる前は動かさない (同じチャンクを 2 度埋め込まないため)。
pub(crate) fn needed(inner: &Inner) -> bool {
    embedder(inner).is_some_and(|(_, key)| {
        inner
            .vectors_opened
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
            == Some(key.as_str())
            && !missing(inner, &key).0.is_empty()
    })
}

/// 索引を DB と突き合わせ終えた (埋め込みの手順を動かしてよい)。
fn mark_opened(inner: &Inner, key: &str) {
    *inner
        .vectors_opened
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(key.to_owned());
}

/// 作り直しの進み具合 (足りないものが多いときだけ。百分率)。
pub(crate) fn rebuilding(inner: &Inner) -> Option<u8> {
    let (_, key) = embedder(inner)?;
    let (gap, total) = missing(inner, &key);
    (gap.len() > REBUILD_SHOWN_AT && total > 0)
        .then(|| (((total - gap.len()) * 100) / total).min(99) as u8)
}

/// 索引を開き、DB の記録と突き合わせる。前の世代より古い索引と記録は消す (05 §2.2)。
pub(crate) async fn open(inner: &Arc<Inner>) {
    let Some((_, key)) = embedder(inner) else {
        return;
    };
    let previous = previous_key(inner, &key);
    let root = inner.store.data_root().to_path_buf();
    let worker = inner.clone();
    let key2 = key.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        let mut keep = vec![key2.as_str()];
        if let Some(p) = &previous {
            keep.push(p.as_str());
        }
        evorag_index::vec::prune_generations(&memory_dir(&root), &keep);
        let r = worker.store.read().ok()?;
        let all = evorag_memory::embeddings_of_model(&r, None).ok()?;
        let chunks: HashSet<Id> = worker
            .index
            .as_ref()
            .map(|ix| ix.snapshot().ids())
            .unwrap_or_default();
        // 対象のチャンクが無い埋め込みと、前の世代より古いモデルの埋め込みは消す。
        let mut stale = Vec::new();
        let mut mine = Vec::new();
        for (id, b, v) in all {
            let kept_model =
                b.model_key == key2 || previous.as_deref() == Some(b.model_key.as_str());
            if !kept_model || (!chunks.is_empty() && !chunks.contains(&b.target)) {
                stale.push(id);
            } else if b.model_key == key2 && v.len() == b.dim as usize {
                mine.push((b, v));
            }
        }
        Some((stale, mine))
    })
    .await
    .ok()
    .flatten();
    let Some((stale, mine)) = loaded else {
        return;
    };
    if !stale.is_empty() {
        let w = writes(inner);
        let ops: Vec<_> = stale
            .iter()
            .flat_map(|id| w.delete_embedding(*id))
            .collect();
        let _ = write_derived(inner, ops).await;
    }
    let Some(dim) = mine.first().map(|(b, _)| b.dim as usize) else {
        // まだ 1 件も無い。最初の埋め込みのときに開く。
        mark_opened(inner, &key);
        return;
    };
    let dir = vec_dir(inner.store.data_root(), &key);
    let worker = inner.clone();
    let opened = tokio::task::spawn_blocking(move || -> std::io::Result<Arc<VecIndex>> {
        let (ix, opened) = VecIndex::open(&dir, dim)?;
        let want: HashSet<Id> = mine.iter().map(|(b, _)| b.target).collect();
        let have = ix.snapshot().ids();
        if opened != Opened::Ok || have != want {
            tracing::info!(rows = want.len(), "vector index rebuilt from the database");
            let extra: HashSet<Id> = have.difference(&want).copied().collect();
            if !extra.is_empty() {
                ix.remove(&extra)?;
            }
            let add: Vec<VecInput> = mine
                .into_iter()
                .filter(|(b, _)| opened != Opened::Ok || !have.contains(&b.target))
                .map(|(b, v)| VecInput {
                    id: b.target,
                    session: b.session,
                    unit: Unit::Chunk,
                    vector: v,
                })
                .collect();
            ix.add(add)?;
        }
        drop(worker);
        Ok(Arc::new(ix))
    })
    .await;
    match opened {
        Ok(Ok(index)) => {
            *inner.vectors.lock().unwrap_or_else(|e| e.into_inner()) = Some(Vectors {
                key: key.clone(),
                index,
            });
            mark_opened(inner, &key);
        }
        _ => tracing::warn!("vector index could not be opened"),
    }
}

/// チャンクの本文を読む (チャンクの範囲を、元のターンの本文から取る)。
fn chunk_texts(inner: &Inner, ids: &[Id]) -> Vec<(Id, ChunkBody, String)> {
    let Ok(r) = inner.store.read() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for id in ids {
        let Ok(Some(Read::Ok(e))) = r.get(KIND_CHUNK, *id) else {
            continue;
        };
        let Ok(c) = e.body_as::<ChunkBody>() else {
            continue;
        };
        let Ok(Some(Read::Ok(t))) = r.get(KIND_TURN, c.turn) else {
            continue;
        };
        let Ok(tb) = t.body_as::<TurnBody>() else {
            continue;
        };
        let span = evorag_retrieval::chunk::Span {
            start: c.start as usize,
            end: c.end as usize,
        };
        out.push((*id, c, evorag_retrieval::chunk::slice(&tb.text, span)));
    }
    out
}

/// 裏の処理の手順: 足りないチャンクを埋め込む。利用者の操作で落とされても、済んだ分は残る。
pub(crate) async fn run(inner: &Arc<Inner>) {
    let Some((client, key)) = embedder(inner) else {
        return;
    };
    loop {
        // 前の回の保存 (落とされない task) が終わるまで待つ。終わる前に足りない分を数えると、
        // 保存中のチャンクをもう一度埋め込んでしまう。鍵は保存の task に渡し、終わるまで持たせる。
        let gate = inner.embed_gate.clone().lock_owned().await;
        let (gap, _) = missing(inner, &key);
        if gap.is_empty() {
            return;
        }
        let batch: Vec<Id> = gap.into_iter().take(BATCH).collect();
        let items = chunk_texts(inner, &batch);
        if items.is_empty() {
            // 読めないチャンク (消えた直後など)。次の突き合わせで片付く。
            return;
        }
        let texts: Vec<String> = items.iter().map(|(_, _, t)| t.clone()).collect();
        let vectors = match tokio::time::timeout(LIMIT, client.embed(&texts)).await {
            Ok(Ok(v)) if v.len() == items.len() && v.iter().all(|x| !x.is_empty()) => v,
            other => {
                tracing::warn!(failed = other.is_err(), "embedding batch failed");
                return;
            }
        };
        // 記録と索引への反映は、落とされない task で行う。
        let worker = inner.clone();
        let key2 = key.clone();
        let saved = tokio::spawn(async move {
            let ok = save(&worker, &key2, items, vectors).await;
            drop(gate);
            ok
        })
        .await;
        if !matches!(saved, Ok(true)) {
            return;
        }
        crate::emit_recall_state(inner);
    }
}

/// 埋め込みを記録し、索引に足す。索引がまだ無ければ、ベクトルの次元で開く。
async fn save(
    inner: &Arc<Inner>,
    key: &str,
    items: Vec<(Id, ChunkBody, String)>,
    vectors: Vec<Vec<f32>>,
) -> bool {
    // 埋め込んでいる間に、ターンの区切りが作り直されて消えたチャンクは書かない (残った分は、次の
    // 起動の突き合わせで消える)。
    let alive: HashSet<Id> = inner
        .store
        .read()
        .ok()
        .map(|r| {
            items
                .iter()
                .filter(|(id, _, _)| matches!(r.get(KIND_CHUNK, *id), Ok(Some(Read::Ok(_)))))
                .map(|(id, _, _)| *id)
                .collect()
        })
        .unwrap_or_default();
    let w = writes(inner);
    let mut ops = Vec::new();
    let mut rows = Vec::new();
    for ((id, c, _), v) in items.into_iter().zip(vectors) {
        if !alive.contains(&id) {
            continue;
        }
        let body = EmbeddingBody {
            target: id,
            unit: UNIT_CHUNK.to_owned(),
            session: c.session,
            model_key: key.to_owned(),
            dim: v.len() as u32,
        };
        match w.embedding(&body, &v) {
            Ok((_, o)) => ops.extend(o),
            Err(_) => return false,
        }
        rows.push(VecInput {
            id,
            session: c.session,
            unit: Unit::Chunk,
            vector: v,
        });
    }
    if rows.is_empty() {
        return true;
    }
    if write_derived(inner, ops).await.is_err() {
        return false;
    }
    let index = match current(inner) {
        Some((k, ix)) if k == key => ix,
        _ => {
            let dim = rows[0].vector.len();
            let dir = vec_dir(inner.store.data_root(), key);
            let Ok(Ok((ix, _))) =
                tokio::task::spawn_blocking(move || VecIndex::open(&dir, dim)).await
            else {
                return false;
            };
            let ix = Arc::new(ix);
            *inner.vectors.lock().unwrap_or_else(|e| e.into_inner()) = Some(Vectors {
                key: key.to_owned(),
                index: ix.clone(),
            });
            ix
        }
    };
    matches!(
        tokio::task::spawn_blocking(move || index.add(rows)).await,
        Ok(Ok(()))
    )
}
