//! コアの API (02 §3): 命令 (画面 → コア)、イベント (コア → 画面)、照会の読み取り用の型。
//!
//! 同じプロセスの中だけで使い、版の間で変えてよい (OQ-25)。値は足すだけで広げる (02 §3 の決めごと 3)。

use crate::{Id, Message, Timestamp};

/// 命令 (画面 → コア)。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Command {
    /// セッションを作って開く。
    NewSession { title: String },
    /// セッションを開く。
    OpenSession { id: Id },
    /// 開いているセッションで発話する。
    Send { text: String },
    /// 生成を中断する。
    Interrupt,
    /// 直前の応答を作り直す (`/retry`。前の応答も残し、再生成の信号を記録する、FR-103)。
    Retry,
    /// 最後の発話を送り直す (失敗の後、FR-106)。
    Resend,
    /// 開いているセッションを改名する (`/rename`)。
    RenameSession { title: String },
    /// 開いているセッションを消す (`/delete`。画面が確認した後に送る、FR-105)。
    DeleteSession,
    /// 直前の応答を評価する (`/good`、`/bad [理由]`、FR-311)。
    Rate { good: bool, reason: Option<String> },
    /// `/model [名前]` (FR-108): 名前が無ければ今のモデルと候補を示し、あれば切り替える。
    Model { name: Option<String> },
    /// `/sources [on|off]` (FR-409): このセッションでメモリを添えるかを示す・変える。
    Sources { memory: Option<bool> },
    /// 応答 `turn` の出典 `n` (セッションの通し番号) の詳細を照会する (FR-110)。
    ShowSource { turn: Id, n: usize },
    /// `/style` (FR-111、07 §3): `set` が無ければ今の答え方の既定を示し、あれば 1 つを変える。
    /// 値は `auto` などの名前で渡し、コアが確かめる。
    Style { set: Option<(StyleItem, String)> },
}

/// 答え方の既定の項目 (07 §3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleItem {
    Language,
    Length,
    Format,
}

impl StyleItem {
    pub const ALL: [StyleItem; 3] = [StyleItem::Language, StyleItem::Length, StyleItem::Format];

    /// `/style <項目>` の名前。
    pub fn as_str(self) -> &'static str {
        match self {
            StyleItem::Language => "language",
            StyleItem::Length => "length",
            StyleItem::Format => "format",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|i| i.as_str() == s)
    }

    /// 選べる値 (先頭が既定の `auto`)。
    pub fn choices(self) -> &'static [&'static str] {
        match self {
            StyleItem::Language => &["auto", "ja", "en"],
            StyleItem::Length => &["auto", "short", "long"],
            StyleItem::Format => &["auto", "plain", "structured"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

/// ターンの状態 (03 §3.3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    /// 発話: 応答をまだ書いていない (起動したときにこのままなら「応答なし」)。
    Pending,
    /// 発話: 応答を書いた。
    Confirmed,
    Completed,
    Interrupted,
    Failed,
}

/// 1 つの役割の、記録したモデルと繋がっているモデル (ファイル名と model_key、06 §3.1)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRoleInfo {
    /// `chat`・`embed`・`rerank`。
    pub role: String,
    pub recorded: Option<(String, String)>,
    pub connected: Option<(String, String)>,
    pub status: RoleStatus,
}

/// 役割の状態 (06 §3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleStatus {
    /// 設定していない (その役割は無いものとして縮退する)。
    Unset,
    /// 繋がらない。繋がるまで使わず、ときどき試す (06 §3 の表)。
    Waiting,
    /// 記録と同じモデルで、使える。
    Ready,
    /// 記録と違うモデル。使わない (`evorag setup` で切り替える)。
    Mismatch,
}

/// 読み取り用のセッション。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub id: Id,
    pub title: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// 読み取り用のターン。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnView {
    pub id: Id,
    pub session: Id,
    pub role: Role,
    pub status: TurnStatus,
    pub text: String,
    pub model_key: Option<String>,
    pub reply_to: Option<Id>,
    pub created_at: Timestamp,
    /// 添えた以前の会話の出典の数 (FR-110)。
    pub sources: usize,
    /// 出典の番号の始まり (セッションの通し番号、05 §5.1)。応答の `[source_base]` から
    /// `[source_base + sources - 1]` までが、この応答で添えた出典。
    pub source_base: usize,
}

/// イベント (コア → 画面)。順序を保つ 1 本の流れで届く (02 §3 の決めごと 4)。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// セッションを開いた (作ったときも)。今までのターンを元の順に持つ。
    SessionOpened {
        session: SessionView,
        turns: Vec<TurnView>,
        /// この版では読めなかったターンの数 (新しい版が書いた記録。消さずに残っている、
        /// 03 §4.2 の規則 11)。`turns` には入らない。
        unreadable: usize,
    },
    /// 発話を保存した (この後で LLM に送る)。
    UserTurnSaved { turn: TurnView },
    /// 発話を保存できなかったので送らなかった。`text` は入力欄に戻す (04 §3 の 1)。
    SendRejected { text: String, error: Message },
    /// 応答の生成を始めた。
    ResponseStarted { reply_to: Id },
    /// 応答の本文の断片。
    Token { reply_to: Id, text: String },
    /// モデルが思考している (本文の前。思考の本文は届けない、07 §2.2)。
    Thinking { reply_to: Id },
    /// 応答が終わった (完了・中断・失敗)。`notice` は中断・失敗の理由。
    ResponseFinished {
        turn: TurnView,
        notice: Option<Message>,
    },
    /// 命令を実行できなかった (状態は変わっていない)。
    Rejected { error: Message },
    /// セッションを改名した。
    SessionRenamed { session: SessionView },
    /// セッションを消した。開いているセッションは無くなる。
    SessionDeleted { id: Id },
    /// 評価を記録した。
    Rated { good: bool },
    /// このセッションでメモリを添えるか (`/sources` の答え)。
    SourcesInfo { memory: bool },
    /// 出典の詳細 (FR-110)。
    SourceDetail {
        n: usize,
        /// 出典のセッションの名前。
        title: String,
        /// 出典の最初のターンの時刻。
        at: Timestamp,
        /// 出典のターン (役割と本文)。
        lines: Vec<(Role, String)>,
    },
    /// 役割ごとの、記録したモデルと繋がっているモデル (`/model` の答え、v0.2 は表示だけ、FR-108)。
    ModelInfo {
        roles: Vec<ModelRoleInfo>,
        /// 応答のモデルのスロットごとの文脈の長さ。
        n_ctx: u32,
    },
    /// 裏の処理 (退避・退避の整理) を止めた (空きの不足など、03 §8)。会話は続けられる。
    BackgroundPaused { reason: Message },
    /// 答え方の既定 (`/style` の答え)。`values` は `StyleItem::ALL` の順の値。`changed` は、
    /// この命令で変えたか (変えたら、次の応答は始まるまで少し長くかかる、07 §3)。
    StyleInfo { values: [String; 3], changed: bool },
    /// モデルの性質の点検が終わり、使えない機能が見つかった (06 §3.3)。
    ProbeIssues {
        role: String,
        /// 思考を切れない。
        thinking_not_off: bool,
        /// 型縛りの出力が空、または型に合わない (型縛りの補助の呼び出しを止める)。
        structured_broken: bool,
    },
    /// embed・rerank の役割の状態が変わった (繋がった、記録と違った、記録した、06 §3)。
    RoleChanged {
        info: ModelRoleInfo,
        /// この変化でモデルを初めて記録した。
        recorded_now: bool,
    },
}
