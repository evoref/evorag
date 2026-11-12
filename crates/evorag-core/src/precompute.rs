//! 先に済ませる処理 (05 §4.5)。
//!
//! - **次のターンの分**: 応答を書いた直後に、次のターンの要求の先頭 (順 1〜5) を、生成を 1 トークンに
//!   限った要求で応答のスロットへ送り、llama-server に処理させておく。応答は捨てる。
//! - **入力が止まったとき**: 画面が知らせた送る前の発話で、問いの準備・候補の収集・リランクを先に済ませる。
//!   結果は問いの埋め込みとリランクのキャッシュ (recall.rs) に残るだけで、保存しない。送った発話が同じなら
//!   応答の経路がキャッシュから使い、違えば使われずに消える。
//!
//! どちらも、応答の経路が始まったら止める (task を落とすと、要求の接続が閉じる、06 §4)。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use evorag_llm::{ChatMessage, ChatRequest, Role};
use evorag_types::Id;

use crate::power::Power;
use crate::sleep::OnBattery;
use crate::{Inner, context, prompt};

/// 先に済ませる処理の task。
#[derive(Default)]
pub(crate) struct Tasks {
    warm: Mutex<Option<tokio::task::JoinHandle<()>>>,
    idle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// 送られた・受け取った「入力が止まった」の命令の数 (試験で、命令が届くまで待つため)。
    pub(crate) asked: AtomicU64,
    seen: AtomicU64,
}

impl Tasks {
    fn replace(slot: &Mutex<Option<tokio::task::JoinHandle<()>>>, t: tokio::task::JoinHandle<()>) {
        if let Some(old) = slot.lock().unwrap_or_else(|e| e.into_inner()).replace(t) {
            old.abort();
        }
    }

    /// 動いているものを止める (応答の経路が始まった、終了する)。止めた task を返す (終了のときに待つ)。
    pub(crate) fn stop(&self) -> Vec<tokio::task::JoinHandle<()>> {
        [&self.warm, &self.idle]
            .into_iter()
            .filter_map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).take())
            .inspect(tokio::task::JoinHandle::abort)
            .collect()
    }

    /// 届いていない命令か、動いているものがある (試験で待つため)。
    pub(crate) fn busy(&self) -> bool {
        self.seen.load(Ordering::SeqCst) < self.asked.load(Ordering::SeqCst)
            || [&self.warm, &self.idle].into_iter().any(|s| {
                s.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .is_some_and(|t| !t.is_finished())
            })
    }
}

/// 電池の設定 (07 §4.2 の `memory.on_battery`)。`stop` は先に済ませる処理をすべて止め、`reduce` は
/// 入力が止まったときの処理だけを止める。
fn on_battery(inner: &Inner) -> Option<OnBattery> {
    ((inner.probe.power)() == Power::Battery).then_some(inner.sleep.on_battery)
}

/// 応答を書いた直後に、次のターンの分を先に処理させる。
pub(crate) fn warm_next(inner: &Arc<Inner>, session: Id) {
    if on_battery(inner) == Some(OnBattery::Stop) {
        return;
    }
    let worker = inner.clone();
    let t = tokio::spawn(async move {
        let (llm, server, _) = worker.conn();
        let n_ctx = server.n_ctx as usize;
        let reserve = context::reserve_for_output(n_ctx, worker.reserve);
        // 今の応答までを履歴に入れる (まだ無い発話より後の ID で切る)。
        let conv = crate::conversation(&worker, session, Id::from_bytes([0xff; 16]));
        let defaults = *worker.answer.lock().unwrap_or_else(|e| e.into_inner());
        let system = ChatMessage::new(
            Role::System,
            prompt::system_prompt(worker.language, &defaults),
        );
        // 今の発話の枠は空にする (次の発話の見出しまでが同じ先頭になる)。
        let assembled = context::assemble(
            &*llm,
            &worker.tokens,
            &server.model_key,
            n_ctx,
            reserve,
            system,
            conv.past,
            (
                Id::from_bytes([0xff; 16]),
                ChatMessage::new(Role::User, String::new()),
            ),
            conv.from,
        )
        .await;
        let mut generation = worker.generation.clone();
        generation.max_tokens = Some(1);
        let req = ChatRequest {
            messages: assembled.messages,
            generation,
            // 応答と同じスロット (06 §4)。
            id_slot: (server.total_slots > 1).then_some(0),
            model: None,
            extra: worker.request_extra.clone(),
        };
        if let Ok(mut stream) = llm.chat(req).await {
            // 終わりまで読んで捨てる。
            while let Some(Ok(_)) = stream.next().await {}
        }
    });
    Tasks::replace(&inner.pre.warm, t);
}

/// 入力が止まった (05 §4.5)。送る前の発話 `text` で、想起を先に済ませる。
pub(crate) fn input_idle(inner: &Arc<Inner>, text: String) {
    start_idle(inner, text);
    // 受け取ったと数えるのは、task を登録した後 (試験の待ち合わせが、登録の前に終わらないように)。
    inner.pre.seen.fetch_add(1, Ordering::SeqCst);
}

fn start_idle(inner: &Arc<Inner>, text: String) {
    if !inner.precompute || on_battery(inner).is_some_and(|b| b != OnBattery::Normal) {
        return;
    }
    if text.trim().is_empty() || crate::is_running(inner) {
        return;
    }
    let Some(session) = crate::current_session(inner) else {
        return;
    };
    if !crate::memory_on(inner, session) {
        return;
    }
    let worker = inner.clone();
    let t = tokio::spawn(async move {
        crate::recall::precompute(
            &worker.store,
            worker.index.as_deref(),
            worker.language,
            session,
            text,
            &worker.rerank_mode(),
            worker.vector_query().as_ref(),
        )
        .await;
    });
    Tasks::replace(&inner.pre.idle, t);
}
