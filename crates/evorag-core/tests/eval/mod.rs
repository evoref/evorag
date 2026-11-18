//! 評価 (09 §5.2) の道具: 実際の llama-server への接続、コアの設定、会話の書き込み、待ち合わせ、点検の結果の
//! 持ち越し、決定論の判定。`live_facts.rs` と `longmemeval.rs` が使う。

#![allow(
    dead_code,
    reason = "each test file uses a different part of the helpers"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::{Endpoint, Generation, HttpClient, LlmClient, RequestExtra, RetryPolicy};
use evorag_memory::{AssistantTurn, ModelProbeBody, ThresholdBody, TurnState, Writes};
use evorag_store::{OpenOptions, Read, Store, WriteRequest, Writer};
use evorag_types::api::{Command, Event, FactRow, FactSubject, TurnView};
use evorag_types::{Clock, ManualClock, SystemClock, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;
use unicode_normalization::UnicodeNormalization;

/// 応答の生成の上限 (トークン)。答えが長くなりすぎて判定の時間が延びないように。
const ANSWER_MAX_TOKENS: u32 = 512;
/// 答えの生成の固定の seed (同じモデルとデータなら同じ答えに近づける)。
const ANSWER_SEED: u64 = 42;

/// 接続先 (環境変数)。応答の llama-server は必須、埋め込みとリランカーは両方あるときだけ使う。
#[derive(Debug, Clone)]
pub struct Servers {
    pub chat: String,
    pub api_key: Option<String>,
    /// (埋め込み, リランカー)。
    pub aux: Option<(String, String)>,
}

impl Servers {
    /// `EVORAG_LIVE_LLM_URL` (必須)、`EVORAG_LIVE_LLM_API_KEY` (任意)、`EVORAG_LIVE_EMBED_URL` と
    /// `EVORAG_LIVE_RERANK_URL` (任意。両方あるときだけ使う)。応答の URL が無ければ `None`。
    pub fn from_env() -> Option<Self> {
        let chat = std::env::var("EVORAG_LIVE_LLM_URL").ok()?;
        let aux = match (
            std::env::var("EVORAG_LIVE_EMBED_URL"),
            std::env::var("EVORAG_LIVE_RERANK_URL"),
        ) {
            (Ok(e), Ok(r)) => Some((e, r)),
            (Err(_), Err(_)) => None,
            _ => {
                eprintln!(
                    "set both EVORAG_LIVE_EMBED_URL and EVORAG_LIVE_RERANK_URL to use them; running without"
                );
                None
            }
        };
        Some(Self {
            chat,
            api_key: std::env::var("EVORAG_LIVE_LLM_API_KEY").ok(),
            aux,
        })
    }

    /// 応答の llama-server への接続。
    pub fn chat_client(&self) -> Arc<dyn LlmClient> {
        http(&self.chat, self.api_key.clone())
    }

    /// 構成の名前 (結果に書く)。
    pub fn setup(&self) -> &'static str {
        if self.aux.is_some() {
            "chat+embed+rerank"
        } else {
            "chat"
        }
    }
}

fn http(url: &str, key: Option<String>) -> Arc<dyn LlmClient> {
    Arc::new(
        HttpClient::new(
            Endpoint::new(url, key, false).expect("a valid llama-server URL"),
            RetryPolicy::default(),
        )
        .expect("an HTTP client"),
    )
}

/// 思考するモデルの思考を切る (思考しないモデルでは、テンプレートが無視する)。
fn thinking_off() -> RequestExtra {
    let mut extra = RequestExtra::new();
    extra.insert(
        "chat_template_kwargs".into(),
        serde_json::json!({"enable_thinking": false}),
    );
    extra
}

/// 評価のコアの設定: 答えは temperature 0・固定の seed、sleep-time はすぐに動かし、点検と較正を裏で行う。
pub fn config(
    root: &Path,
    lang: PromptLanguage,
    servers: &Servers,
    clock: Arc<dyn Clock>,
) -> CoreConfig {
    let (embed, rerank) = match &servers.aux {
        Some((e, r)) => (Some(http(e, None)), Some(http(r, None))),
        None => (None, None),
    };
    CoreConfig {
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: env!("CARGO_PKG_VERSION").into(),
        prompt_language: lang,
        prompt_language_explicit: true,
        generation: Generation {
            temperature: 0.0,
            max_tokens: Some(ANSWER_MAX_TOKENS),
            seed: Some(ANSWER_SEED),
            ..Generation::default()
        },
        request_extra: thinking_off(),
        reserve_for_output: None,
        backup_dir: None,
        allow_unsafe_location: false,
        probe: evorag_core::Probe::system(),
        model_name: "live".into(),
        ignored_config: Vec::new(),
        embed,
        rerank,
        aux_request_extra: thinking_off(),
        probe_models: true,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: false,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
    }
}

/// 会話の話し手。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    User,
    Assistant,
}

/// 書き込む 1 つのセッション。ターンは利用者から始めて交互に並べる。
#[derive(Debug, Clone)]
pub struct Session {
    pub title: String,
    /// セッションの始まりの時刻 (前のセッションより前なら、前のセッションの後にずらす)。
    pub start: Timestamp,
    pub turns: Vec<(Speaker, String)>,
}

/// ターンの間の時間 (ミリ秒)。
const TURN_GAP_MS: i64 = 30_000;

/// 会話をデータ根に直接書く (コアと同じ書き込みの入口 `Writes` を使う。応答の llama-server は使わない)。
/// 時刻は `clock` で決め、セッションごとに `start` に合わせ、ターンごとに進める。最後に書いた時刻を返す。
///
/// # Panics
///
/// データ根を開けない・書けない、または応答のターンの前に利用者のターンが無いとき。
pub fn write_sessions(root: &Path, clock: &Arc<ManualClock>, sessions: &[Session]) -> Timestamp {
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let (store, _) = Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: env!("CARGO_PKG_VERSION").into(),
        kinds: evorag_memory::kinds(),
        clock: dyn_clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(dyn_clock, 7)),
        config_file: None,
    })
    .expect("open the data root");
    let store = Arc::new(store);
    let (w, h) = Writer::new(store.clone()).expect("a writer");
    let t = std::thread::spawn(move || w.run());
    let writes = Writes {
        clock: &**clock,
        ids: &**store.ids(),
    };
    let read = |kind: &str, id| match store.read().expect("read").get(kind, id).expect("get") {
        Some(Read::Ok(e)) => e,
        other => panic!("cannot read back {kind} {id:?}: {other:?}"),
    };
    for s in sessions {
        let next = clock.now().as_millis() + TURN_GAP_MS;
        clock.set(Timestamp::from_millis(s.start.as_millis().max(next)));
        let (sid, ops) = writes.create_session(&s.title).expect("a session");
        h.submit_wait(WriteRequest::conversation(ops))
            .expect("write the session");
        let mut user = None;
        for (speaker, text) in &s.turns {
            clock.advance(TURN_GAP_MS);
            let session = read(evorag_memory::KIND_SESSION, sid);
            let (id, ops) = match speaker {
                Speaker::User => writes.user_turn(&session, text).expect("a user turn"),
                Speaker::Assistant => {
                    let u = read(
                        evorag_memory::KIND_TURN,
                        user.expect("a user turn before an answer"),
                    );
                    writes
                        .assistant_turn(
                            &session,
                            &u,
                            AssistantTurn {
                                state: TurnState::Completed,
                                text: text.clone(),
                                model_key: None,
                                generation: None,
                                attached: Vec::new(),
                                error: None,
                                recall: None,
                                obs: None,
                            },
                        )
                        .expect("an answer turn")
                }
            };
            h.submit_wait(WriteRequest::conversation(ops))
                .expect("write the turn");
            if *speaker == Speaker::User {
                user = Some(id);
            }
        }
    }
    h.shutdown();
    t.join().expect("the writer thread");
    clock.now()
}

/// 前のデータ根で済んだ点検 (06 §3.3) と較正 (05 §3.7) の結果。同じサーバなら、次のデータ根に書いておき、
/// 事例ごとに点検と較正をやり直さない (setup で済ませた後の状態と同じ)。
#[derive(Debug, Default)]
pub struct Checks {
    probes: Vec<ModelProbeBody>,
    threshold: Option<ThresholdBody>,
}

impl Checks {
    /// 動いているコアから、点検と較正の結果を拾う。
    pub fn capture(&mut self, core: &Core) {
        for role in [
            evorag_memory::ROLE_CHAT,
            evorag_memory::ROLE_EMBED,
            evorag_memory::ROLE_RERANK,
        ] {
            if let Some(p) = core.probe_result(role) {
                self.probes.retain(|q| q.role != p.role);
                self.probes.push(p);
            }
        }
        if let Some(t) = core.threshold() {
            self.threshold = Some(t);
        }
    }

    /// 新しいデータ根 (コアを開く前) に書く。
    ///
    /// # Panics
    ///
    /// データ根を開けない・書けないとき。
    pub fn seed(&self, root: &Path) {
        if self.probes.is_empty() && self.threshold.is_none() {
            return;
        }
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let m = DataRootModels::open(
            root,
            env!("CARGO_PKG_VERSION"),
            clock.clone(),
            Arc::new(UuidV7Gen::new(clock)),
        )
        .expect("open the data root");
        for p in &self.probes {
            m.save_probe(p).expect("save the probe result");
        }
        if let Some(t) = &self.threshold {
            m.save_threshold(t).expect("save the threshold");
        }
    }
}

/// 裏の処理 (点検・較正・埋め込み・ノート・事実の抽出・訂正の検証・事実の表) の要る手順が無くなるまで待つ。
/// `limit` までに終わらなければ `false`。
pub async fn settle(core: &Core, limit: Duration) -> bool {
    let c = core.clone();
    tokio::spawn(async move { c.settle_background_within(limit).await })
        .await
        .is_ok()
}

/// `pred` に合うイベントまで待つ。`limit` を過ぎるか、流れが閉じたら `None`。
pub async fn until(
    rx: &mut UnboundedReceiver<Event>,
    limit: Duration,
    pred: impl Fn(&Event) -> bool,
) -> Option<Event> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv()).await.ok()??;
        if pred(&ev) {
            return Some(ev);
        }
    }
}

/// 新しいセッションを開き、問いを送って答えを待つ。待ちきれなければ `None`。
pub async fn ask_in_new_session(
    core: &Core,
    rx: &mut UnboundedReceiver<Event>,
    question: &str,
    limit: Duration,
) -> Option<TurnView> {
    core.command(Command::NewSession {
        title: "eval".into(),
    });
    until(rx, limit, |e| matches!(e, Event::SessionOpened { .. })).await?;
    core.command(Command::Send {
        text: question.into(),
    });
    let Event::ResponseFinished { turn, .. } =
        until(rx, limit, |e| matches!(e, Event::ResponseFinished { .. })).await?
    else {
        unreachable!("matched ResponseFinished")
    };
    core.settle().await;
    Some(turn)
}

/// 今の事実の一覧 (`/memory`) を `主語/属性=値 (状態, 時制)` の並び (今の値の消えた属性は `(vacant)`) で返す (結果の記録と、外したときの調べのため)。
pub async fn facts(core: &Core, rx: &mut UnboundedReceiver<Event>) -> Vec<String> {
    core.command(Command::ShowFacts);
    let Some(Event::FactsInfo { list }) = until(rx, Duration::from_secs(30), |e| {
        matches!(e, Event::FactsInfo { .. })
    })
    .await
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for g in &list.groups {
        for row in &g.rows {
            let subject = match &g.group.subject {
                FactSubject::User => "self",
                FactSubject::Named(n) => n.as_str(),
            };
            match row {
                FactRow::Value { key, value, .. } => out.push(format!(
                    "{subject}/{}={value} ({:?}, {:?})",
                    key.attribute, key.status, key.tense
                )),
                FactRow::Vacant { group } => {
                    out.push(format!("{subject}/{}=(vacant)", group.attribute));
                }
                FactRow::RuleOnly { group, .. } => {
                    out.push(format!("{subject}/{}=(private rule only)", group.attribute));
                }
            }
        }
    }
    out
}

/// 結果に書くモデルの情報 (`/props` と `/v1/models` から)。ローカルのパスを書かないように、モデルのファイルと
/// 名前 (llama-server はパスを返すことがある) は、最後の部分だけにする (09 §8.2 の「固有の情報は記録に書かない」)。
pub async fn models(servers: &Servers) -> serde_json::Value {
    async fn one(c: Arc<dyn LlmClient>) -> serde_json::Value {
        match c.server_info().await {
            Ok(s) => serde_json::json!({
                "model_file": file_name(&s.model_path),
                "model_name": file_name(&s.model_name),
                "model_key": s.model_key,
                "build_info": s.build_info,
                "n_ctx": s.n_ctx,
            }),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        }
    }
    let mut v = serde_json::json!({ "chat": one(servers.chat_client()).await });
    if let Some((e, r)) = &servers.aux {
        v["embed"] = one(http(e, None)).await;
        v["rerank"] = one(http(r, None)).await;
    }
    v
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// 今日の日付 (地方時、`YYYY-MM-DD`)。
pub fn today() -> String {
    let ms = SystemClock.now().as_millis();
    let off = evorag_core::localtime::offset(ms).unwrap_or(0);
    let (y, m, d, _, _) = evorag_core::localtime::civil(ms + i64::from(off) * 1000);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `EVORAG_EVAL_LOG` にファイルのパスがあれば、コアの記録 (tracing、INFO 以上) をそこに書く (外した事例を
/// 調べるため。裏の手順ごとの件数が出る)。
pub fn init_log() {
    let Some(path) = std::env::var_os("EVORAG_EVAL_LOG") else {
        return;
    };
    let file = std::fs::File::create(&path).expect("create the log file");
    let _ = tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .try_init();
}

/// 結果の置き場所: `EVORAG_BENCH_OUT` があればそこ、無ければ target の試験用のフォルダ。
pub fn out_dir() -> PathBuf {
    std::env::var_os("EVORAG_BENCH_OUT")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from)
}

/// JSON を書く (整形し、末尾に改行)。
///
/// # Panics
///
/// 書けないとき。
pub fn write_json(path: &Path, v: &serde_json::Value) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("create the output folder");
    }
    let text = serde_json::to_string_pretty(v).expect("serialize") + "\n";
    std::fs::write(path, text).expect("write the results");
    println!("wrote {}", path.display());
}

/// 決定論の判定 (LLM の判定を使わない)。
pub mod judge {
    use super::UnicodeNormalization;

    /// 比べる前の正規化: NFKC、小文字、右の引用符を `'` に、空白の並びを 1 つの空白に。ASCII でない文字
    /// (日本語など) に接する空白は除く (`5 月 13 日` を `5月13日` と同じにする)。
    pub fn normalize(s: &str) -> String {
        let s: String = s
            .nfkc()
            .flat_map(char::to_lowercase)
            .map(|c| if c == '\u{2019}' { '\'' } else { c })
            .collect();
        let words: Vec<&str> = s.split_whitespace().collect();
        let mut out = String::with_capacity(s.len());
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                let before = words[i - 1]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii());
                let after = w.chars().next().is_some_and(|c| c.is_ascii());
                if before && after {
                    out.push(' ');
                }
            }
            out.push_str(w);
        }
        out
    }

    /// 正規化した答えが、正規化した値を含むか。
    pub fn contains(answer: &str, value: &str) -> bool {
        let v = normalize(value);
        !v.is_empty() && normalize(answer).contains(&v)
    }

    /// 「分からない・聞いていない」旨の印 (正規化した形)。答えの無い問いで、作り話をせずに断ったとみなす。
    pub const ABSTAIN_MARKERS: &[&str] = &[
        // 日本語
        "わかりません",
        "分かりません",
        "わかりかねます",
        "分かりかねます",
        "わからない",
        "分からない",
        "知りません",
        "存じません",
        "存じ上げません",
        "聞いていません",
        "聞いておりません",
        "伺っていません",
        "伺っておりません",
        "うかがっていません",
        "お聞きしていません",
        "お聞きしておりません",
        "教えていただいていません",
        "教えていただいておりません",
        "教えてもらっていません",
        "まだ伺って",
        "まだ聞いて",
        "まだお聞き",
        "記憶にありません",
        "記憶していません",
        "記憶しておりません",
        "覚えていません",
        "情報がありません",
        "情報はありません",
        "情報がない",
        "記録がありません",
        "記録はありません",
        "見当たりません",
        "把握していません",
        "把握しておりません",
        "含まれていません",
        "含まれておりません",
        "記載されていません",
        "記載がありません",
        "登録されていません",
        "特定することはできません",
        "特定できません",
        "お答えできません",
        "お答えすることはできません",
        // 英語
        "don't know",
        "do not know",
        "not sure",
        "didn't mention",
        "did not mention",
        "haven't mentioned",
        "have not mentioned",
        "never mentioned",
        "not mentioned",
        "no mention",
        "didn't tell",
        "did not tell",
        "haven't told",
        "have not told",
        "never told",
        "didn't share",
        "did not share",
        "haven't shared",
        "have not shared",
        "didn't provide",
        "did not provide",
        "not provided",
        "no information",
        "not enough information",
        "don't have information",
        "do not have information",
        "don't have any information",
        "do not have any information",
        "no record",
        "don't have a record",
        "do not have a record",
        "can't find",
        "cannot find",
        "couldn't find",
        "could not find",
        "don't recall",
        "do not recall",
        "unable to determine",
        "can't determine",
        "cannot determine",
        "not included",
        "isn't included",
        "not specified",
        "isn't specified",
        "can't tell",
        "cannot tell",
    ];

    /// 答えに、断った印があるか。
    pub fn abstained(answer: &str) -> bool {
        let a = normalize(answer);
        ABSTAIN_MARKERS.iter().any(|m| a.contains(m))
    }
}
