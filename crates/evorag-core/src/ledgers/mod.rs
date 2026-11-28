//! 区切りと、区切りごとの 1 回の呼び出し (B2) の sleep-time の手順 (04 §4 の手順 2・3、v0.5 の根)。
//!
//! 1. **区切り** (手順 2、LLM を使わない): チャットのセッションごとに、記録した区切りと台帳から戻せる区切り
//!    (04 §2 の「台帳から戻す細目」) を前提に区切りを決め ([`evorag_memory::plan_segments`])、変わった区切りを書く
//!    (閉じた区切りは `Immediate`、03 §3.4)。`memory.forget_key` が使えなくても動く。
//! 2. **台帳** (手順 3): 呼び直すかの判定 ([`evorag_memory::needs_call`]) に当たる区切りを、最初の往復の順に、
//!    `memory` の役割の呼ぶ先 ([`crate::MemoryCall`]) で呼び、台帳を `Immediate` で書く (04 §5.2.1)。区切りごとに、
//!    呼ぶ前に呼ぶ先のモデルを確かめる ([`crate::MemoryCall::verify`])。
//!
//! 進み具合は状態から求めるので job を書かない (04 §4)。利用者が操作したら、流れている要求を 1 秒以内に止め
//! (ストリームを落として接続を閉じる、06 §4)、型の上で完結した要素を `partial` の台帳に残す (04 §5.2.1 の
//! 「止められたとき」)。この手順は裏の処理の future を落とさずに自分で止める ([`crate::sleep`])。
//!
//! 忘れるの指示は、`[非表示]` の範囲を印に置き換えてから渡し、指示と合わないターンは渡さない。忘れるの指示の
//! `turns`・`redactions`・`hidden` に入る発話は訂正の候補にしない (04 §6・§7.5)。private の値は伏せずに渡す
//! (04 §5.2)。台帳は書く前に、単位を書く経路のロックの下で、出典のターンの改訂の番号を確かめ、効いているすべての
//! 忘れるの指示で掃除する (04 §4)。旧エンジンの根 ([`crate::RootFormat::V04Bench`]) では動かさない。
//!
//! - `snapshot`: 記録を読んで区切りを決め、呼ぶ区切りの入力を作る。
//! - `flow`: 区切り 1 つの呼び出しの進め方 (I/O の無い状態機械)。
//! - `call`: 止め方・要求の受け取り・区切り 1 つの呼び出し。
//! - `save`: 台帳と区切りを書く。
//! - `keys`: 使う台帳の `keys` から区切りの鍵を作り、語彙とベクトルの索引に入れる (04 §5.5、手順 5)。

mod call;
mod flow;
pub(crate) mod keys;
mod save;
mod snapshot;

use std::collections::HashMap;
use std::sync::Arc;

use evorag_llm::{ChatMessage, ChatRequest, Generation, Role};
use evorag_memory::b2::{self, Request};
use evorag_memory::{Current, Selection};
use evorag_types::Id;

use crate::{Inner, MemoryCall, MemoryRoute};
use call::{Calling, Done, Yield};

/// 手順の状態 (このプロセスの間だけ)。
#[derive(Default)]
pub(crate) struct State {
    /// 会話・指示・呼ぶ先が変わるたびに増える。突き合わせ終えた版と違えば、もう一度突き合わせる。
    version: u64,
    clean: Option<u64>,
    /// 開いている末尾の区切りが閉じる、いちばん早い時刻 (ミリ秒)。
    due_at: Option<i64>,
    /// 番号を振った文はあるのに 1 文も送れなかった区切り (04 §5.2.1 の「全部を送れなかった区切り」) と、そのときの
    /// 区切りの往復の数。台帳を書かないので、往復が変わるまで (か、次の起動まで) 飛ばす。
    unsent: HashMap<Id, usize>,
    /// 締切切れと 5xx が続いた区切りと、その回数 (台帳の `attempts`)。
    server_errors: HashMap<Id, u32>,
    /// 続けて 4xx か、締切切れ・5xx で終わった区切りの数。
    consecutive: u32,
    /// 呼び出しを止めた呼ぶ先と理由。呼ぶ先 (model_key・`build_info`・要求の付け足し) が変われば試し直す。
    halted: Option<(HaltKey, HaltReason)>,
}

fn state(inner: &Inner) -> std::sync::MutexGuard<'_, State> {
    inner.ledgers.lock().unwrap_or_else(|e| e.into_inner())
}

/// 会話・指示・呼ぶ先が変わった (ターンを書いた、セッションを消した、忘れる、点検・役割の変化)。区切りの鍵は、
/// 使う台帳か指示か呼ぶ先が変わりうるところで別に突き合わせ直す ([`keys::touch`]。ターンを足しただけでは
/// 変わらない)。
pub(crate) fn touch(inner: &Inner) {
    state(inner).version += 1;
}

/// 区切りの呼び出しを止めた理由 (状態の行と `doctor` の材料、04 §5.2.1 の「失敗の扱い」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HaltReason {
    /// 続けて 3 区切りが 4xx か、締切切れ・5xx で終わった。
    Failures,
    /// 構成の誤り (接続先の形・ループバック以外・ルーターのモデルの名前が無い)。
    Misconfigured,
}

/// 試験と `doctor` のための数 ([`crate::Core::ledger_stats`])。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LedgerStats {
    /// 番号を振った文はあるのに、予算に入らず 1 文も送れなかった区切りの数 (このプロセスの間。台帳を書かない、
    /// 04 §5.2.1)。
    pub nothing_sent: usize,
    /// 区切りの呼び出しを止めている理由 (止めていなければ `None`)。
    pub halted: Option<HaltReason>,
    /// このプロセスの間に、区切りの鍵の項目を入れ替えた区切りの数 (指紋が違った・語彙の索引に無かった、04 §5.5)。
    pub segment_keys_changed: usize,
    /// このプロセスの間に、区切りの鍵の項目を除いた区切りの数 (鍵が無くなった・区切りが消えた)。
    pub segment_keys_removed: usize,
}

pub(crate) fn stats(inner: &Inner) -> LedgerStats {
    let (segment_keys_changed, segment_keys_removed) = keys::counts(inner);
    let st = state(inner);
    LedgerStats {
        nothing_sent: st.unsent.len(),
        halted: st.halted.as_ref().map(|(_, r)| *r),
        segment_keys_changed,
        segment_keys_removed,
    }
}

/// 要るか (状態から求める。突き合わせていない変更があるか、末尾の区切りが閉じる時刻が来たか)。v0.5 の根で、
/// 起動時の突き合わせが済んだときだけ。`memory.forget_key` が使えなくても、区切りを決める (手順 2。LLM を使わない)
/// ために動き、呼び出し (手順 3) だけを止める (04 §5.2.1 の「止めるとき」)。
pub(crate) fn needed(inner: &Inner) -> bool {
    if crate::old_engine(inner) || !inner.startup_done.load(std::sync::atomic::Ordering::SeqCst) {
        return false;
    }
    let st = state(inner);
    st.clean != Some(st.version)
        || st
            .due_at
            .is_some_and(|t| inner.clock.now().as_millis() >= t)
}

/// B2 の要求 (06 §6: 型縛り・temperature 0・固定の seed・固定のスロット・要求の付け足し)。
pub(crate) fn chat_request(
    request: &Request,
    extra: &evorag_llm::RequestExtra,
    slot: Option<u32>,
) -> ChatRequest {
    let mut extra = extra.clone();
    extra.insert("response_format".into(), request.response_format());
    ChatRequest {
        messages: vec![
            ChatMessage::new(Role::System, request.system),
            ChatMessage::new(Role::User, request.user.clone()),
        ],
        generation: Generation {
            temperature: 0.0,
            top_p: 1.0,
            max_tokens: Some(b2::MAX_TOKENS),
            seed: Some(1),
        },
        id_slot: slot,
        model: None,
        extra,
    }
}

/// 止めを解くかを比べる呼ぶ先 (model_key・`build_info`・要求の付け足しのハッシュ)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HaltKey {
    model_key: String,
    build_info: String,
    extra_hash: String,
}

/// 呼ぶ先の、選び方と書き込みに要る値。
#[derive(Debug, Clone)]
pub(crate) struct Target {
    model_key: String,
    build_info: String,
    fallback: bool,
    role_model_key: Option<String>,
    extra_hash: String,
}

impl Target {
    fn of(c: &MemoryCall) -> Self {
        Self {
            model_key: c.model_key.clone(),
            build_info: c.build_info.clone(),
            fallback: c.is_fallback(),
            role_model_key: c.role_model_key.clone(),
            extra_hash: crate::probe::extra_hash(&c.extra),
        }
    }

    /// 今の選択 (選び方の「今」の段と、置き換えて消す台帳の見分けに使う)。
    fn current(&self) -> Current<'_> {
        if self.fallback {
            Current::Fallback {
                variant: b2::VARIANT,
                intended: self.role_model_key.as_deref(),
                called: &self.model_key,
            }
        } else {
            Current::Ready(Selection {
                variant: b2::VARIANT,
                model_key: &self.model_key,
            })
        }
    }

    fn halt_key(&self) -> HaltKey {
        HaltKey {
            model_key: self.model_key.clone(),
            build_info: self.build_info.clone(),
            extra_hash: self.extra_hash.clone(),
        }
    }
}

/// 止めた呼び出しを、今の呼ぶ先でも止めたままにするか (止めた呼ぶ先と違えば止めを解く)。
fn halted_for(halted: Option<&(HaltKey, HaltReason)>, now: &HaltKey) -> bool {
    halted.is_some_and(|(k, _)| k == now)
}

/// 呼ぶ先が変わったか、繋がらなかった。設定した memory のサーバなら、接続待ちに戻して記録と照合し直し、その間は
/// chat で代行する (すぐに続ける)。chat なら、応答の経路の照合 (06 §3) に任せて待つ。
fn target_lost(inner: &Arc<Inner>, call: &MemoryCall) -> Option<bool> {
    if !call.is_fallback() && inner.memory_configured() {
        crate::memory_role::mark_unreachable(inner);
        Some(true)
    } else {
        Some(false)
    }
}

/// 手順を動かす。利用者が操作したら、流れている要求を止め、止めたところまでを書いて `None` を返す。終えたら、
/// 進んだか (区切りか台帳を 1 つでも書いたか。すぐに続けるときも真)。
pub(crate) async fn run(inner: &Arc<Inner>, generation: u64) -> Option<bool> {
    let mut stop = Yield::new(inner);
    if inner.activity.generation() != generation {
        return None;
    }
    let version = state(inner).version;
    // `memory.forget_key` が使えなければ、区切りだけを決めて呼ばない (03 §3.1.1 のフェイルクローズ)。
    let call = match crate::memory_role::resolve(inner).1 {
        MemoryRoute::Call(c) if inner.facts_key.key().is_some() => {
            let key = Target::of(&c).halt_key();
            let mut st = state(inner);
            if halted_for(st.halted.as_ref(), &key) {
                None
            } else {
                if st.halted.take().is_some() {
                    st.consecutive = 0;
                }
                Some(c)
            }
        }
        _ => None,
    };
    let target = call.as_ref().map(Target::of);
    let unsent = state(inner).unsent.clone();
    let store = inner.store.clone();
    let lang = inner.language;
    let now = inner.clock.now().as_millis();
    let previous = inner
        .store
        .read()
        .ok()
        .and_then(|r| save::previous_key(&r, inner));
    let target_for_read = target.clone();
    // 読んでから区切りを書き終えるまで、単位を書く経路のロックを持つ (その間にセッションを消したら、消した
    // セッションの区切りを書き戻さないため。落とされても、終了の処理がこのロックを取れたら、読み終えてストアを
    // 手放している)。
    let lock = inner.facts_lock.clone().lock_owned().await;
    let (lock, snap) = tokio::task::spawn_blocking(move || {
        let judge = target_for_read.as_ref().map(|t| snapshot::Judge {
            target: t,
            previous,
            unsent: &unsent,
        });
        (lock, snapshot::read(&store, lang, now, judge.as_ref()))
    })
    .await
    .ok()?;
    let Some(snap) = snap else {
        return Some(false);
    };
    let mut progressed = !snap.writes.is_empty();
    if progressed {
        keys::touch(inner);
    }
    let wrote = save::write_segments(inner, snap.writes).await;
    drop(lock);
    if !wrote {
        return Some(false);
    }
    state(inner).due_at = snap.due_at;
    let (Some(call), Some(target)) = (call, target) else {
        state(inner).clean = Some(version);
        return Some(progressed);
    };
    let calling = Calling {
        inner,
        call: &call,
        target: &target,
        forgets_seen: snap.forgets_seen,
    };
    for work in &snap.work {
        // 読んだ後に会話・指示・呼ぶ先が変わったら、読み直す (読んだ時の本文を使い回さない)。
        if state(inner).version != version {
            return Some(true);
        }
        // 区切りを呼ぶ前ごとに、呼ぶ先のモデルと `build_info` が選んだ時と同じかを確かめる (06 §2.1 の細目)。
        let info = match stop.run(call.verify()).await? {
            Ok(info) if info.build_info == target.build_info => info,
            Ok(info) => {
                tracing::warn!(
                    build_info = info.build_info,
                    "the segment call target runs another build"
                );
                return target_lost(inner, &call);
            }
            Err(e) => {
                tracing::warn!(error = %e, "the segment call target changed or is not reachable");
                return target_lost(inner, &call);
            }
        };
        if !usize::try_from(info.n_ctx).is_ok_and(b2::fits) {
            tracing::warn!(
                n_ctx = info.n_ctx,
                "the slot context is too small for segment calls; they wait"
            );
            return Some(false);
        }
        match calling.segment(work, &info.build_info, &mut stop).await {
            Done::Written => {
                progressed = true;
                // 使う台帳が替わりうるので、区切りの鍵を突き合わせ直す (04 §5.5 の「作り直し」)。
                keys::touch(inner);
            }
            Done::Skipped | Done::Unsent | Done::Rejected => {}
            Done::Stopped => return None,
            // 次の機会に送り直す (すぐには送り直さず、裏の処理の「繰り返しを防ぐ」で待たせる、04 §4)。止めた
            // ときは、次の回の突き合わせで、呼ぶ先が変わるまで呼ばない。応答のスロットが処理中のときも待たせる。
            Done::Busy | Done::Retry | Done::Halted => return Some(false),
            Done::Unreachable => return target_lost(inner, &call),
        }
    }
    state(inner).clean = Some(version);
    Some(progressed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(extra: &str) -> HaltKey {
        HaltKey {
            model_key: "m".into(),
            build_info: "b".into(),
            extra_hash: extra.into(),
        }
    }

    #[test]
    fn a_halt_holds_only_for_the_same_target_and_settings() {
        let halted = (key("e1"), HaltReason::Failures);
        assert!(halted_for(Some(&halted), &key("e1")));
        assert!(
            !halted_for(Some(&halted), &key("e2")),
            "the request settings changed"
        );
        let mut other = key("e1");
        other.build_info = "b2".into();
        assert!(!halted_for(Some(&halted), &other));
        assert!(!halted_for(None, &key("e1")));
    }
}
