//! 先に済ませる処理 (05 §4.5)。
//!
//! - **次のターンの分**: 応答を書いた直後と、セッションを開いた直後に、次のターンの要求の先頭 (順 1〜5) を、
//!   生成を 1 トークンに限った要求で応答のスロットへ送り、llama-server に処理させておく。応答は捨てる。
//! - **入力が止まったとき**: 画面が知らせた送る前の発話 (下書き) で、問いの準備・候補の収集・リランクを先に
//!   済ませ、添えるものがあれば大きな予算で引用の枠を作る (投機的な先読み、v0.4、OQ-50)。枠は「履歴 + 枠の
//!   メッセージ + 空の発話」の形で応答のスロットへ送り、llama-server に処理させておく。枠は保持するだけで
//!   保存しない。送った発話で枠が同じなら、応答の経路がそのバイト列をそのまま使う。
//! - **先読みの世代**: 忘れる・private・削除・事実の表の差し替えのたびに上げ、古い世代の枠を捨てる。
//! - 下書きは、ループバックの接続先の役割にだけ送る (役割ごとに判定する)。保存せず、ログに出さない。
//!
//! どれも、応答の経路が始まったら止める (task を落とすと、要求の接続が閉じる、06 §4)。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use evorag_llm::{ChatMessage, ChatRequest, Role};
use evorag_types::Id;

use crate::power::Power;
use crate::recall::{self, Attached, RerankMode};
use crate::sleep::OnBattery;
use crate::{Inner, context};

/// 先読みした引用の枠 (05 §4.5)。主記憶にだけ置く。
#[derive(Clone)]
pub(crate) struct Prefetched {
    pub(crate) session: Id,
    /// 下書き (送った発話と同じかを比べるためだけに持つ)。
    pub(crate) draft: String,
    pub(crate) attached: Attached,
    pub(crate) generation: u64,
}

/// 先に済ませる処理の task。
#[derive(Default)]
pub(crate) struct Tasks {
    warm: Mutex<Option<tokio::task::JoinHandle<()>>>,
    idle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// 送られた・受け取った「入力が止まった」の命令の数 (試験で、命令が届くまで待つため)。
    pub(crate) asked: AtomicU64,
    seen: AtomicU64,
    /// 先読みの世代 (05 §4.5)。
    generation: AtomicU64,
    /// 先読みした枠 (いちばん新しいもの 1 つ)。
    prefetched: Mutex<Option<Prefetched>>,
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

    /// 次のターンの分を処理させている (sleep-time の手順はその後、04 §4)。
    pub(crate) fn warming(&self) -> bool {
        self.warm
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|t| !t.is_finished())
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

    /// 今の先読みの世代。
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// 先読みの世代を上げ、先読みした枠を捨て、入力が止まったときの処理を止める (忘れる・private・削除・
    /// 事実の表の差し替え、05 §4.5)。止めた task を返す (忘れるは、止まったことを確かめてからスロットの
    /// キャッシュの消去を求める)。
    pub(crate) fn bump(&self) -> Option<tokio::task::JoinHandle<()>> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.prefetched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .inspect(tokio::task::JoinHandle::abort)
    }

    /// 先読みした枠を取り出す (このセッションの、今の世代のものだけ)。
    pub(crate) fn take(&self, session: Id) -> Option<Prefetched> {
        let p = self
            .prefetched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        (p.session == session && p.generation == self.generation()).then_some(p)
    }

    fn keep(&self, p: Prefetched) {
        if p.generation == self.generation() {
            *self.prefetched.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
        }
    }
}

/// 電池の設定 (07 §4.2 の `memory.on_battery`)。`stop` は先に済ませる処理をすべて止め、`reduce` は
/// 入力が止まったときの処理だけを止める。
fn on_battery(inner: &Inner) -> Option<OnBattery> {
    ((inner.probe.power)() == Power::Battery).then_some(inner.sleep.on_battery)
}

/// 応答のスロットに、生成を 1 トークンに限った要求を送り、終わりまで読んで捨てる。
async fn prime(inner: &Inner, messages: Vec<ChatMessage>) {
    let (llm, server, _) = inner.conn();
    let mut generation = inner.generation.clone();
    generation.max_tokens = Some(1);
    let req = ChatRequest {
        messages,
        generation,
        // 応答と同じスロット (06 §4)。
        id_slot: (server.total_slots > 1).then_some(0),
        model: None,
        extra: inner.request_extra.clone(),
    };
    if let Ok(mut stream) = llm.chat(req).await {
        while let Some(Ok(_)) = stream.next().await {}
    }
}

/// 次の発話の見出しまでを同じ先頭にするための、空の発話 (まだ無い発話より後の ID)。
fn empty_utterance() -> (Id, ChatMessage) {
    (
        Id::from_bytes([0xff; 16]),
        ChatMessage::new(Role::User, String::new()),
    )
}

/// 応答を書いた直後と、セッションを開いた直後に、次のターンの分を先に処理させる。
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
        // 次のターンと同じシステムプロンプト (置いている事実の表を含む)。
        let defaults = *worker.answer.lock().unwrap_or_else(|e| e.into_inner());
        let (system, _) = crate::system_for_next(&worker, session, &defaults, &conv).await;
        // 今の発話の枠は空にする (次の発話の見出しまでが同じ先頭になる)。
        let assembled = context::assemble(
            &*llm,
            &worker.tokens,
            &server.model_key,
            n_ctx,
            reserve,
            system,
            conv.past,
            None,
            empty_utterance(),
            conv.from,
        )
        .await;
        prime(&worker, assembled.messages).await;
    });
    Tasks::replace(&inner.pre.warm, t);
}

/// 入力が止まった (05 §4.5)。送る前の発話 `text` で、想起と先読みを先に済ませる。
pub(crate) fn input_idle(inner: &Arc<Inner>, text: String) {
    start_idle(inner, text);
    // 受け取ったと数えるのは、task を登録した後 (試験の待ち合わせが、登録の前に終わらないように)。
    inner.pre.seen.fetch_add(1, Ordering::SeqCst);
}

/// 下書きを送れないとき (ループバック以外の接続先) の、下書きの想起の縮退の理由。記録しない。
const REMOTE: &str = "remote";

fn start_idle(inner: &Arc<Inner>, text: String) {
    if !inner.precompute || on_battery(inner).is_some_and(|b| b != OnBattery::Normal) {
        return;
    }
    if text.trim().is_empty() || crate::is_running(inner) || crate::forgetting(inner) {
        return;
    }
    let Some(session) = crate::current_session(inner) else {
        return;
    };
    if !crate::memory_on(inner, session) {
        return;
    }
    let generation = inner.pre.generation();
    let worker = inner.clone();
    let t = tokio::spawn(async move {
        let (llm, server, _) = worker.conn();
        // 下書きは、ループバックの接続先の役割にだけ送る (役割ごとに判定する、05 §4.5)。
        let vectors = worker.vector_query().filter(|v| v.client.is_loopback());
        let mode = match worker.rerank_mode() {
            RerankMode::Use(u) if !u.client.is_loopback() => RerankMode::Lexical(REMOTE),
            m => m,
        };
        let n_ctx = server.n_ctx as usize;
        let reserve = context::reserve_for_output(n_ctx, worker.reserve);
        let conv = crate::conversation(&worker, session, Id::from_bytes([0xff; 16]));
        let defaults = *worker.answer.lock().unwrap_or_else(|e| e.into_inner());
        let (system, table) = crate::system_for_next(&worker, session, &defaults, &conv).await;
        let budgets = context::frame_budgets(n_ctx, reserve, table.is_some());
        let attached = recall::draft_attach(
            &worker.store,
            worker.index.as_deref(),
            worker.language,
            &*llm,
            &worker.tokens,
            &server.model_key,
            budgets.recall,
            session,
            text.clone(),
            conv.next_source,
            &mode,
            vectors.as_ref(),
        )
        .await;
        // 枠の先読みは、応答の接続先がループバックのときだけ。添えるものが無ければ、次のターンの分のまま。
        let Some(quote) = attached.quote.clone() else {
            return;
        };
        if !llm.is_loopback() || worker.pre.generation() != generation {
            return;
        }
        let assembled = context::assemble(
            &*llm,
            &worker.tokens,
            &server.model_key,
            n_ctx,
            reserve,
            system,
            conv.past,
            Some(ChatMessage::new(Role::User, quote)),
            empty_utterance(),
            conv.from,
        )
        .await;
        worker.pre.keep(Prefetched {
            session,
            draft: text,
            attached,
            generation,
        });
        prime(&worker, assembled.messages).await;
    });
    Tasks::replace(&inner.pre.idle, t);
}
