//! モデルの性質の点検 (06 §3.3、FR-514、OQ-37 の 7)。
//!
//! 小さな固定の課題 (作った文) で、速さ・思考を切れるか・型縛りの出力が空にならないかを確かめ、
//! 役割・model_key・`build_info` ごとに保存する。行うのは setup のときと、model_key か `build_info` が
//! 前回の点検と違うときだけ (起動のたびには行わない、NFR-101)。

use std::time::{Duration, Instant};

use evorag_llm::{
    ChatEvent, ChatMessage, ChatRequest, Generation, LlmClient, LlmError, RequestExtra, Role,
};
use evorag_memory::ModelProbeBody;
use serde_json::json;

/// 1 つの課題の締切 (遅いモデルでも、点検が止まり続けないように)。
const LIMIT: Duration = Duration::from_secs(120);

/// 点検の結果のうち、使えない機能 (状態の行と `doctor` に出す理由)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeIssue {
    /// 補助の呼び出しの設定でも、思考が出る (思考を切れない)。
    ThinkingNotOff,
    /// 型縛りの出力が空、または型に合わない。
    StructuredBroken,
}

/// 点検の結果から、使えない機能を拾う。
pub fn issues(p: &ModelProbeBody) -> Vec<ProbeIssue> {
    let mut out = Vec::new();
    if p.thinking_off == Some(false) {
        out.push(ProbeIssue::ThinkingNotOff);
    }
    if p.structured_ok == Some(false) {
        out.push(ProbeIssue::StructuredBroken);
    }
    out
}

#[derive(Default)]
pub(crate) struct Collected {
    pub(crate) text: String,
    /// 生成が終わった理由 (`stop`・`length` など。サーバが返したときだけ)。
    pub(crate) finish_reason: Option<String>,
    reasoning: bool,
    usage: Option<evorag_llm::Usage>,
    /// 最初の断片までの時間 (プロンプトの処理の時間とみなす)。
    first: Option<Duration>,
    elapsed: Duration,
}

/// 逐次表示を最後まで受け取れなかった理由 (呼ぶ側が失敗の種類で扱いを分ける、06 §7)。
#[derive(Debug)]
pub(crate) enum ChatError {
    /// 締切 (`LIMIT`) までに終わらなかった。
    TimedOut,
    /// llama-server の失敗 (繋がらない・HTTP の状態・逐次表示の途中の失敗)。
    Llm(LlmError),
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => f.write_str("timed out"),
            Self::Llm(e) => e.fmt(f),
        }
    }
}

/// 要求を送り、逐次表示を最後まで受け取る (締切は `LIMIT`)。
pub(crate) async fn run_chat(
    llm: &dyn LlmClient,
    req: ChatRequest,
) -> Result<Collected, ChatError> {
    let started = Instant::now();
    let fut = async {
        let mut stream = llm.chat(req).await.map_err(ChatError::Llm)?;
        let mut c = Collected::default();
        while let Some(ev) = stream.next().await {
            let ev = ev.map_err(ChatError::Llm)?;
            if c.first.is_none() && !matches!(ev, ChatEvent::Done { .. }) {
                c.first = Some(started.elapsed());
            }
            match ev {
                ChatEvent::Delta(d) => c.text.push_str(&d),
                ChatEvent::Reasoning(_) => c.reasoning = true,
                ChatEvent::Done {
                    finish_reason,
                    usage,
                } => {
                    c.finish_reason = finish_reason;
                    c.usage = usage;
                    break;
                }
            }
        }
        Ok::<_, ChatError>(c)
    };
    let mut c = tokio::time::timeout(LIMIT, fut)
        .await
        .map_err(|_| ChatError::TimedOut)??;
    c.elapsed = started.elapsed();
    Ok(c)
}

/// 毎秒の数 (小数 1 桁)。数か時間が 0 なら測れなかったとする。
fn per_sec(n: u64, d: Duration) -> Option<f64> {
    let secs = d.as_secs_f64();
    (n > 0 && secs > 0.0).then(|| (n as f64 / secs * 10.0).round() / 10.0)
}

/// 速さを測る課題のプロンプト (プロンプトの処理の時間が測れるよう、数百トークンにする)。
fn speed_prompt() -> String {
    let mut s = String::from("次の文を読んでから、最後の問いに答えてください。\n");
    for i in 1..=40 {
        s.push_str(&format!(
            "{i} 番目の棚には、植物の本と料理の本が並んでいる。\n"
        ));
    }
    s.push_str("問い: 1 から 30 までの数を、空白で区切って並べてください。");
    s
}

fn request(prompt: &str, extra: RequestExtra, max_tokens: u32, slot: Option<u32>) -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage::new(Role::User, prompt.to_owned())],
        generation: Generation {
            temperature: 0.0,
            top_p: 1.0,
            max_tokens: Some(max_tokens),
            seed: Some(1),
        },
        id_slot: slot,
        model: None,
        extra,
    }
}

/// 思考を切る指定 (`chat_template_kwargs`) の項目の名前 (06 §6)。
pub const THINKING_KEY: &str = "chat_template_kwargs";

/// 点検が自動で足す、思考を切る指定 (OQ-52)。
pub fn thinking_off_value() -> serde_json::Value {
    json!({"enable_thinking": false})
}

/// 補助の呼び出しに足す JSON: 設定の指定に、点検が通った思考を切る指定 (`p.aux_thinking_off`) を足したもの
/// (設定に `chat_template_kwargs` があれば、そちらを使う、06 §6、OQ-52)。
pub fn aux_extra_with(config: &RequestExtra, p: Option<&ModelProbeBody>) -> RequestExtra {
    let mut e = config.clone();
    if !e.contains_key(THINKING_KEY) && p.is_some_and(|p| p.aux_thinking_off == Some(true)) {
        e.insert(THINKING_KEY.into(), thinking_off_value());
    }
    e
}

/// 型縛りの補助の呼び出しを 1 回試す: (思考が出なかったか, 型に合う JSON だったか)。
async fn structured(
    llm: &dyn LlmClient,
    aux_extra: &RequestExtra,
    slot: Option<u32>,
) -> Result<(bool, bool), ChatError> {
    let mut extra = aux_extra.clone();
    extra.insert(
        "response_format".into(),
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": "probe",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {"n": {"type": "integer"}},
                    "required": ["n"],
                    "additionalProperties": false
                }
            }
        }),
    );
    let c = run_chat(
        llm,
        request(
            "1 から 9 までの数を 1 つ選び、JSON で答えてください。",
            extra,
            64,
            slot,
        ),
    )
    .await?;
    let ok = serde_json::from_str::<serde_json::Value>(c.text.trim())
        .ok()
        .and_then(|v| v.get("n").and_then(serde_json::Value::as_i64))
        .is_some();
    Ok((!c.reasoning, ok))
}

/// chat の役割を点検する。`aux_extra` は補助の呼び出しに足す JSON (`llm.chat.aux_request_extra`)。
pub async fn chat(
    llm: &dyn LlmClient,
    model_key: &str,
    build_info: &str,
    aux_extra: &RequestExtra,
    slot: Option<u32>,
) -> ModelProbeBody {
    let mut p = ModelProbeBody {
        role: evorag_memory::ROLE_CHAT.into(),
        model_key: model_key.into(),
        build_info: build_info.into(),
        ..ModelProbeBody::default()
    };
    // 型縛りの補助の呼び出し (06 §6): 型に合う JSON が返るか、思考が出ないか。
    match structured(llm, aux_extra, slot).await {
        Ok((thinking_off, ok)) => {
            p.thinking_off = Some(thinking_off);
            p.structured_ok = Some(ok);
        }
        Err(e) => p.error = Some(format!("structured: {e}")),
    }
    // 思考を切れない・型縛りの出力が空で、設定に思考を切る指定が無ければ、指定を足して確かめ直す (OQ-52)。
    let failed = p.thinking_off == Some(false) || p.structured_ok == Some(false);
    if failed && !aux_extra.contains_key(THINKING_KEY) {
        let mut with = aux_extra.clone();
        with.insert(THINKING_KEY.into(), thinking_off_value());
        if let Ok((true, true)) = structured(llm, &with, slot).await {
            p.thinking_off = Some(true);
            p.structured_ok = Some(true);
            p.aux_thinking_off = Some(true);
        }
    }
    let aux_extra = &aux_extra_with(aux_extra, Some(&p));
    // 生成の速さ (補助の呼び出しと同じ設定で、思考の時間を数えないようにする)。
    // 前の点検のキャッシュでプロンプトの処理が省かれないようにする。
    let mut extra = aux_extra.clone();
    extra.insert("cache_prompt".into(), json!(false));
    match run_chat(llm, request(&speed_prompt(), extra, 96, slot)).await {
        Ok(c) => {
            if let (Some(u), Some(first)) = (c.usage, c.first) {
                p.prompt_tps = per_sec(u.prompt_tokens, first);
                // 最初の断片の後の時間で、残りのトークンを割る。
                p.gen_tps = per_sec(
                    u.completion_tokens.saturating_sub(1),
                    c.elapsed.saturating_sub(first),
                );
            }
        }
        Err(e) => {
            p.error.get_or_insert(format!("speed: {e}"));
        }
    }
    p
}

/// embed の役割を点検する。
pub async fn embed(llm: &dyn LlmClient, model_key: &str, build_info: &str) -> ModelProbeBody {
    let mut p = ModelProbeBody {
        role: evorag_memory::ROLE_EMBED.into(),
        model_key: model_key.into(),
        build_info: build_info.into(),
        ..ModelProbeBody::default()
    };
    let input = vec!["観葉植物の水やりは、土が乾いてからにする。".to_owned()];
    let started = Instant::now();
    match tokio::time::timeout(LIMIT, llm.embed(&input)).await {
        Ok(Ok(v)) if v.first().is_some_and(|x| !x.is_empty()) => {
            p.dims = Some(v[0].len() as u32);
            p.embed_ms = Some(started.elapsed().as_millis() as u32);
        }
        Ok(Ok(_)) => p.error = Some("empty embedding".into()),
        Ok(Err(e)) => p.error = Some(e.to_string()),
        Err(_) => p.error = Some("timed out".into()),
    }
    p
}

/// rerank の役割を点検する (往復 6 件ほどの長さの文書を並べ替える時間、05 §3.4)。
pub async fn rerank(llm: &dyn LlmClient, model_key: &str, build_info: &str) -> ModelProbeBody {
    let mut p = ModelProbeBody {
        role: evorag_memory::ROLE_RERANK.into(),
        model_key: model_key.into(),
        build_info: build_info.into(),
        ..ModelProbeBody::default()
    };
    let topics = [
        "水やり",
        "車の点検",
        "京都の宿",
        "朝の運動",
        "本の整理",
        "夕食の献立",
    ];
    let docs: Vec<String> = topics
        .iter()
        .map(|t| {
            format!(
                "利用者: {t}について相談したい。\nアシスタント: {t}について、順に考えましょう。"
            )
            .repeat(4)
        })
        .collect();
    let started = Instant::now();
    match tokio::time::timeout(LIMIT, llm.rerank("観葉植物の水やりの頻度", &docs)).await
    {
        Ok(Ok(s)) if s.len() == docs.len() && s.iter().all(|x| x.is_finite()) => {
            p.rerank6_ms = Some(started.elapsed().as_millis() as u32);
        }
        Ok(Ok(_)) => p.error = Some("bad scores".into()),
        Ok(Err(e)) => p.error = Some(e.to_string()),
        Err(_) => p.error = Some("timed out".into()),
    }
    p
}
