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
        // job は作り直せる手順の記録なので、ジャーナルには書かない。
        KindSpec::new(KIND_JOB, 1),
    ]
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
}

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
