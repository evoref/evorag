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
}

/// イベント (コア → 画面)。順序を保つ 1 本の流れで届く (02 §3 の決めごと 4)。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// セッションを開いた (作ったときも)。今までのターンを元の順に持つ。
    SessionOpened {
        session: SessionView,
        turns: Vec<TurnView>,
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
    /// 今のモデルと切り替えの候補 (`/model` の答え、切り替えた後)。
    ModelInfo {
        /// 候補の名前 (設定の名前。候補から選んでいなければ接続先)。
        current: String,
        /// llama-server が返したモデルの名前。
        model: String,
        n_ctx: u32,
        choices: Vec<String>,
    },
    /// 裏の処理 (退避・退避の整理) を止めた (空きの不足など、03 §8)。会話は続けられる。
    BackgroundPaused { reason: Message },
}
