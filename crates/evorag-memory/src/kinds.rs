//! 会話の層の種類 (03 §3.3、04 §2)。本体は項目を省略せずに書く (値の無い項目は null、03 §4.2 の規則 7)。

use evorag_store::key::OwnedPart;
use evorag_store::{DeriveFn, Envelope, KindSpec, WriteOp};
use evorag_types::Id;
use serde::{Deserialize, Serialize};

pub const KIND_SESSION: &str = "session";
pub const KIND_TURN: &str = "turn";
pub const KIND_SIGNAL: &str = "signal";
/// 裏の処理の手順の進み具合 (03 §3.3)。
pub const KIND_JOB: &str = "job";
/// 利用者が画面から変えた、データ根の全体に効く設定 (03 §3.3、v0.2)。
pub const KIND_SETTING: &str = "setting";
/// 役割ごとに記録したモデル (06 §3.1、03 §3.3、v0.2)。
pub const KIND_MODEL_BINDING: &str = "model_binding";
/// 役割の名前 (足すだけで広げる文字列、03 §3.3)。
pub const ROLE_CHAT: &str = "chat";
pub const ROLE_EMBED: &str = "embed";
pub const ROLE_RERANK: &str = "rerank";
/// 設定の名前: 読んで無視した設定のキーと、その内容のハッシュ (同じなら 2 回目から知らせない、07 §4.1)。
pub const SETTING_IGNORED_CONFIG: &str = "notice.ignored_config";
/// 設定の名前: 答え方の既定 (07 §3 の `/style`)。
pub const SETTING_ANSWER_LANGUAGE: &str = "answer.language";
pub const SETTING_ANSWER_LENGTH: &str = "answer.length";
pub const SETTING_ANSWER_FORMAT: &str = "answer.format";
/// job の手順: 消した記録を退避から除く (03 §7.3)。
pub const JOB_PURGE_BACKUPS: &str = "purge_backups";

/// ターン → セッション。
pub const REL_IN_SESSION: &str = "in_session";
/// 応答のターン → 発話のターン。
pub const REL_REPLY_TO: &str = "reply_to";
/// 信号 → 対象のターン。
pub const REL_TARGET: &str = "target";
/// 関係: 埋め込み → 対象 (チャンク) (05 §2.2)。
pub const REL_EMBEDDING_OF: &str = "embedding_of";

/// 埋め込み (05 §2.2、v0.2)。どの記録のどの単位の、どのモデルの埋め込みか。ベクトルは `blobs` の
/// part 0 (f32 のリトルエンディアン)。作り直せる派生の記録なので、ジャーナルに書かない。
pub const KIND_EMBEDDING: &str = "embedding";
/// 埋め込みの単位: チャンク。
pub const UNIT_CHUNK: &str = "chunk";

/// `embedding` の本体 (v1)。出典の ID を本体にも持つ (03 §4.2 の規則 12)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingBody {
    /// 対象の記録 (チャンク)。
    pub target: Id,
    /// 単位 (`chunk`、後の版で `key`)。知らない値は読んで残し、使わない。
    pub unit: String,
    /// 対象のセッション (今のセッションを候補から除くため)。
    pub session: Id,
    /// 埋め込みのモデルの model_key。
    pub model_key: String,
    pub dim: u32,
}

/// 関係: チャンク → 元のターン (05 §2.4)。
pub const REL_CHUNK_OF: &str = "chunk_of";

/// ターンの中の範囲 (05 §2.4、v0.2)。作り直せる派生の記録なので、ジャーナルに書かない。
pub const KIND_CHUNK: &str = "chunk";

/// `chunk` の本体 (v1)。本文は持たない (ターンの本文から作り直せる)。出典のターンの ID を本体にも持つ
/// (03 §4.2 の規則 12)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkBody {
    pub turn: Id,
    pub session: Id,
    /// ターンの本文の中の範囲 (文字の位置)。
    pub start: u32,
    pub end: u32,
    /// 区切り方の版 (05 §2.4)。変われば作り直す。
    pub rules: u32,
}

/// 関係: 想起への操作 → 問いの応答のターン (05 §5.2)。
pub const REL_FEEDBACK_ANSWER: &str = "feedback_answer";
/// 関係: 想起への操作 → 対象の往復 (を代表するターン)。
pub const REL_FEEDBACK_TARGET: &str = "feedback_target";

/// 想起への利用者の操作 (05 §5.2、v0.2)。利用者の明示の操作なのでジャーナルに書く。
pub const KIND_RECALL_FEEDBACK: &str = "recall_feedback";
/// 帯の往復を添えて作り直した。
pub const FEEDBACK_ATTACH: &str = "attach";
/// 「この記録は、この問いには関係が無い」の印。
pub const FEEDBACK_IRRELEVANT: &str = "irrelevant";

/// `recall_feedback` の本体 (v1)。出典の ID を本体にも持つ (03 §4.2 の規則 12)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallFeedbackBody {
    /// `attach`・`irrelevant`。知らない値は読んで残し、使わない。
    pub op: String,
    /// 問いの応答のターン。
    pub answer: Id,
    /// 対象の往復 (を代表するターン)。
    pub target: Id,
    /// その時のリランクの生の値 (リランクしていなければ無い)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    /// その時のリランカーの model_key。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reranker: Option<String>,
}

/// 副索引: セッションの更新時刻 (一覧を新しい順に並べる)。
pub(crate) const LOOKUP_UPDATED_AT: &str = "updated_at";

/// この版が書く、会話の層の種類。会話の追記は、新しい版の記録があっても今の `v` で足してよい
/// (巻き戻した後も会話を続けられるように、03 §4.2 の規則 6)。
///
/// どれも会話と利用者の明示の操作なので、ジャーナルにも書く (03 §6)。ジャーナルから取り込むときは、
/// 本体から関係と副索引を作り直す。
pub fn kinds() -> Vec<KindSpec> {
    let append = |name, derive: DeriveFn| KindSpec {
        name,
        current_v: 1,
        create_when_newer_present: true,
        journaled: true,
        derive: Some(derive),
    };
    vec![
        append(KIND_SESSION, derive_session),
        append(KIND_TURN, derive_turn),
        append(KIND_SIGNAL, derive_signal),
        append(KIND_RECALL_FEEDBACK, derive_feedback),
        // 利用者の明示の操作なのでジャーナルに書く。関係も副索引も持たない (件数が少ないので全部読む)。
        KindSpec {
            name: KIND_SETTING,
            current_v: 1,
            create_when_newer_present: true,
            journaled: true,
            derive: None,
        },
        // 利用者が setup で確かめた (または最初の起動で記録した) 正本なので、ジャーナルに書く。
        KindSpec {
            name: KIND_MODEL_BINDING,
            current_v: 1,
            create_when_newer_present: true,
            journaled: true,
            derive: None,
        },
        // 点検の結果は作り直せる派生の記録なので、ジャーナルには書かない (03 §3.3)。
        KindSpec::new(KIND_MODEL_PROBE, 1),
        // チャンクは作り直せる派生の記録 (05 §2.4)。関係は書くときに足す。
        KindSpec::new(KIND_CHUNK, 1),
        // 埋め込みは作り直せる派生の記録 (05 §2.2)。
        KindSpec::new(KIND_EMBEDDING, 1),
        // v0.2 のしきい値は同梱の事例から求め直せるので、ジャーナルには書かない (03 §3.3)。
        KindSpec::new(KIND_THRESHOLD, 1),
        // job は作り直せる手順の記録なので、ジャーナルには書かない。
        KindSpec::new(KIND_JOB, 1),
    ]
}

/// モデルの性質の点検の結果 (06 §3.3、v0.2)。
pub const KIND_MODEL_PROBE: &str = "model_probe";

/// `model_probe` の本体 (v1)。役割・model_key・`build_info` ごとに 1 件 (新しいものを採る)。
/// 測っていない項目は null。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelProbeBody {
    pub role: String,
    pub model_key: String,
    pub build_info: String,
    /// chat: プロンプトの処理の速さ (トークン毎秒)。
    pub prompt_tps: Option<f64>,
    /// chat: 生成の速さ (トークン毎秒)。
    pub gen_tps: Option<f64>,
    /// chat: 補助の呼び出しの設定で、思考が出なかった。
    pub thinking_off: Option<bool>,
    /// chat: 型縛りの出力が、型に合う JSON だった (空にならなかった)。
    pub structured_ok: Option<bool>,
    /// embed: 次元の数。
    pub dims: Option<u32>,
    /// embed: 問い 1 件の埋め込みの時間 (ms)。
    pub embed_ms: Option<u32>,
    /// rerank: 往復 6 件のリランクの時間 (ms)。
    pub rerank6_ms: Option<u32>,
    /// 点検が失敗した理由 (利用者に見せる文言ではない)。
    pub error: Option<String>,
}

/// リランクのしきい値と帯の下限 (05 §3.7、v0.2)。
pub const KIND_THRESHOLD: &str = "threshold";
/// しきい値の対象の種類: 以前の会話 (05 §3.4)。足すだけで広げる文字列。
pub const TARGET_CONVERSATION: &str = "conversation";
/// 求めた方法: 同梱の事例 (05 §3.7)。v0.4 から `signals`。
pub const METHOD_BUNDLED: &str = "bundled";

/// `threshold` の本体 (v1)。リランカーの model_key と対象の種類ごとに、版を上げて足していく
/// (前の版を残し、巻き戻せる)。採用した時刻は封筒の作成時刻。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThresholdBody {
    pub reranker_key: String,
    pub target: String,
    pub version: u32,
    pub method: String,
    /// 使った同梱の事例 (`ja-1` など)。変わったら求め直す。
    pub cases: String,
    /// 合否 (05 §3.7)。落ちたときも記録し (値は使わない)、起動のたびに求め直さない。
    pub passed: bool,
    pub threshold: Option<f64>,
    pub band_low: Option<f64>,
    /// 数えた値 (`rank_ok`・`rank_n`・`value_ok` など。足すだけで広げる)。
    #[serde(default)]
    pub stats: std::collections::BTreeMap<String, u32>,
}

/// モデルの元の値 (違いの中身を見せるため、06 §3.1)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOrigin {
    pub model_key: String,
    /// `/props` の `model_path` のファイル名 (フォルダを除く)。
    pub file_name: String,
    /// `/v1/models` の `data[0].id`。
    pub model_name: String,
    pub n_params: Option<u64>,
    pub size: Option<u64>,
    pub n_vocab: Option<u64>,
    /// 記録したときの llama-server の `build_info`。
    pub build_info: String,
}

/// `model_binding` の本体 (v1)。役割ごとに 1 件 (2 件以上あれば `updated_at` の新しいものを採る)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelBindingBody {
    /// `chat`・`embed`・`rerank`。知らない役割は読んで残し、使わない (03 §3.3)。
    pub role: String,
    pub current: ModelOrigin,
    /// 前に記録していたモデル (新しいものが後ろ)。前のモデルに戻したときに見分ける (06 §3.2 の 8)。
    pub history: Vec<ModelOrigin>,
}

/// `setting` の本体 (v1)。名前ごとに 1 件 (2 件以上あれば `updated_at` の新しいものを採る、03 §3.3)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingBody {
    /// 設定の名前 (`answer.language` など)。
    pub name: String,
    /// 値。知らない値の読み方は、名前ごとに決める (答え方の既定は `auto`、07 §3)。
    pub value: String,
}

/// `job` の本体 (v1)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobBody {
    /// 手順の名前 (`purge_backups` など)。
    pub task: String,
    /// 手順の対象の記録の ID。
    pub ids: Vec<Id>,
}

fn link(from: Id, rel: &str, to: Id) -> WriteOp {
    WriteOp::PutLink {
        from,
        rel: rel.into(),
        to,
        attrs: Vec::new(),
    }
}

fn derive_session(e: &Envelope) -> Vec<WriteOp> {
    vec![WriteOp::PutLookup {
        kind: KIND_SESSION.into(),
        field: LOOKUP_UPDATED_AT.into(),
        value: OwnedPart::Int(e.updated_at.as_millis()),
        id: e.id,
    }]
}

/// ターンの本体のうち、関係を作るのに要る項目だけ (03 §4.2 の規則 11)。
///
/// 本体の全体 (`TurnBody`) では読まない。新しい版が書いた、この版では読めない値 (知らない列挙の値など)
/// を含むターンでも、セッションとの関係を失わないようにする。
#[derive(Deserialize)]
struct TurnLinks {
    session: Id,
    #[serde(default)]
    reply_to: Option<Id>,
}

/// 信号の本体のうち、関係を作るのに要る項目だけ (03 §4.2 の規則 11)。
#[derive(Deserialize)]
struct SignalLinks {
    target: Id,
}

fn derive_turn(e: &Envelope) -> Vec<WriteOp> {
    let Ok(b) = e.body_as::<TurnLinks>() else {
        return Vec::new();
    };
    let mut ops = vec![link(e.id, REL_IN_SESSION, b.session)];
    if let Some(u) = b.reply_to {
        ops.push(link(e.id, REL_REPLY_TO, u));
    }
    ops
}

fn derive_feedback(e: &Envelope) -> Vec<WriteOp> {
    match e.body_as::<RecallFeedbackBody>() {
        Ok(b) => vec![
            link(e.id, REL_FEEDBACK_ANSWER, b.answer),
            link(e.id, REL_FEEDBACK_TARGET, b.target),
        ],
        Err(_) => Vec::new(),
    }
}

fn derive_signal(e: &Envelope) -> Vec<WriteOp> {
    match e.body_as::<SignalLinks>() {
        Ok(b) => vec![link(e.id, REL_TARGET, b.target)],
        Err(_) => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
}

/// `session` の本体 (v1)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBody {
    pub title: String,
    /// v0.1 は `chat` だけ (コーディングモードは v1.0 より後、ADR-0016)。
    pub mode: String,
    pub state: SessionState,
    /// このセッションでメモリを添えるか (`/sources`、FR-409)。無ければ設定 `memory.enabled`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRole {
    User,
    Assistant,
    System,
}

/// ターンの状態 (03 §3.3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    /// 発話: 送った。応答をまだ書いていない (起動したときにこのままなら「応答なし」)。
    Pending,
    /// 発話: 応答を書いた。
    Confirmed,
    /// 応答: 最後まで受け取った。
    Completed,
    /// 応答: 利用者が中断した。受け取ったところまでの本文を持つ。
    Interrupted,
    /// 応答: 失敗した。受け取ったところまでの本文と理由を持つ。
    Failed,
}

/// 生成の設定の記録 (06 §7)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationRecord {
    pub temperature: f64,
    pub top_p: f64,
    pub max_tokens: Option<u32>,
    pub seed: Option<u64>,
}

/// `turn` の本体 (v1)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnBody {
    pub session: Id,
    pub role: TurnRole,
    pub state: TurnState,
    pub text: String,
    /// 応答を作ったモデル (06 §5、FR-508)。発話では null。
    pub model_key: Option<String>,
    pub generation: Option<GenerationRecord>,
    /// 応答に添えたもの (以前の会話のターンなど) の ID。
    pub attached: Vec<Id>,
    /// 応答の対象の発話 (応答だけ)。
    pub reply_to: Option<Id>,
    /// 中断・失敗の理由 (利用者に見せる文言ではなく、分類と詳細)。
    pub error: Option<String>,
    /// 想起の判定点と出典の区切り (応答だけ、05 §3.4・§5)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall: Option<RecallRecord>,
    /// 後から作り直せない観測値 (応答だけ、v0.2、05 §3.6)。判定点 (`recall`) の中には足さない
    /// (古い版が判定点を書き戻しても落ちないように、本体の最上位に置く、03 §3.3)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obs: Option<TurnObs>,
}

/// 応答のターンの観測値 (05 §3.6)。項目はすべて任意で、足すだけにする。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnObs {
    /// このセッションの出典の通し番号の始まり (05 §5.1)。無いターン (v0.1) は 1 から。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_base: Option<u32>,
    /// 履歴に入れた最も古いターン (05 §4.2)。次のターンは、ここから履歴を組み立てる。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_from: Option<Id>,
    /// 想起の引用を添えた位置 (`before_utterance`)。添えなかったら無い。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_position: Option<String>,
    /// 選んでいた答え方の既定 (`language`・`length`・`format` → 値、07 §3)。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub answer: std::collections::BTreeMap<String, String>,
    /// リランクした往復と、その生の値 (高い順、05 §3.6)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reranked: Vec<Reranked>,
    /// 採点したリランカーの model_key。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reranker: Option<String>,
    /// 使ったしきい値の記録の版 (05 §3.7)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_version: Option<u32>,
    /// 帯に出した往復 (05 §3.4)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub band: Vec<Id>,
    /// 利用者が帯から選んで添えた往復 (05 §5.2 の「添えて作り直す」)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_attached: Vec<Id>,
    /// 縮退の理由 (`no_embed`・`no_rerank` など、05 §3.5)。値は足すだけで、知らない値はそのまま読む。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded: Vec<String>,
}

/// リランクした 1 つの往復。`id` は往復を代表するターン (利用者の発話。発話の無い往復は、そのターン)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Reranked {
    pub id: Id,
    pub score: f32,
}

/// 縮退の理由 (05 §3.5)。
pub const DEGRADED_NO_EMBED: &str = "no_embed";
pub const DEGRADED_NO_RERANK: &str = "no_rerank";
pub const DEGRADED_RERANK_SLOW: &str = "rerank_slow";
pub const DEGRADED_EMBED_DEADLINE: &str = "embed_deadline";
pub const DEGRADED_RERANK_DEADLINE: &str = "rerank_deadline";
pub const DEGRADED_RERANK_ERROR: &str = "rerank_error";
pub const DEGRADED_NO_THRESHOLD: &str = "no_threshold";
pub const DEGRADED_NO_INDEX: &str = "no_index";

/// 想起の引用を、今の発話の直前 (同じメッセージの中) に添えた (05 §4.1)。
pub const RECALL_BEFORE_UTTERANCE: &str = "before_utterance";

/// 判定点 (05 §3.4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallDecision {
    /// 添えた。
    Fire,
    /// 候補はあったが、しきい値を超えなかった。
    Abstain,
    /// 索引が無くて引けなかった。
    Skip,
}

/// 応答のときの想起の記録。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallRecord {
    pub decision: RecallDecision,
    /// 出典の番号の順に、`attached` の中のその出典のターンの数 (出典 n は `attached` の
    /// 先頭から数えた n 番目の区切り)。
    pub sources: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    Good,
    Bad,
    Retry,
}

/// `signal` の本体 (v1、13 §2)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalBody {
    pub kind: SignalKind,
    pub target: Id,
    /// `/bad 理由` の理由。
    pub reason: Option<String>,
}
