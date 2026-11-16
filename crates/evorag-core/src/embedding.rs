//! 埋め込みとベクトルの索引 (05 §2.2・§2.3、04 §4 の手順 1、FR-410)。
//!
//! - 埋め込みのモデル (embed の役割、06 §3) ごとに、`index/memory/vec-<model_key>/` に索引を持つ。
//!   今と前の世代だけを残す。
//! - 裏の処理の手順: 語彙の索引にあってベクトルの索引に無いチャンクを、32 件ずつ埋め込み、記録
//!   (`embedding`、ベクトルは `blobs`) と索引に足す。進み具合は状態 (索引にある ID) から求めるので、
//!   止めても強制終了しても、次の機会に続きから作る (job は書かない)。1 回の手順は数回分で区切り、
//!   ほかの手順も動けるようにする。
//! - チャンクと索引を変える処理 (起動時の突き合わせ、チャンクの作り直し、削除、索引を開く、埋め込みの
//!   保存) は、すべて `Inner::index_lock` の下で行う (互いの間に入り込んで、消した会話の埋め込みが
//!   残る・同じチャンクを 2 度埋め込む、を防ぐ)。埋め込みの要求の間は鍵を持たない。
//! - 保存は冪等: 同じモデル・同じチャンクの前の埋め込みは、同じ依頼で消してから書く。
//! - 埋め込みのサーバに断られ続けるチャンクは、このプロセスの間は飛ばす (後ろのチャンクを止めないため)。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use evorag_index::lex::Opened;
use evorag_index::vec::{Unit, VecIndex, VecInput};
use evorag_llm::LlmClient;
use evorag_memory::{
    ChunkBody, EmbeddingBody, KIND_CHUNK, KIND_NOTE, KIND_TURN, NoteBody, TurnBody, UNIT_CHUNK,
    UNIT_KEY,
};
use evorag_store::Read;
use evorag_types::Id;

use crate::{Inner, roles, write_derived, writes};

/// 1 回に埋め込むチャンクの数。
const BATCH: usize = 32;
/// 1 回の手順で埋め込む回数の上限 (ほかの手順も動けるように、ここで区切る)。
const ROUNDS_PER_STEP: usize = 8;
/// 埋め込みの要求の締切 (1 件あたり 2 秒、少なくとも 20 秒)。
fn limit(n: usize) -> Duration {
    Duration::from_secs((2 * n as u64).max(20))
}
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

/// 足りないチャンク (語彙の索引にあって、今のモデルのベクトルの索引に無く、飛ばしていないもの) と、全体の数。
fn missing(inner: &Inner, key: &str) -> (Vec<Id>, usize) {
    let Some(lex) = &inner.index else {
        return (Vec::new(), 0);
    };
    let all = lex.snapshot().ids();
    let have: HashSet<Id> = match current(inner) {
        Some((k, ix)) if k == key => ix.snapshot().ids(),
        _ => HashSet::new(),
    };
    let skip = inner.embed_skip.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<Id> = all
        .difference(&have)
        .filter(|id| !skip.contains(*id))
        .copied()
        .collect();
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

/// 作り直しの進み具合 (足りないものが多いときだけ。百分率)。
pub(crate) fn rebuilding(inner: &Inner) -> Option<u8> {
    let (_, key) = embedder(inner)?;
    let (gap, total) = missing(inner, &key);
    (gap.len() > REBUILD_SHOWN_AT && total > 0)
        .then(|| (((total - gap.len()) * 100) / total).min(99) as u8)
}

/// 索引を開き、DB の記録と突き合わせる (`index_lock` の下で、同じモデルについては 1 回だけ)。前の世代より
/// 古い索引と記録は消す (05 §2.2)。DB の本体だけで ID を比べ、足す分のベクトルだけを読む (NFR-101)。
pub(crate) async fn open(inner: &Arc<Inner>) {
    let Some((_, key)) = embedder(inner) else {
        return;
    };
    let _lock = inner.index_lock.lock().await;
    if inner
        .vectors_opened
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        == Some(key.as_str())
    {
        return;
    }
    let previous = previous_key(inner, &key);
    let worker = inner.clone();
    let key2 = key.clone();
    let planned =
        tokio::task::spawn_blocking(move || plan_open(&worker, &key2, previous.as_deref()))
            .await
            .ok()
            .flatten();
    let Some(plan) = planned else {
        tracing::warn!("vector index could not be checked");
        return;
    };
    if !plan.stale.is_empty() {
        let w = writes(inner);
        let ops: Vec<_> = plan
            .stale
            .iter()
            .flat_map(|id| w.delete_embedding(*id))
            .collect();
        let _ = write_derived(inner, ops).await;
    }
    match plan.index {
        Some(index) => {
            *inner.vectors.lock().unwrap_or_else(|e| e.into_inner()) = Some(Vectors {
                key: key.clone(),
                index,
            });
        }
        None => {
            *inner.vectors.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
    *inner
        .vectors_opened
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(key);
}

struct OpenPlan {
    /// 消す埋め込みの記録 (古いモデル、対象の無いもの、重複、ベクトルの壊れたもの)。
    stale: Vec<Id>,
    /// 突き合わせた索引 (今のモデルの記録が無ければ `None`)。
    index: Option<Arc<VecIndex>>,
}

fn plan_open(inner: &Inner, key: &str, previous: Option<&str>) -> Option<OpenPlan> {
    let root = inner.store.data_root().to_path_buf();
    let mut keep = vec![key];
    keep.extend(previous);
    evorag_index::vec::prune_generations(&memory_dir(&root), &keep);
    let r = inner.store.read().ok()?;
    let bodies = evorag_memory::embedding_bodies(&r).ok()?;
    let chunks: HashSet<Id> = inner
        .index
        .as_ref()
        .map(|ix| ix.snapshot().ids())
        .unwrap_or_default();
    let mut stale = Vec::new();
    // 今と前のモデルの、対象と単位 (チャンク、またはノートの鍵の位置) ごとに 1 件 (新しい記録を残す。ID は
    // 時刻の順)。
    let mut mine: HashMap<(Id, Option<u32>), (Id, EmbeddingBody)> = HashMap::new();
    let mut prev: HashMap<(Id, Option<u32>), Id> = HashMap::new();
    for (id, b) in bodies {
        let orphan = !chunks.is_empty() && !chunks.contains(&b.target);
        if orphan {
            stale.push(id);
        } else if b.model_key == key {
            let unit = (b.target, b.key);
            match mine.get(&unit) {
                Some((kept, _)) if *kept > id => stale.push(id),
                Some((kept, _)) => {
                    stale.push(*kept);
                    mine.insert(unit, (id, b));
                }
                None => {
                    mine.insert(unit, (id, b));
                }
            }
        } else if previous == Some(b.model_key.as_str()) {
            let unit = (b.target, b.key);
            match prev.get(&unit) {
                Some(kept) if *kept > id => stale.push(id),
                Some(kept) => {
                    stale.push(*kept);
                    prev.insert(unit, id);
                }
                None => {
                    prev.insert(unit, id);
                }
            }
        } else {
            stale.push(id);
        }
    }
    let dir = vec_dir(&root, key);
    let Some(dim) = mine.values().next().map(|(_, b)| b.dim as usize) else {
        // 今のモデルの記録が無い: 索引のフォルダがあっても、中身は信用できないので消す (作り直す)。
        if dir.is_dir() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        return Some(OpenPlan { stale, index: None });
    };
    let (ix, opened) = VecIndex::open(&dir, dim).ok()?;
    let want: HashSet<Id> = mine.keys().map(|(t, _)| *t).collect();
    let have = if opened == Opened::Ok {
        ix.snapshot().ids()
    } else {
        HashSet::new()
    };
    let extra: HashSet<Id> = have.difference(&want).copied().collect();
    if !extra.is_empty() {
        ix.remove(&extra).ok()?;
    }
    let mut add = Vec::new();
    for ((target, _), (id, b)) in &mine {
        if have.contains(target) {
            continue;
        }
        match evorag_memory::embedding_vector(&r, *id).ok().flatten() {
            Some(v) if v.len() == dim && b.dim as usize == dim => add.push(VecInput {
                id: *target,
                session: b.session,
                unit: if b.unit == UNIT_KEY {
                    Unit::Key
                } else {
                    Unit::Chunk
                },
                vector: v,
            }),
            // ベクトルの壊れた・次元の違う記録は消す (もう一度埋め込む)。
            _ => stale.push(*id),
        }
    }
    if !add.is_empty() {
        tracing::info!(rows = add.len(), "vector index filled from the database");
        ix.add(add).ok()?;
    }
    Some(OpenPlan {
        stale,
        index: Some(Arc::new(ix)),
    })
}

/// 消した会話のチャンクの行を、今と前の世代のベクトルの索引から除く (04 §7、AC-FR-309-1)。
/// 呼び手は `index_lock` を持つ。
pub(crate) fn remove_everywhere(inner: &Inner, gone: &HashSet<Id>) {
    if gone.is_empty() {
        return;
    }
    let current = current(inner);
    if let Some((_, ix)) = &current {
        let _ = ix.remove(gone);
    }
    let Ok(entries) = std::fs::read_dir(memory_dir(inner.store.data_root())) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(key) = name.strip_prefix("vec-") else {
            continue;
        };
        if current.as_ref().is_some_and(|(k, _)| k == key) {
            continue;
        }
        if let Some(ix) = VecIndex::open_existing(&e.path()) {
            let _ = ix.remove(gone);
        }
    }
}

/// 埋め込む 1 件 (チャンク 1 つ、またはノートの鍵 1 つ)。
struct Item {
    id: Id,
    session: Id,
    /// ノートの鍵の位置 (チャンクは `None`)。
    key: Option<u32>,
    text: String,
}

/// 埋め込む本文を読む。チャンクは範囲を元のターンの本文から取り、ノートは鍵のそれぞれを 1 件にする。
fn unit_texts(inner: &Inner, ids: &[Id]) -> Vec<Item> {
    let Ok(r) = inner.store.read() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for id in ids {
        if let Ok(Some(Read::Ok(e))) = r.get(KIND_NOTE, *id)
            && let Ok(n) = e.body_as::<NoteBody>()
        {
            for (i, k) in n.keys.iter().enumerate() {
                out.push(Item {
                    id: *id,
                    session: n.session,
                    key: Some(i as u32),
                    text: k.clone(),
                });
            }
            continue;
        }
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
        out.push(Item {
            id: *id,
            session: c.session,
            key: None,
            text: evorag_retrieval::chunk::slice(&tb.text, span),
        });
    }
    out
}

/// 埋め込む。要求ごと断られたら 1 件ずつ試し、断られたものは `None` にする。
async fn embed_each(client: &dyn LlmClient, texts: &[String]) -> Vec<Option<Vec<f32>>> {
    let ok = |v: &Vec<f32>| !v.is_empty() && v.iter().all(|x| x.is_finite());
    if let Ok(Ok(v)) = tokio::time::timeout(limit(texts.len()), client.embed(texts)).await
        && v.len() == texts.len()
        && v.iter().all(ok)
    {
        return v.into_iter().map(Some).collect();
    }
    let mut out = Vec::with_capacity(texts.len());
    for t in texts {
        let one = std::slice::from_ref(t);
        out.push(
            match tokio::time::timeout(limit(1), client.embed(one)).await {
                Ok(Ok(mut v)) if v.len() == 1 && ok(&v[0]) => v.pop(),
                _ => None,
            },
        );
    }
    out
}

/// 裏の処理の手順: 足りないチャンクを埋め込む (数回分で区切る)。利用者の操作で落とされても、済んだ分は
/// 残る。1 件でも足せたら `true`。
pub(crate) async fn run(inner: &Arc<Inner>) -> bool {
    let Some((client, key)) = embedder(inner) else {
        return false;
    };
    let mut progressed = false;
    for _ in 0..ROUNDS_PER_STEP {
        let (gap, _) = missing(inner, &key);
        let batch: Vec<Id> = gap.into_iter().take(BATCH).collect();
        if batch.is_empty() {
            break;
        }
        let items = unit_texts(inner, &batch);
        let read: HashSet<Id> = items.iter().map(|i| i.id).collect();
        // 読めないチャンク (区切りの作り直しで消えた直後など) は、このプロセスの間は飛ばす
        // (次の起動の突き合わせで片付く)。
        inner
            .embed_skip
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(batch.iter().filter(|id| !read.contains(id)));
        if items.is_empty() {
            continue;
        }
        let texts: Vec<String> = items.iter().map(|i| i.text.clone()).collect();
        let vectors = embed_each(&*client, &texts).await;
        // 断られ続ける単位は飛ばす (後ろを止めないため)。ノートは、鍵が 1 つでも断られたら全体を飛ばす
        // (鍵の行はノートごとにまとめて入れ替えるので、一部だけを書かない)。
        let refused: HashSet<Id> = items
            .iter()
            .zip(&vectors)
            .filter(|(_, v)| v.is_none())
            .map(|(i, _)| i.id)
            .collect();
        if !refused.is_empty() {
            tracing::warn!(
                n = refused.len(),
                "some units were refused by the embedding server; skipping them"
            );
            inner
                .embed_skip
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(refused.iter().copied());
        }
        let mut keep = Vec::new();
        let mut done = Vec::new();
        for (item, v) in items.into_iter().zip(vectors) {
            if let Some(v) = v.filter(|_| !refused.contains(&item.id)) {
                keep.push(item);
                done.push(v);
            }
        }
        if keep.is_empty() {
            continue;
        }
        // 記録と索引への反映は、鍵の下の落とされない task で行う。
        let worker = inner.clone();
        let key2 = key.clone();
        let saved = tokio::spawn(async move {
            let _lock = worker.index_lock.lock().await;
            save(&worker, &key2, keep, done).await
        })
        .await;
        if !matches!(saved, Ok(true)) {
            break;
        }
        progressed = true;
        crate::emit_recall_state(inner);
    }
    progressed
}

/// 埋め込みを記録し、索引に足す (呼び手は `index_lock` を持つ)。同じモデル・同じチャンクの前の埋め込みは
/// 同じ依頼で消す (冪等)。索引がまだ無ければ、ベクトルの次元で開く。
async fn save(inner: &Arc<Inner>, key: &str, items: Vec<Item>, vectors: Vec<Vec<f32>>) -> bool {
    let w = writes(inner);
    let mut ops = Vec::new();
    let mut rows = Vec::new();
    {
        let Ok(r) = inner.store.read() else {
            return false;
        };
        let mut cleared = HashSet::new();
        for (item, v) in items.into_iter().zip(vectors) {
            let id = item.id;
            // 埋め込んでいる間に消えたチャンクとノートは書かない (削除・区切りの作り直し・ノートの作り直しは、
            // この鍵の下で行う)。
            let kind = if item.key.is_some() {
                KIND_NOTE
            } else {
                KIND_CHUNK
            };
            if !matches!(r.get(kind, id), Ok(Some(Read::Ok(_)))) {
                continue;
            }
            if cleared.insert(id) {
                for old in evorag_memory::embeddings_of_chunk(&r, id, key).unwrap_or_default() {
                    ops.extend(w.delete_embedding(old));
                }
            }
            let body = EmbeddingBody {
                target: id,
                unit: if item.key.is_some() {
                    UNIT_KEY
                } else {
                    UNIT_CHUNK
                }
                .to_owned(),
                session: item.session,
                model_key: key.to_owned(),
                dim: v.len() as u32,
                key: item.key,
            };
            match w.embedding(&body, &v) {
                Ok((_, o)) => ops.extend(o),
                Err(_) => return false,
            }
            rows.push(VecInput {
                id,
                session: item.session,
                unit: if item.key.is_some() {
                    Unit::Key
                } else {
                    Unit::Chunk
                },
                vector: v,
            });
        }
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
