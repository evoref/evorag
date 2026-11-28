//! 区切りの鍵を語彙とベクトルの索引に入れる (04 §5.5、04 §4 の手順 5、v0.5 の根)。
//!
//! 区切りごとに、使う台帳 ([`evorag_memory::select_ledger`]) の `keys` に区切りの鍵の検査 (B3) と導出の時点のふるい
//! を当て ([`evorag_memory::segment_keys`])、残った鍵を区切りの ID の項目として索引に入れる (鍵ごとに 1 件。
//! セッションに結び付けない値 [`crate::recall::SEGMENT_KEY_SESSION`] で入れる)。鍵の埋め込みは、埋め込みの手順
//! ([`crate::embedding`]) が語彙の索引との差から作り、(台帳の ID, 鍵の値の鍵) で記録する。同じ記録があれば
//! 埋め込みの要求を出さずに使い直す (`/private off` で同じ鍵が戻るとき・使う台帳が戻ったとき)。
//!
//! - **突き合わせ** ([`sync`]): 起動の後と、会話・指示・呼ぶ先が変わった後・台帳を書いた後 ([`touch`]) に、記録から
//!   求めた鍵と、索引に入れた鍵 (区切りごとの指紋) を比べ、違う区切りの項目だけを入れ替える (区切りの ID の項目を
//!   語彙とベクトルのすべての世代の索引から除いてから、語彙の索引に足す)。指紋は索引の置き場所の
//!   `segment-keys.json` に持ち (作り直せる派生の値、05 §2.3)、入れ替える前に、入れ替える区切りを除いた指紋を
//!   書く (途中で止まったら、次の起動で入れ替え直す)。読めなければ、すべての区切りを入れ替える。
//! - **埋め込みを消す**: 台帳の区切りの鍵の埋め込みのうち、その台帳の今の検査の結果に無い鍵 (台帳を掃除した・
//!   規則の版が変わった) と、忘れるのふるいで除いた鍵のものを消す (04 §5.5・§7.2。掃除の前と後の検査の結果の
//!   差 [`evorag_memory::segment_keys::stale_embeddings`] と同じものを、状態から求める)。private のふるいで除いた
//!   鍵の埋め込みは残す (索引の項目だけを除く)。
//! - 導出 (B4) が入るまで、導出で private の印を付けた単位は無いものとしてふるう (`private_units` は空)。
//!
//! `memory.forget_key` が使えないとき・指示を読めないときは、索引を変えない (鍵の値の鍵と忘れるの範囲を確かめ
//! られないため。03 §3.1.1 のフェイルクローズ)。旧エンジンの根 ([`crate::RootFormat::V04Bench`]) では動かさない。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use evorag_memory::b2;
use evorag_memory::b3::Rules;
use evorag_memory::corrections::CORRECTION_V1;
use evorag_memory::segment_keys::{
    CheckedKeys, KeyDrop, SEGMENT_KEYS_VERSION, Screen, SegmentKey, check_keys,
};
use evorag_memory::{
    Current, DirectiveBody, DirectiveSet, ForgetKey, KIND_DIRECTIVE, KIND_SEGMENT, KIND_TURN,
    LedgerBody, RuleVersions, SegmentBody, SelectEnv, Selection, StoredLedger, UNIT_SEGMENT_KEY,
    ValueKey, select_ledger, turn_rev_checked,
};
use evorag_store::{Read, ReadTxn, Store};
use evorag_types::{Id, PromptLanguage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Inner, MemoryRoute};

/// 指紋の置き場所の名前 (索引の置き場所 `index/memory/` の中、05 §2.3)。
const STATE_FILE: &str = "segment-keys.json";
/// 指紋のファイルの形式名と版。
const STATE_FORMAT: &str = "evorag-segment-keys";
const STATE_VERSION: u32 = 1;

/// 索引に入れた区切りの鍵 (区切り 1 つ分)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Indexed {
    /// 使った台帳 (鍵の埋め込みを指す)。
    pub(crate) ledger: Id,
    /// 区切りのセッション (鍵の埋め込みの記録の `session`)。
    pub(crate) session: Id,
    /// 索引に入れた鍵 (台帳の順。1 つ以上)。
    pub(crate) keys: Vec<SegmentKey>,
    /// 区切りの項目の指紋 ([`fingerprint`])。
    hash: String,
}

/// 台帳ごとの検査とふるいの結果 (指示と台帳の `keys` が変わらなければ使い直す)。
#[derive(Debug, Clone)]
struct Memo {
    raw: Vec<String>,
    stamp: Stamp,
    first_turn: Option<Id>,
    checked: CheckedKeys,
    /// ふるいの後に残った鍵 (索引に入れる)。
    kept: Vec<SegmentKey>,
    /// 忘れるのふるいで除いた鍵の値の鍵 (埋め込みも消す)。
    forgotten: BTreeSet<ValueKey>,
}

/// 指示の集合の見分け (件数と最大の ID。指示は足すだけなので、変われば違う値になる)。
type Stamp = (usize, Option<Id>);

/// 手順の状態 (このプロセスの間だけ)。
#[derive(Default)]
pub(crate) struct State {
    /// 会話・指示・呼ぶ先・台帳が変わるたびに増える。突き合わせ終えた版と違えば、もう一度突き合わせる。
    version: u64,
    clean: Option<u64>,
    /// 索引に入れた区切りの鍵 (区切りの ID ごと)。起動の後に突き合わせるまでは `None`。
    indexed: Option<HashMap<Id, Indexed>>,
    /// 今の `indexed` の指紋をファイルに書けた。
    saved: bool,
    memo: HashMap<Id, Memo>,
}

fn state(inner: &Inner) -> std::sync::MutexGuard<'_, State> {
    inner.segment_keys.lock().unwrap_or_else(|e| e.into_inner())
}

/// 会話・指示・呼ぶ先・台帳が変わった。
pub(crate) fn touch(inner: &Inner) {
    state(inner).version += 1;
}

/// 要るか (v0.5 の根で、起動時の突き合わせが済み、突き合わせていない変更があるとき)。
pub(crate) fn needed(inner: &Inner) -> bool {
    if crate::old_engine(inner)
        || inner.index.is_none()
        || inner.facts_key.key().is_none()
        || !inner.startup_done.load(std::sync::atomic::Ordering::SeqCst)
    {
        return false;
    }
    let st = state(inner);
    st.clean != Some(st.version)
}

/// 索引に入れた区切り `segment` の鍵 (埋め込みの手順が読む)。
pub(crate) fn indexed(inner: &Inner, segment: Id) -> Option<Indexed> {
    state(inner).indexed.as_ref()?.get(&segment).cloned()
}

/// 索引に入れた区切りの ID (突き合わせる前は空)。
pub(crate) fn indexed_ids(inner: &Inner) -> HashSet<Id> {
    state(inner)
        .indexed
        .as_ref()
        .map(|m| m.keys().copied().collect())
        .unwrap_or_default()
}

/// 試験のため: 索引に入れた区切りの鍵 (区切りの ID, 鍵の文字列)。区切りの ID の順。
pub(crate) fn snapshot(inner: &Inner) -> Vec<(Id, Vec<String>)> {
    let mut out: Vec<(Id, Vec<String>)> = state(inner)
        .indexed
        .as_ref()
        .map(|m| {
            m.iter()
                .map(|(id, x)| (*id, x.keys.iter().map(|k| k.text().to_owned()).collect()))
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|(id, _)| *id);
    out
}

/// 区切りの項目の指紋: 作り方の版・使った台帳・鍵の文字列 (台帳の順) の SHA-256。鍵の値の鍵はデータ根の鍵と鍵の
/// 文字列で決まるので入れない。
fn fingerprint(ledger: Id, keys: &[SegmentKey]) -> String {
    let mut h = Sha256::new();
    h.update(b"evorag.segment_keys.1\0");
    h.update(SEGMENT_KEYS_VERSION.to_be_bytes());
    h.update(ledger.as_bytes());
    for k in keys {
        let t = k.text().as_bytes();
        h.update(u32::try_from(t.len()).unwrap_or(u32::MAX).to_be_bytes());
        h.update(t);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 今の選択と直前の選択 (選び方の入力。呼ぶ先が決まっていなければ、役割の記録のモデルを今の選択とする)。
struct Choice {
    current: Owned,
    build_info: String,
    previous: Option<String>,
}

enum Owned {
    Ready(String),
    Fallback {
        intended: Option<String>,
        called: String,
    },
}

impl Choice {
    fn of(inner: &Inner) -> Self {
        let previous = inner
            .store
            .read()
            .ok()
            .and_then(|r| super::save::previous_key(&r, inner));
        match crate::memory_role::resolve(inner).1 {
            MemoryRoute::Call(c) => {
                let t = super::Target::of(&c);
                let current = if t.fallback {
                    Owned::Fallback {
                        intended: t.role_model_key,
                        called: t.model_key,
                    }
                } else {
                    Owned::Ready(t.model_key)
                };
                Self {
                    current,
                    build_info: t.build_info,
                    previous,
                }
            }
            _ => {
                let role = if inner.memory_configured() {
                    evorag_memory::ROLE_MEMORY
                } else {
                    evorag_memory::ROLE_CHAT
                };
                let recorded = inner
                    .store
                    .read()
                    .ok()
                    .and_then(|r| evorag_memory::load_bindings(&r).ok())
                    .and_then(|b| {
                        b.by_role
                            .get(role)
                            .map(|b| b.body.current.model_key.clone())
                    })
                    .unwrap_or_default();
                Self {
                    current: Owned::Ready(recorded),
                    build_info: String::new(),
                    previous,
                }
            }
        }
    }

    fn current(&self) -> Current<'_> {
        match &self.current {
            Owned::Ready(model_key) => Current::Ready(Selection {
                variant: b2::VARIANT,
                model_key,
            }),
            Owned::Fallback { intended, called } => Current::Fallback {
                variant: b2::VARIANT,
                intended: intended.as_deref(),
                called,
            },
        }
    }
}

/// 記録から求めた、索引に入れる区切りの鍵と消す埋め込み。
struct Plan {
    desired: HashMap<Id, Indexed>,
    /// 消す埋め込みの記録。
    stale: Vec<Id>,
    memo: HashMap<Id, Memo>,
}

/// 指示の本体 (読めない指示があれば `None`)。
fn directive_bodies(r: &ReadTxn<'_>) -> Option<Vec<(Id, DirectiveBody)>> {
    let mut bodies = Vec::new();
    let mut unreadable = 0usize;
    r.for_each(KIND_DIRECTIVE, |x| {
        match x {
            Read::Ok(e) => match e.body_as::<DirectiveBody>() {
                Ok(b) => bodies.push((e.id, b)),
                Err(_) => unreadable += 1,
            },
            _ => unreadable += 1,
        }
        true
    })
    .ok()?;
    if unreadable > 0 {
        tracing::warn!(
            unreadable,
            "directives could not be read; segment keys are not changed"
        );
        return None;
    }
    Some(bodies)
}

/// 台帳 1 つに検査とふるいを当てる (前と同じ入力なら使い直す)。
fn memo_of(
    old: &mut HashMap<Id, Memo>,
    ledger: Id,
    body: &LedgerBody,
    first_turn: Option<Id>,
    cx: (&Rules<'_>, &ForgetKey, &DirectiveSet, Stamp),
) -> Memo {
    let (rules, key, directives, stamp) = cx;
    let raw = &body.output.keys().items;
    if let Some(m) = old.remove(&ledger)
        && m.raw == *raw
        && m.stamp == stamp
        && m.first_turn == first_turn
    {
        return m;
    }
    let checked = check_keys(rules, key, &body.output);
    let empty = BTreeSet::new();
    let screened = checked.screen(&Screen {
        key,
        directives,
        private_units: &empty,
        first_turn,
    });
    let by_index: BTreeMap<usize, ValueKey> = checked.entries().map(|(i, k)| (i, k.vk())).collect();
    let forgotten = screened
        .dropped()
        .iter()
        .filter(|d| matches!(d.reason, KeyDrop::Forgotten(_)))
        .filter_map(|d| by_index.get(&d.index).copied())
        .collect();
    Memo {
        raw: raw.clone(),
        stamp,
        first_turn,
        kept: screened.keys().to_vec(),
        checked,
        forgotten,
    }
}

/// 記録を読んで、索引に入れる区切りの鍵と消す埋め込みを求める (I/O は読み取りだけ)。
fn plan(
    store: &Store,
    lang: PromptLanguage,
    key: &ForgetKey,
    choice: &Choice,
    mut old: HashMap<Id, Memo>,
) -> Option<Plan> {
    let r = store.read().ok()?;
    let bodies = directive_bodies(&r)?;
    let stamp: Stamp = (bodies.len(), bodies.iter().map(|(id, _)| *id).max());
    let directives = DirectiveSet::new(
        bodies,
        evorag_memory::redaction_marker(lang.as_str()),
        str::to_owned,
    );
    let ledgers = super::snapshot::load_ledgers(&r)?;
    let mut segments: HashMap<Id, SegmentBody> = HashMap::new();
    r.for_each(KIND_SEGMENT, |x| {
        if let Read::Ok(e) = x
            && let Ok(b) = e.body_as::<SegmentBody>()
        {
            segments.insert(e.id, b);
        }
        true
    })
    .ok()?;
    let mut revs: HashMap<Id, u32> = HashMap::new();
    r.for_each(KIND_TURN, |x| {
        if let Read::Ok(e) = x
            && let Some(rev) = turn_rev_checked(&e)
        {
            revs.insert(e.id, rev);
        }
        true
    })
    .ok()?;
    let env = SelectEnv {
        current: choice.current(),
        build_info: &choice.build_info,
        previous: choice.previous.as_deref().map(|model_key| Selection {
            variant: b2::VARIANT,
            model_key,
        }),
        versions: RuleVersions::current(CORRECTION_V1),
        turn_rev: |id: Id| revs.get(&id).copied(),
    };
    let rules = Rules::bundled();
    let cx = (&rules, key, &directives, stamp);
    let mut memo: HashMap<Id, Memo> = HashMap::new();
    let mut desired = HashMap::new();
    for (seg, body) in &segments {
        let Some(stored) = ledgers.get(seg) else {
            continue;
        };
        let stored: Vec<StoredLedger<'_>> = stored
            .iter()
            .map(|(id, body)| StoredLedger { id: *id, body })
            .collect();
        let Some(chosen) = select_ledger(*seg, &stored, &env) else {
            continue;
        };
        let id = chosen.ledger.id;
        let m = memo_of(
            &mut old,
            id,
            chosen.ledger.body,
            body.pairs.first().copied(),
            cx,
        );
        if !m.kept.is_empty() {
            desired.insert(
                *seg,
                Indexed {
                    ledger: id,
                    session: body.session,
                    hash: fingerprint(id, &m.kept),
                    keys: m.kept.clone(),
                },
            );
        }
        memo.insert(id, m);
    }
    // 区切りの鍵の埋め込みを持つ台帳 (使っていない台帳を含む) の、今の検査の結果に無い鍵と忘れた鍵の埋め込み。
    let mut stale = Vec::new();
    for (seg, list) in &ledgers {
        let first_turn = segments.get(seg).and_then(|s| s.pairs.first().copied());
        for (id, body) in list {
            let found = evorag_memory::embeddings_of(&r, *id).ok()?;
            let mine: Vec<(Id, Option<ValueKey>)> = found
                .into_iter()
                .filter_map(|(e, b)| {
                    let b = b.filter(|b| b.unit == UNIT_SEGMENT_KEY)?;
                    Some((e, b.vk.as_deref().and_then(ValueKey::from_hex)))
                })
                .collect();
            if mine.is_empty() {
                continue;
            }
            let m = match memo.remove(id) {
                Some(m) => m,
                None => memo_of(&mut old, *id, body, first_turn, cx),
            };
            let live = m.checked.vks();
            stale.extend(
                mine.into_iter()
                    .filter(|(_, vk)| {
                        vk.is_none_or(|v| !live.contains(&v) || m.forgotten.contains(&v))
                    })
                    .map(|(e, _)| e),
            );
            memo.insert(*id, m);
        }
    }
    stale.sort();
    Some(Plan {
        desired,
        stale,
        memo,
    })
}

/// 指紋のファイルの中身。
#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    format: String,
    version: u32,
    /// 区切りの鍵の作り方の版 (読む側は指紋で比べる。人が見て分かるように書く)。
    segment_keys: u32,
    /// 区切りの ID → 指紋。
    segments: BTreeMap<Id, String>,
}

fn state_path(root: &Path) -> PathBuf {
    crate::embedding::memory_dir(root).join(STATE_FILE)
}

/// 指紋を読む。無い・読めない・形式が違うときは空 (すべての区切りを入れ替える)。
fn load_recorded(root: &Path) -> BTreeMap<Id, String> {
    let path = state_path(root);
    let Ok(bytes) = std::fs::read(&path) else {
        return BTreeMap::new();
    };
    match serde_json::from_slice::<StateFile>(&bytes) {
        Ok(f) if f.format == STATE_FORMAT && f.version == STATE_VERSION => f.segments,
        _ => {
            tracing::info!("the segment key fingerprints could not be read; all are rebuilt");
            BTreeMap::new()
        }
    }
}

/// 指紋を書く (一時名に書いて書き切ってから入れ替える)。
fn save_recorded(root: &Path, segments: &BTreeMap<Id, String>) -> std::io::Result<()> {
    let path = state_path(root);
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::other("no index directory"));
    };
    std::fs::create_dir_all(dir)?;
    let body = serde_json::to_vec(&StateFile {
        format: STATE_FORMAT.into(),
        version: STATE_VERSION,
        segment_keys: SEGMENT_KEYS_VERSION,
        segments: segments.clone(),
    })
    .map_err(std::io::Error::other)?;
    let tmp = dir.join(format!("{STATE_FILE}.tmp"));
    std::fs::write(&tmp, body)?;
    evorag_store::fsutil::sync_file(&tmp)?;
    evorag_store::fsutil::rename_durable(&tmp, &path)
}

/// 指紋のファイルを消す (`evorag rebuild` が索引を作り直すとき)。無ければ何もしない。
pub(crate) fn remove_recorded(root: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(state_path(root)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// 区切りの鍵を索引に合わせる (起動の後と sleep-time の手順)。チャンクと索引を変える処理の鍵の下の、落とされない
/// task で行う。合わせられたら `true`。
pub(crate) async fn sync(inner: &Arc<Inner>) -> bool {
    if crate::old_engine(inner) {
        return true;
    }
    let (Some(_), Some(key)) = (inner.index.clone(), inner.facts_key.key().cloned()) else {
        return true;
    };
    let version = state(inner).version;
    // 鍵は spawn の前に取る (終了の処理が鍵を取れたら、出した保存は終わっている)。
    let lock = inner.index_lock.clone().lock_owned().await;
    let worker = inner.clone();
    let task = tokio::spawn(async move {
        let _lock = lock;
        let choice = Choice::of(&worker);
        let old = std::mem::take(&mut state(&worker).memo);
        let store = worker.store.clone();
        let lang = worker.language;
        let planned = tokio::task::spawn_blocking(move || plan(&store, lang, &key, &choice, old))
            .await
            .ok()
            .flatten();
        let Some(plan) = planned else {
            return false;
        };
        apply(&worker, plan).await
    });
    let done = task.await.unwrap_or(false);
    if done {
        state(inner).clean = Some(version);
    }
    done
}

/// 求めた鍵を索引に当てる (呼び手は `index_lock` を持つ)。
async fn apply(inner: &Arc<Inner>, plan: Plan) -> bool {
    let Plan {
        desired,
        stale,
        memo,
    } = plan;
    state(inner).memo = memo;
    if !stale.is_empty() {
        let w = crate::writes(inner);
        let ops: Vec<_> = stale
            .iter()
            .flat_map(|id| w.delete_embedding(*id))
            .collect();
        if let Err(e) = crate::write_derived(inner, ops).await {
            tracing::warn!(error = %e, "stale segment key embeddings could not be removed");
            return false;
        }
        tracing::info!(
            removed = stale.len(),
            "stale segment key embeddings removed"
        );
    }
    let Some(ix) = inner.index.clone() else {
        return true;
    };
    let root = inner.store.data_root().to_path_buf();
    // 索引に入れた鍵の指紋 (起動の後の最初の回は、前の起動が書いたファイルから)。
    let known = {
        let st = state(inner);
        st.indexed.as_ref().map(|m| {
            let hashes: BTreeMap<Id, String> =
                m.iter().map(|(id, x)| (*id, x.hash.clone())).collect();
            (hashes, st.saved)
        })
    };
    let (recorded, saved) = match known {
        Some(k) => k,
        None => {
            let r2 = root.clone();
            let loaded = tokio::task::spawn_blocking(move || load_recorded(&r2))
                .await
                .unwrap_or_default();
            (loaded, true)
        }
    };
    let in_lex = ix.snapshot().ids_where(crate::recall::is_segment_key);
    let in_vec: HashSet<Id> = crate::embedding::current(inner)
        .map(|(_, v)| {
            v.snapshot()
                .ids_where(|r| r.session == crate::recall::SEGMENT_KEY_SESSION)
        })
        .unwrap_or_default();
    // 入れ替える区切り (指紋が違う・語彙の索引に無い) と、除く区切り (今は鍵が無い)。
    let changed: BTreeSet<Id> = desired
        .iter()
        .filter(|(id, x)| recorded.get(id) != Some(&x.hash) || !in_lex.contains(id))
        .map(|(id, _)| *id)
        .collect();
    let removed: BTreeSet<Id> = recorded
        .keys()
        .chain(&in_lex)
        .chain(&in_vec)
        .filter(|id| !desired.contains_key(id))
        .copied()
        .collect();
    let want: BTreeMap<Id, String> = desired
        .iter()
        .map(|(id, x)| (*id, x.hash.clone()))
        .collect();
    if changed.is_empty() && removed.is_empty() {
        state(inner).indexed = Some(desired);
        if saved && want == recorded {
            return true;
        }
        return persist(inner, &root, want).await;
    }
    // 入れ替える前に、入れ替える区切りを除いた指紋を書く (途中で止まったら、次の起動で入れ替え直す)。
    let kept: BTreeMap<Id, String> = recorded
        .iter()
        .filter(|(id, _)| !changed.contains(id) && !removed.contains(id))
        .map(|(id, h)| (*id, h.clone()))
        .collect();
    {
        let r2 = root.clone();
        let saved = tokio::task::spawn_blocking(move || save_recorded(&r2, &kept)).await;
        if !matches!(saved, Ok(Ok(()))) {
            tracing::warn!("the segment key fingerprints could not be written; keys wait");
            return false;
        }
    }
    let gone: HashSet<Id> = changed.iter().chain(&removed).copied().collect();
    let docs: Vec<_> = changed
        .iter()
        .flat_map(|id| {
            crate::recall::segment_docs(*id, desired[id].keys.iter().map(SegmentKey::text))
        })
        .collect();
    let (n_changed, n_removed) = (changed.len(), removed.len());
    // 埋め込みの手順が新しい鍵を読むように、索引を変える前に入れ替える (鍵の下なので、その間に保存は来ない)。
    {
        let mut st = state(inner);
        st.indexed = Some(desired);
        st.saved = false;
    }
    let worker = inner.clone();
    let done = tokio::task::spawn_blocking(move || -> std::io::Result<bool> {
        let vectors = crate::embedding::remove_everywhere(&worker, &gone);
        ix.remove(&gone)?;
        if !docs.is_empty() {
            ix.add(docs)?;
        }
        Ok(vectors)
    })
    .await;
    match done {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            // ベクトルの索引から除けなかった行は、次の回で入れ替え直す (指紋は書かない)。
            tracing::warn!("segment keys could not be removed from a vector index");
            return false;
        }
        _ => {
            tracing::warn!("segment keys could not be written to the lexical index");
            return false;
        }
    }
    tracing::info!(
        changed = n_changed,
        removed = n_removed,
        "segment keys synced"
    );
    persist(inner, &root, want).await
}

/// 今の指紋を書く。
async fn persist(inner: &Inner, root: &Path, want: BTreeMap<Id, String>) -> bool {
    let r2 = root.to_path_buf();
    let saved = tokio::task::spawn_blocking(move || save_recorded(&r2, &want)).await;
    let ok = matches!(saved, Ok(Ok(())));
    if !ok {
        tracing::warn!("the segment key fingerprints could not be written");
    }
    state(inner).saved = ok;
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fingerprint_follows_the_ledger_and_the_key_texts() {
        let key = ForgetKey::from_bytes([7; 32]);
        let rules = Rules::bundled();
        let keys = |raw: &[&str]| -> Vec<SegmentKey> {
            evorag_memory::segment_keys::check_strings(&rules, &key, raw.iter().copied())
                .keys()
                .cloned()
                .collect()
        };
        let a = Id::from_bytes([1; 16]);
        let b = Id::from_bytes([2; 16]);
        let base = fingerprint(a, &keys(&["京都の暮らし", "将棋"]));
        assert_eq!(base, fingerprint(a, &keys(&["京都の暮らし", "将棋"])));
        assert_ne!(
            base,
            fingerprint(b, &keys(&["京都の暮らし", "将棋"])),
            "another ledger"
        );
        assert_ne!(
            base,
            fingerprint(a, &keys(&["将棋", "京都の暮らし"])),
            "the order"
        );
        assert_ne!(base, fingerprint(a, &keys(&["京都の暮らし"])), "a key less");
        // 長さを前に置くので、区切り方の違う同じ文字の並びと取り違えない。
        assert_ne!(
            fingerprint(a, &keys(&["ab", "c"])),
            fingerprint(a, &keys(&["a", "bc"]))
        );
    }
}
