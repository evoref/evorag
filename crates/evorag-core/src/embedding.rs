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
//! - 区切りの鍵 (04 §5.5、v0.5 の根) は、区切りの ID の項目ごとに、索引に入れた鍵 ([`crate::ledgers::keys`]) を
//!   1 行ずつ埋め込み、記録は (台帳の ID, 鍵の値の鍵) で指す。同じ記録 (同じモデル) があれば、要求を出さずに
//!   そのベクトルを使い直す。区切りの行はまとめて入れる (鍵が 1 つでも断られたら、その区切りを飛ばす)。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use evorag_index::lex::Opened;
use evorag_index::vec::{Unit, VecIndex, VecInput};
use evorag_llm::LlmClient;
use evorag_memory::{
    ChunkBody, EmbeddingBody, KIND_CHUNK, KIND_NOTE, KIND_PROPOSAL, KIND_TURN, NoteBody, TurnBody,
    UNIT_CHUNK, UNIT_KEY, UNIT_SEGMENT_KEY, ValueKey,
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
/// 事実の検索の鍵は埋め込まない (語彙の索引だけ、fact_table.rs) ので数えない。区切りの鍵は、区切りの鍵の手順が
/// 突き合わせた区切りだけを数える (読む鍵がまだ分からない区切りを飛ばさないため)。
fn missing(inner: &Inner, key: &str) -> (Vec<Id>, usize) {
    let Some(lex) = &inner.index else {
        return (Vec::new(), 0);
    };
    let segments = crate::ledgers::keys::indexed_ids(inner);
    let all = lex.snapshot().ids_where(|d| {
        !crate::recall::is_fact_key(d)
            && (!crate::recall::is_segment_key(d) || segments.contains(&d.id))
    });
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
    if !plan.unsynced.is_empty() {
        // 同じ行を除く job が残っていれば、足さない (起動のたびに増やさない)。
        let want: HashSet<Id> = plan.unsynced.iter().copied().collect();
        let queued = inner
            .store
            .read()
            .ok()
            .and_then(|r| evorag_memory::pending_cleanup_jobs(&r).ok())
            .unwrap_or_default()
            .iter()
            .any(|j| !j.backups && j.ids.iter().copied().collect::<HashSet<Id>>() == want);
        if !queued
            && let Ok((_, job)) = evorag_memory::index_job(&writes(inner), plan.unsynced.clone())
            && let Err(e) = write_derived(inner, vec![job]).await
        {
            tracing::warn!(error = %e, "the index cleanup job could not be written");
        }
        inner
            .cleanup_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
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
    /// 前の世代の索引から除けなかった行 (索引の整理の job に残す)。
    unsynced: Vec<Id>,
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
    // 区切りの鍵の埋め込みは台帳を指す (04 §5.5)。行は区切りの鍵の手順が区切りの ID で入れるので、ここでは索引に
    // 足さず、台帳の無いもの・今と前のモデルでないもの・(モデル, 台帳, 鍵) の重複だけを消す。
    // v0.5 の根で台帳を読めなければ開かない (台帳の集合を空とみなして、有効な埋め込みを消さないため)。v0.4 の形式の
    // 根には台帳の種類が無い。
    let mut ledgers: HashSet<Id> = HashSet::new();
    if inner.store.is_v05_root() {
        r.for_each(KIND_PROPOSAL, |x| {
            if let Read::Ok(e) = x {
                ledgers.insert(e.id);
            }
            true
        })
        .ok()?;
    }
    let mut keys_seen: HashMap<(String, Id, String), Id> = HashMap::new();
    let mut key_dim: Option<usize> = None;
    // 今と前のモデルの、対象と単位 (チャンク、またはノートの鍵の位置) ごとに 1 件 (新しい記録を残す。ID は
    // 時刻の順)。
    let mut mine: HashMap<(Id, Option<u32>), (Id, EmbeddingBody)> = HashMap::new();
    let mut prev: HashMap<(Id, Option<u32>), Id> = HashMap::new();
    for (id, b) in bodies {
        if b.unit == UNIT_SEGMENT_KEY {
            let usable = ledgers.contains(&b.target)
                && b.vk.as_deref().and_then(ValueKey::from_hex).is_some()
                && (b.model_key == key || previous == Some(b.model_key.as_str()));
            if !usable {
                stale.push(id);
                continue;
            }
            if b.model_key == key {
                key_dim.get_or_insert(b.dim as usize);
            }
            let unit = (
                b.model_key.clone(),
                b.target,
                b.vk.clone().unwrap_or_default(),
            );
            match keys_seen.get(&unit) {
                Some(kept) if *kept > id => stale.push(id),
                Some(kept) => {
                    stale.push(*kept);
                    keys_seen.insert(unit, id);
                }
                None => {
                    keys_seen.insert(unit, id);
                }
            }
            continue;
        }
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
    let unsynced = match previous {
        Some(p) => {
            let want: HashSet<Id> = prev.keys().map(|(t, _)| *t).collect();
            sync_previous(&vec_dir(&root, p), &want)
        }
        None => Vec::new(),
    };
    let dir = vec_dir(&root, key);
    let Some(dim) = mine
        .values()
        .next()
        .map(|(_, b)| b.dim as usize)
        .or(key_dim)
    else {
        // 今のモデルの記録が無い: 索引のフォルダがあっても、中身は信用できないので消す (作り直す)。
        if dir.is_dir() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        return Some(OpenPlan {
            stale,
            index: None,
            unsynced,
        });
    };
    let (ix, opened) = VecIndex::open(&dir, dim).ok()?;
    let want: HashSet<Id> = mine.keys().map(|(t, _)| *t).collect();
    // 区切りの鍵の行は、区切りの鍵の手順が突き合わせる (区切りの ID の行で、記録の対象は台帳のため)。
    let have = if opened == Opened::Ok {
        ix.snapshot()
            .ids_where(|r| r.session != crate::recall::SEGMENT_KEY_SESSION)
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
        unsynced,
    })
}

/// 消した記録の行を、今と前の世代のすべてのベクトルの索引から除く (04 §7.7 の 2、AC-FR-309-1)。呼び手は
/// `index_lock` を持つ。すべて除けたら `true` (除けなかった分は、呼び手が job に残して試し直す)。開けない前の
/// 世代の索引は、使えないので消す (使うときに DB から作り直す、05 §2.2)。
pub(crate) fn remove_everywhere(inner: &Inner, gone: &HashSet<Id>) -> bool {
    if gone.is_empty() {
        return true;
    }
    let mut ok = true;
    let current = current(inner);
    if let Some((_, ix)) = &current
        && let Err(e) = ix.remove(gone)
    {
        tracing::warn!(error = %e, "deleted rows could not be removed from the vector index");
        ok = false;
    }
    let entries = match std::fs::read_dir(memory_dir(inner.store.data_root())) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ok,
        Err(e) => {
            tracing::warn!(error = %e, "the vector indexes could not be listed");
            return false;
        }
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(key) = name.strip_prefix("vec-") else {
            continue;
        };
        if current.as_ref().is_some_and(|(k, _)| k == key) {
            continue;
        }
        ok &= remove_from_generation(&e.path(), gone);
    }
    ok
}

/// 前の世代 (今開いていない) のベクトルの索引から行を除く。開けなければフォルダごと消す。
fn remove_from_generation(dir: &std::path::Path, gone: &HashSet<Id>) -> bool {
    match VecIndex::open_existing(dir) {
        Some(ix) => match ix.remove(gone) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, dir = %dir.display(), "deleted rows could not be removed from a previous vector index");
                false
            }
        },
        None => match std::fs::remove_dir_all(dir) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                tracing::warn!(error = %e, dir = %dir.display(), "an unreadable vector index could not be removed");
                false
            }
        },
    }
}

/// 前の世代の索引を、DB の記録と突き合わせる (起動時の突き合わせをすべての世代に当てる、04 §7.7 の 2)。
/// `want` は、DB に記録のある前のモデルの埋め込みの対象。それ以外の行を除く。除けなかった行を返す (呼び手が
/// 索引の整理の job に残す)。読めない索引を消せないときは、次の起動でもう一度試す。
fn sync_previous(dir: &std::path::Path, want: &HashSet<Id>) -> Vec<Id> {
    if !dir.is_dir() {
        return Vec::new();
    }
    let Some(ix) = VecIndex::open_existing(dir) else {
        // 読めない索引は使えないので消す (戻したときに DB から作り直す)。
        if let Err(e) = std::fs::remove_dir_all(dir) {
            tracing::warn!(error = %e, dir = %dir.display(), "an unreadable vector index could not be removed");
        }
        return Vec::new();
    };
    let extra: HashSet<Id> = ix
        .snapshot()
        .ids_where(|r| r.session != crate::recall::SEGMENT_KEY_SESSION)
        .difference(want)
        .copied()
        .collect();
    match ix.remove(&extra) {
        Ok(()) => Vec::new(),
        Err(e) => {
            tracing::warn!(error = %e, "the previous vector index could not be synced");
            let mut left: Vec<Id> = extra.into_iter().collect();
            left.sort();
            left
        }
    }
}

/// 埋め込む単位の対象。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// チャンク (出典のターンの改訂の番号、03 §3.3 の `src_rev`)。
    Chunk { src_rev: u32 },
    /// ノートの鍵の位置 (ノートの `src_revs` で判定するので `src_rev` は持たない)。
    Note { key: u32 },
    /// 区切りの鍵 (04 §5.5)。記録は (台帳の ID, 鍵の値の鍵) で指し、`session` は区切りのセッション。
    Segment {
        ledger: Id,
        vk: ValueKey,
        session: Id,
    },
}

/// 埋め込む 1 件 (チャンク 1 つ、ノートの鍵 1 つ、または区切りの鍵 1 つ)。
struct Item {
    /// 索引の行の ID (チャンク・ノート・区切り)。
    id: Id,
    /// 索引の行のセッション (区切りの鍵はセッションに結び付けない値)。
    session: Id,
    target: Target,
    text: String,
    /// 同じモデルの記録があって使い直す (区切りの鍵だけ)。記録は書かない。
    reused: bool,
}

/// 区切りの鍵の、あるモデルの埋め込みの記録 (新しい順)。
fn segment_key_embeddings(
    r: &evorag_store::ReadTxn<'_>,
    ledger: Id,
    vk: &ValueKey,
    model_key: &str,
) -> Vec<Id> {
    let hex = vk.to_hex();
    let mut out: Vec<Id> = evorag_memory::embeddings_of(r, ledger)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, b)| {
            b.as_ref().is_some_and(|b| {
                b.unit == UNIT_SEGMENT_KEY
                    && b.model_key == model_key
                    && b.vk.as_deref() == Some(hex.as_str())
            })
        })
        .map(|(id, _)| id)
        .collect();
    out.sort_by(|a, b| b.cmp(a));
    out
}

/// 埋め込む本文を読む。チャンクは範囲を元のターンの本文から取り、ノートは鍵のそれぞれを 1 件にする。区切りは
/// 索引に入れた鍵のそれぞれを 1 件にし、同じモデルの記録があればそのベクトルを添える。
fn unit_texts(inner: &Inner, ids: &[Id], model_key: &str) -> Vec<(Item, Option<Vec<f32>>)> {
    let Ok(r) = inner.store.read() else {
        return Vec::new();
    };
    // 忘れるの指示と合わないターンのチャンクは埋め込まない (04 §7.2。伏せる前の本文かもしれないため)。確かめ
    // られなければ、この回はチャンクを埋め込まない (どちらも、このプロセスの間は飛ばす)。
    let hide = crate::masking::Hide::load(&r, inner.language);
    let mut out = Vec::new();
    for id in ids {
        if let Some(seg) = crate::ledgers::keys::indexed(inner, *id) {
            for k in &seg.keys {
                let vk = k.vk();
                let known = segment_key_embeddings(&r, seg.ledger, &vk, model_key)
                    .into_iter()
                    .find_map(|e| evorag_memory::embedding_vector(&r, e).ok().flatten())
                    .filter(|v| !v.is_empty() && v.iter().all(|x| x.is_finite()));
                out.push((
                    Item {
                        id: *id,
                        session: crate::recall::SEGMENT_KEY_SESSION,
                        target: Target::Segment {
                            ledger: seg.ledger,
                            vk,
                            session: seg.session,
                        },
                        text: k.text().to_owned(),
                        reused: known.is_some(),
                    },
                    known,
                ));
            }
            continue;
        }
        if let Ok(Some(Read::Ok(e))) = r.get(KIND_NOTE, *id)
            && let Ok(n) = e.body_as::<NoteBody>()
        {
            for (i, k) in n.keys.iter().enumerate() {
                out.push((
                    Item {
                        id: *id,
                        session: n.session,
                        target: Target::Note { key: i as u32 },
                        text: k.clone(),
                        reused: false,
                    },
                    None,
                ));
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
        if hide.as_ref().is_none_or(|h| h.excluded(&t)) {
            continue;
        }
        let Ok(tb) = t.body_as::<TurnBody>() else {
            continue;
        };
        let span = evorag_retrieval::chunk::Span {
            start: c.start as usize,
            end: c.end as usize,
        };
        out.push((
            Item {
                id: *id,
                session: c.session,
                target: Target::Chunk {
                    src_rev: c.src_rev(),
                },
                text: evorag_retrieval::chunk::slice(&tb.text, span),
                reused: false,
            },
            None,
        ));
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
/// 残る。1 件でも足せたか飛ばしたら (足りないものが減ったら) `true`。断られた単位を飛ばしただけの回を
/// 「進まなかった」とすると、その間に新しいチャンク (応答のチャンクなど) が増えたときに、手順ごと待たされる
/// (sleep.rs の `BACKOFF`)。
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
        let (items, known): (Vec<Item>, Vec<Option<Vec<f32>>>) =
            unit_texts(inner, &batch, &key).into_iter().unzip();
        let read: HashSet<Id> = items.iter().map(|i| i.id).collect();
        // 読めないチャンク (区切りの作り直しで消えた直後など) は、このプロセスの間は飛ばす
        // (次の起動の突き合わせで片付く)。
        let unreadable: Vec<Id> = batch
            .iter()
            .filter(|id| !read.contains(id))
            .copied()
            .collect();
        if !unreadable.is_empty() {
            progressed = true;
            inner
                .embed_skip
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(unreadable);
        }
        if items.is_empty() {
            continue;
        }
        // 使い直せる記録の無いものだけを埋め込む。
        let pending: Vec<usize> = known
            .iter()
            .enumerate()
            .filter(|(_, v)| v.is_none())
            .map(|(n, _)| n)
            .collect();
        let texts: Vec<String> = pending.iter().map(|&n| items[n].text.clone()).collect();
        let mut vectors = known;
        if !texts.is_empty() {
            for (n, v) in pending.into_iter().zip(embed_each(&*client, &texts).await) {
                vectors[n] = v;
            }
        }
        // 断られ続ける単位は飛ばす (後ろを止めないため)。ノートと区切りは、鍵が 1 つでも断られたら全体を飛ばす
        // (鍵の行はノート・区切りごとにまとめて入れ替えるので、一部だけを書かない)。
        let refused: HashSet<Id> = items
            .iter()
            .zip(&vectors)
            .filter(|(_, v)| v.is_none())
            .map(|(i, _)| i.id)
            .collect();
        if !refused.is_empty() {
            progressed = true;
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
        // 記録と索引への反映は、鍵の下の落とされない task で行う。鍵は spawn の前に取る (終了の処理が鍵を
        // 取れたら、出した保存は終わっている)。
        let lock = inner.index_lock.clone().lock_owned().await;
        let worker = inner.clone();
        let key2 = key.clone();
        let saved = tokio::spawn(async move {
            let _lock = lock;
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

/// 行をまとめて入れてよい区切り: 埋め込んだ鍵が、今索引に入れている鍵 (台帳と値の鍵の集合) と同じもの (埋め込んで
/// いる間に区切りの鍵の手順が鍵を入れ替えた区切りは、一部の行だけを入れないように書かない)。
fn current_segments(inner: &Inner, items: &[Item]) -> HashSet<Id> {
    let mut got: HashMap<Id, (Id, BTreeSet<ValueKey>)> = HashMap::new();
    for i in items {
        if let Target::Segment { ledger, vk, .. } = &i.target {
            got.entry(i.id)
                .or_insert_with(|| (*ledger, BTreeSet::new()))
                .1
                .insert(*vk);
        }
    }
    got.into_iter()
        .filter(|(seg, (ledger, vks))| {
            crate::ledgers::keys::indexed(inner, *seg).is_some_and(|now| {
                now.ledger == *ledger
                    && now.keys.iter().map(|k| k.vk()).collect::<BTreeSet<_>>() == *vks
            })
        })
        .map(|(seg, _)| seg)
        .collect()
}

/// 埋め込みの記録の中身 (対象の記録の種類と ID、単位、鍵の位置、出典の改訂の番号、鍵の値の鍵、セッション)。
struct Record {
    kind: &'static str,
    target: Id,
    unit: &'static str,
    key: Option<u32>,
    src_rev: Option<u32>,
    vk: Option<ValueKey>,
    session: Id,
}

impl Item {
    fn record(&self) -> Record {
        match &self.target {
            Target::Chunk { src_rev } => Record {
                kind: KIND_CHUNK,
                target: self.id,
                unit: UNIT_CHUNK,
                key: None,
                src_rev: Some(*src_rev),
                vk: None,
                session: self.session,
            },
            Target::Note { key } => Record {
                kind: KIND_NOTE,
                target: self.id,
                unit: UNIT_KEY,
                key: Some(*key),
                src_rev: None,
                vk: None,
                session: self.session,
            },
            Target::Segment {
                ledger,
                vk,
                session,
            } => Record {
                kind: KIND_PROPOSAL,
                target: *ledger,
                unit: UNIT_SEGMENT_KEY,
                key: None,
                src_rev: None,
                vk: Some(*vk),
                session: *session,
            },
        }
    }
}

/// 埋め込みを記録し、索引に足す (呼び手は `index_lock` を持つ)。同じモデル・同じチャンクの前の埋め込みは
/// 同じ依頼で消す (冪等)。使い直す区切りの鍵の埋め込みは記録を書かず、行だけを足す。索引がまだ無ければ、
/// ベクトルの次元で開く。
async fn save(inner: &Arc<Inner>, key: &str, items: Vec<Item>, vectors: Vec<Vec<f32>>) -> bool {
    let w = writes(inner);
    let mut ops = Vec::new();
    let mut rows = Vec::new();
    let segments = current_segments(inner, &items);
    {
        let Ok(r) = inner.store.read() else {
            return false;
        };
        let mut cleared = HashSet::new();
        for (item, v) in items.into_iter().zip(vectors) {
            if matches!(item.target, Target::Segment { .. }) && !segments.contains(&item.id) {
                continue;
            }
            let rec = item.record();
            // 埋め込んでいる間に消えたチャンク・ノート・台帳は書かない (削除・区切りの作り直し・ノートの作り直しと
            // 区切りの鍵の入れ替えは、この鍵の下で行う)。
            if !matches!(r.get(rec.kind, rec.target), Ok(Some(Read::Ok(_)))) {
                continue;
            }
            if !item.reused {
                // 書き手のトランザクションの中でも、対象が残っていることを確かめる (04 §7.7 の 4)。
                if !cleared.iter().any(|(t, _)| *t == rec.target) {
                    ops.push(evorag_memory::expect_record(rec.kind, rec.target));
                }
                if cleared.insert((rec.target, rec.vk)) {
                    let old = match &rec.vk {
                        Some(vk) => segment_key_embeddings(&r, rec.target, vk, key),
                        None => evorag_memory::embeddings_of_chunk(&r, rec.target, key)
                            .unwrap_or_default(),
                    };
                    for old in old {
                        ops.extend(w.delete_embedding(old));
                    }
                }
                let body = EmbeddingBody {
                    target: rec.target,
                    unit: rec.unit.to_owned(),
                    session: rec.session,
                    model_key: key.to_owned(),
                    dim: v.len() as u32,
                    key: rec.key,
                    src_rev: rec.src_rev,
                    vk: rec.vk.map(|v| v.to_hex()),
                };
                match w.embedding(&body, &v) {
                    Ok((_, o)) => ops.extend(o),
                    Err(_) => return false,
                }
            }
            rows.push(VecInput {
                id: item.id,
                session: item.session,
                unit: match item.target {
                    Target::Chunk { .. } => Unit::Chunk,
                    Target::Note { .. } | Target::Segment { .. } => Unit::Key,
                },
                vector: v,
            });
        }
    }
    if rows.is_empty() {
        return true;
    }
    if !ops.is_empty() && write_derived(inner, ops).await.is_err() {
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
