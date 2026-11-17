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
    /// 応答 `answer` の帯の往復 `target` の詳細を照会する (05 §5.2)。
    ShowBand { answer: Id, target: Id },
    /// 帯の往復 `target` を添えて、最新の応答 `answer` を作り直す (05 §5.2 の「添えて作り直す」)。
    AttachBand { answer: Id, target: Id },
    /// 応答 `answer` の出典・帯の往復 `target` に「無関係」の印を付ける。付いていれば取り消す (05 §5.2)。
    ToggleIrrelevant { answer: Id, target: Id },
    /// 入力欄の文字が約 500 ms 変わらなかった (05 §4.5 の先に済ませる処理の合図)。`text` は、そのまま送ったら
    /// 発話になる本文。コアは想起を先に済ませ、同じ発話が送られたら結果を使う。保存しない。
    InputIdle { text: String },
    /// `/memory` (FR-310、07 §3): 直前の応答に添えたメモリ、候補の帯、リランクの値、縮退の理由と、
    /// このセッションの出典の番号と往復の対応を照会する。
    Memory,
    /// 事実の一覧を照会する (04 §7.1、07 §3 の `/memory`)。データ根の全体のものなので、セッションを開いて
    /// いなくてよい。答えは [`Event::FactsInfo`]。
    ShowFacts,
    /// (主語, 属性) の置き換えの履歴を照会する (`/memory history`、04 §7.4)。答えは [`Event::FactHistoryInfo`]。
    ShowFactHistory { group: FactGroupKey },
    /// 忘れた事実の一覧を照会する (`/memory forgotten`、04 §7.6)。答えは [`Event::ForgottenInfo`]。
    ShowForgotten,
    /// 値の行を private にする (`/private`、04 §7.5)。画面が確認した後に送る。
    SetFactPrivate {
        target: FactKey,
        scope: PrivateScope,
    },
    /// 行に効いているすべての private の指示を解く (`/private off`、04 §7.5)。画面が確認した後に送る。
    UnsetFactPrivate { target: FactRowRef },
    /// `Contested` の値を今の値にする (`/memory accept`、04 §7.4)。
    AcceptFact { target: FactKey },
    /// 履歴の値を今の値に戻す (`/memory revert`、04 §7.4)。
    RevertFact { target: FactKey },
    /// 再び覚えない印を解く (`/memory unblock`、04 §7.6)。
    UnblockFact { target: ForgottenRef },
    /// 忘れるの見本を求める (`/forget` の確認の画面、04 §7.1)。記録は変えない。答えは
    /// [`Event::ForgetPreviewInfo`]。画面は選択を変えるたびに求め直す。
    PreviewForget {
        target: FactKey,
        options: ForgetOptions,
    },
    /// 値の行を忘れる (`/forget`、04 §7.1〜§7.3)。`plan` は見本の計画のハッシュ。コアは書く直前に計画を作り
    /// 直し、違えば実行せずに新しい見本を返す。答えは [`Event::FactForgotten`] か [`Event::ForgetRejected`]。
    ForgetFact {
        target: FactKey,
        options: ForgetOptions,
        plan: ForgetPlanHash,
    },
    /// `broken/` に移した壊れた DB のファイルを消す (忘れるの後に尋ねて、利用者が選んだとき、04 §7.2)。
    DiscardBroken,
}

/// 忘れるの選択 (04 §7.1 の確認の画面)。既定は [`ForgetOptions::default`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForgetOptions {
    /// 出典の発話を伏せ字にする (既定)。偽なら事実だけを消し、選んだ範囲を想起での `[非表示]` にする。
    pub redact: bool,
    /// 同じ (主語, 属性) の置き換えの履歴の値もまとめて忘れる。
    pub history: bool,
    /// 同じ往復の応答の中で値の鍵が一致する範囲も伏せる (既定)。出典の発話の根拠の外の一致も含む。
    pub echo: bool,
    /// ほかのターンで値の鍵が一致する範囲も伏せる。
    pub others: bool,
}

impl Default for ForgetOptions {
    fn default() -> Self {
        Self {
            redact: true,
            history: false,
            echo: true,
            others: false,
        }
    }
}

/// 忘れるの計画のハッシュ (04 §7.1 の「見本と計画」。16 進の文字列)。実行の命令に添える。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForgetPlanHash(String);

impl ForgetPlanHash {
    pub fn new(hex: String) -> Self {
        Self(hex)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 見本の 1 件: 伏せ字 (か `[非表示]`) にするターンの、前と後の本文を 1 行ずつ (変わる箇所の周り)。`Debug` は
/// 本文を出さない (ログに忘れる値を残さないため)。
#[derive(Clone, PartialEq, Eq)]
pub struct ForgetSample {
    pub turn: Id,
    pub role: Role,
    pub before: String,
    pub after: String,
}

impl std::fmt::Debug for ForgetSample {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForgetSample")
            .field("turn", &self.turn)
            .field("role", &self.role)
            .field("before_len", &self.before.len())
            .field("after_len", &self.after.len())
            .finish()
    }
}

/// 伏せ字にできない理由 (04 §7.1 の「中止」、§7.3)。見本では警告に示し、伏せ字にしないで忘れることを勧める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetWarning {
    /// 伏せ字にするターンが読めない (読めない記録・新しい版の記録)。
    UnreadableTurn,
    /// 取り消す事実の出典の改訂の番号が、今のターンと合わない。
    StaleSource,
    /// 伏せ字にする範囲の本文が、忘れる値と合わない (本文が記録と食い違う)。
    ValueMismatch,
    /// 伏せ字にするターンが、前の忘れるの指示と合わない。
    MismatchedTurn,
}

/// 忘れるの見本 (04 §7.1 の「見本と計画」)。値は見本の本文の中にだけある。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetPreview {
    pub target: FactKey,
    /// 見本を作った選択 (画面は、今の選択と同じ見本だけを使う)。
    pub options: ForgetOptions,
    /// 伏せ字 (か `[非表示]`) にするターンの見本 (ターンの順)。
    pub samples: Vec<ForgetSample>,
    /// ほかのターンで値の鍵が一致したターンの数と、範囲の数 (選んでいなくても数える)。
    pub other_turns: usize,
    pub other_matches: usize,
    /// まとめて忘れられる履歴の値の数 (選んでいなくても数える)。
    pub history: usize,
    /// 伏せ字の範囲に span が重なる、ほかの事実の行 (自動では忘れない、04 §7.1 の「影響するほかの事実」)。
    pub affected: Vec<FactKey>,
    /// 根拠の範囲に値の一致が無いので伏せ字にしない出典のターンの数。
    pub unredacted_sources: usize,
    /// 出典の発話の、根拠の範囲の外で値の鍵が一致した範囲の数 (`echo` と同じ選択で伏せる)。
    pub source_outside: usize,
    /// 位置を戻せないので全体を `[非表示]` にするターンの数 (伏せ字にしないとき)。
    pub whole_turns: usize,
    /// 前の忘れるの指示と合わないので飛ばしたターンの数 (伏せ字にしないとき)。
    pub mismatched_turns: usize,
    /// 探さずに飛ばした読めないターンの数。
    pub skipped_unreadable: usize,
    /// 実行できる計画のハッシュ。伏せ字にできなければ、その理由 (計画は無い)。
    pub plan: Result<ForgetPlanHash, ForgetWarning>,
}

/// 忘れるの状態 (状態の行、04 §7.2・§7.8)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForgetState {
    /// 忘れるの指示と合わないターンの数 (想起・裏の処理・書き出しに使わず、画面は本文を出さない)。
    pub mismatched: usize,
    /// ジャーナルに、伏せ字の前の行が残っている (書き直しの入れ替えに失敗した。sleep-time に試し直す)。
    pub journal_pending: bool,
    /// `memory.forget_key` が使えないので、忘れた値に当たる事実の取り消しを保留している。
    pub key_missing: bool,
}

/// 事実の主語 (04 §5.1)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FactSubject {
    /// 利用者本人 (`self`)。
    User,
    /// 発話にある呼び名 (正規化 v1 の後)。
    Named(String),
}

/// 事実の時制 (04 §5.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FactTense {
    Current,
    Past,
    Planned,
}

/// 一覧に示した行の状態の種類 (04 §7.1 の行の参照)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FactStatus {
    Live,
    Contested,
    Superseded,
}

/// 値の行の参照 (04 §7.1): 事実への指示と同じ鍵 (主語, 属性, 値の鍵) と、表示したときの状態の種類と時制。
///
/// 等しさとハッシュは `fact` を見ない (事実の ID は作り直すと変わるので照合に使わない。参考として持つ)。
#[derive(Debug, Clone)]
pub struct FactKey {
    pub subject: FactSubject,
    pub attribute: String,
    /// 値の鍵 (64 桁の小文字の 16 進)。
    pub vk: String,
    pub status: FactStatus,
    pub tense: FactTense,
    /// 表示したときの事実の ID (参考)。
    pub fact: Option<Id>,
}

impl FactKey {
    fn parts(&self) -> (&FactSubject, &str, &str, FactStatus, FactTense) {
        (
            &self.subject,
            &self.attribute,
            &self.vk,
            self.status,
            self.tense,
        )
    }

    /// 行が属する (主語, 属性)。
    pub fn group(&self) -> FactGroupKey {
        FactGroupKey {
            subject: self.subject.clone(),
            attribute: self.attribute.clone(),
        }
    }
}

impl PartialEq for FactKey {
    fn eq(&self, other: &Self) -> bool {
        self.parts() == other.parts()
    }
}

impl Eq for FactKey {}

impl std::hash::Hash for FactKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.parts().hash(state);
    }
}

/// (主語, 属性) の参照 (今の値が消えた属性・後継まで効く private の指示だけが残る属性の行、履歴の照会)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FactGroupKey {
    pub subject: FactSubject,
    pub attribute: String,
}

/// `/private off` の対象の行。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FactRowRef {
    /// 値の行。
    Value(FactKey),
    /// 値が 1 つも無く、後継まで効く private の指示だけが残る属性の行。
    RuleOnly(FactGroupKey),
}

/// 忘れた事実の一覧の行の参照 (忘れるの指示の ID)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForgottenRef {
    pub directive: Id,
}

/// 属性の名前と、多値か (表示名は画面がカタログの表で引く。知らない属性は名前のまま出す)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrInfo {
    pub name: String,
    pub multi: bool,
}

/// private の効く範囲 (04 §7.5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrivateScope {
    /// 指定した値と、同じ (主語, 属性) にあとで入る値 (既定)。
    Successors,
    /// この値だけ。
    ValueOnly,
}

/// 値が private である理由 (04 §7.5 の「private の理由の表示」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateCause {
    /// 利用者がこの値を選んだ。
    Chosen { scope: PrivateScope },
    /// 後継まで効く指示の後に言われた値。
    Successor,
    /// セッションの削除で後継でなくなったので、削除のときに足した指示。
    KeptAfterDelete,
}

/// 値の行の private の印。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateInfo {
    /// 理由 (重ならない。表示の順)。
    pub causes: Vec<PrivateCause>,
    /// 効いている private の指示 (`/private off` で解くもの。ほかの行と共有していれば、確認の画面で示す)。
    pub directives: Vec<Id>,
}

/// 事実の一覧の 1 行 (04 §7.1 の「番号を振る行」)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactRow {
    /// 今の値・予定の値・`Contested` の値。
    Value {
        key: FactKey,
        value: String,
        private: Option<PrivateInfo>,
    },
    /// 今の値が消えた属性 (04 §7.4)。履歴に届く。
    Vacant { group: FactGroupKey },
    /// 値が 1 つも無く、後継まで効く private の指示だけが残る属性。
    RuleOnly {
        group: FactGroupKey,
        directives: Vec<Id>,
    },
}

/// (主語, 属性) ごとの行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactGroup {
    pub group: FactGroupKey,
    pub attr: AttrInfo,
    pub rows: Vec<FactRow>,
    /// 履歴 (`Superseded`) の値の数。
    pub history: usize,
    /// 履歴の値のうち private のもの (`/private off` の確認で、一緒に印が外れる履歴の値を示すため、04 §7.5)。
    pub private_history: Vec<PrivateHistoryValue>,
}

/// private の履歴の値 1 つと、それに効いている private の指示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateHistoryValue {
    pub value: String,
    pub directives: Vec<Id>,
}

/// 事実を作っていない・止めている理由 (07 §3 の `/memory`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactsUnavailable {
    /// `memory.sleep_time = false`。
    SleepTimeOff,
    /// 点検で型縛りの補助の呼び出しを止めた (06 §3.3)。
    StoppedByProbe,
    /// `memory.forget_key` が使えない (03 §3.1.1)。事実への操作もできない。
    KeyUnavailable,
}

/// 事実の一覧 (`/memory`、04 §7.1)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactList {
    /// 属性の一覧の順 → 主語 (`self` が先) の順。
    pub groups: Vec<FactGroup>,
    pub note: Option<FactsUnavailable>,
}

/// 置き換えの履歴の 1 件 (`/memory history`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactHistoryRow {
    /// 状態は `Superseded`。
    pub key: FactKey,
    pub value: String,
    /// 置き換えた・終えた出来事の時刻。
    pub superseded_at: Timestamp,
    pub private: bool,
}

/// (主語, 属性) の置き換えの履歴 (新しい順)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactHistory {
    pub group: FactGroupKey,
    pub attr: AttrInfo,
    pub items: Vec<FactHistoryRow>,
}

/// 忘れた事実の一覧の 1 件 (`/memory forgotten`、値は持たない、04 §7.6)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgottenFact {
    pub target: ForgottenRef,
    pub subject: FactSubject,
    pub attr: AttrInfo,
    /// 忘れた日時。
    pub at: Timestamp,
    /// 忘れた値の数 (再び覚えない印の数)。
    pub values: usize,
    /// 出典の発話を伏せ字にしたか。
    pub redacted: bool,
    /// 出典のセッションの題名 (残っているもの)。
    pub sessions: Vec<String>,
    /// 消えた出典のターンの数 (消えたターンからはセッションが分からないので、ターンで数える)。
    pub deleted_turns: usize,
}

/// 事実への操作 (結果の知らせを選ぶため)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactOp {
    Private,
    PrivateOff,
    Accept,
    Revert,
    Unblock,
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

/// 裏の処理の状態 (07 §2.1 の状態の行、04 §4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundState {
    /// 待機中 (操作の無い時間を待っている、または要る手順が無い)。
    Idle,
    /// 整理中 (手順が動いている)。
    Working,
    /// 電池のため休止 (`memory.on_battery = "stop"`)。
    PausedBattery,
    /// 空きが足りないので休止 (03 §8)。
    PausedLowSpace,
    /// 設定で切っている (`memory.sleep_time = false`)。
    Off,
}

/// 想起の状態 (07 §2.1 の状態の行の「想起」の欄、05 §3.5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallState {
    /// リランカーと、合格したしきい値がある。
    Standard,
    /// リランカーが無い・使えない (語彙の規則などで動く)。
    Simple,
    /// 埋め込みを作り直している (進み具合の百分率)。その間は語彙の候補とリランクで答える (FR-410)。
    Rebuilding { percent: u8 },
    /// リランカーはあるが、しきい値が無い (自動では添えず、候補だけを出す)。
    CandidatesOnly {
        /// 較正の途中。
        calibrating: bool,
        /// 較正に落ちた (`evorag setup` で別のリランカーを勧める)。
        failed: bool,
    },
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
    /// 候補の帯 (05 §3.4・§5.2)。同じセッションで「無関係」の印を付けた往復は除く。
    pub band: Vec<BandItem>,
    /// 帯が「近い記録 (自動では添えていません)」(しきい値が無いとき、05 §3.5 の `no_threshold`)。
    pub band_nearby: bool,
    /// この応答で「無関係」の印を付けた往復 (出典・帯の代表のターン)。
    pub irrelevant: Vec<Id>,
    /// 伏せ字と `[非表示]` の印の範囲 (04 §7.3 の「画面への印の範囲」。始まりの順、重ならない)。
    pub marks: Vec<TurnMark>,
    /// 本文が忘れるの指示と合わないので、本文を確かめられない (04 §7.2 の「合わないターン」)。`text` は空。
    /// 画面は本文の代わりに、確かめられないことを示す。
    pub unverified: bool,
}

/// 画面への印 1 つ (04 §7.3)。範囲は今の本文の UTF-8 のバイト位置で、書記素の境界にある。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnMark {
    pub range: std::ops::Range<usize>,
    pub kind: TurnMarkKind,
}

/// 印の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnMarkKind {
    /// 本文のその範囲が、すでに伏せ字の印の文字列。そのまま見分けられる表示で出す。
    Redacted,
    /// 本文には値が残っている。画面は、その範囲を `[非表示]` の印に置き換えて出す。
    Hidden,
}

/// 帯の 1 件 (05 §5.2): セッション名・日時・利用者の発話の冒頭。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandItem {
    /// 往復を代表するターン。
    pub target: Id,
    pub title: String,
    pub at: Timestamp,
    /// 利用者の発話の冒頭 (1 行、30 文字ほど)。
    pub head: String,
}

/// `/memory` の 1 件の置き場所 (しきい値との関係、07 §3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPlace {
    /// 添えた。
    Attached,
    /// 候補の帯 (しきい値の少し下)。
    Band,
    /// 帯より下 (リランクしたが使わなかった)。
    Below,
}

/// `/memory` の、直前の応答の往復 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryItem {
    /// 往復を代表するターン。
    pub target: Id,
    pub place: MemoryPlace,
    /// 添えた往復の出典の番号 (セッションの通し番号)。
    pub n: Option<u32>,
    pub title: String,
    /// 利用者の発話の冒頭 (1 行、30 文字ほど)。
    pub head: String,
    /// リランクの生の値と、しきい値からの差 (どちらも 1000 倍した整数。リランクしていなければ無い。
    /// 差は、応答のときのしきい値の記録が今も使えるときだけ)。
    pub score_milli: Option<i32>,
    pub delta_milli: Option<i32>,
}

/// `/memory` の、このセッションの出典 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySource {
    pub n: u32,
    pub title: String,
    pub head: String,
}

/// `/memory` の答え (FR-310、07 §3)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryReport {
    /// 直前の応答 (このセッションにまだ応答が無ければ `None`)。
    pub answer: Option<Id>,
    /// リランクした往復 (値の高い順)。リランクしていなければ、添えた往復と帯。
    pub items: Vec<MemoryItem>,
    /// 縮退の理由 (05 §3.5 の名前)。
    pub degraded: Vec<String>,
    /// 今の想起の状態 (次にすることを示すため)。
    pub recall: RecallState,
    /// このセッションの出典の番号と往復 (番号の順)。
    pub sources: Vec<MemorySource>,
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
        /// 出典の番号。帯の往復なら 0。
        n: usize,
        /// 問いの応答のターンと、往復を代表するターン (「無関係」の印の対象)。
        answer: Id,
        target: Id,
        /// 「無関係」の印が付いている。
        irrelevant: bool,
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
    /// `/memory` の答え (FR-310)。
    MemoryInfo { report: MemoryReport },
    /// 答え方の既定 (`/style` の答え)。`values` は `StyleItem::ALL` の順の値。`changed` は、
    /// この命令で変えたか (変えたら、次の応答は始まるまで少し長くかかる、07 §3)。
    StyleInfo { values: [String; 3], changed: bool },
    /// 「無関係」の印を付けた・取り消した (05 §5.2)。
    IrrelevantChanged {
        answer: Id,
        target: Id,
        irrelevant: bool,
    },
    /// 裏の処理の状態が変わった (04 §4)。
    BackgroundState { state: BackgroundState },
    /// 想起の状態が変わった (較正が終わった、リランカーが繋がった、05 §3.7)。
    RecallStatus { state: RecallState },
    /// モデルの性質の点検が終わり、使えない機能が見つかった (06 §3.3)。
    ProbeIssues {
        role: String,
        /// 思考を切れない。
        thinking_not_off: bool,
        /// 型縛りの出力が空、または型に合わない (型縛りの補助の呼び出しを止める)。
        structured_broken: bool,
    },
    /// 1 回の裏の処理で事実が増えた・置き換わった・矛盾になった (04 §5.2.1・§5.3 の「知らせ」)。
    /// 件数だけを持ち、値は持たない。
    FactsChanged {
        added: usize,
        replaced: usize,
        contested: usize,
    },
    /// embed・rerank の役割の状態が変わった (繋がった、記録と違った、記録した、06 §3)。
    RoleChanged {
        info: ModelRoleInfo,
        /// この変化でモデルを初めて記録した。
        recorded_now: bool,
    },
    /// 事実の一覧 (`ShowFacts` の答え)。
    FactsInfo { list: FactList },
    /// 置き換えの履歴 (`ShowFactHistory` の答え)。
    FactHistoryInfo { history: FactHistory },
    /// 忘れた事実の一覧 (`ShowForgotten` の答え)。
    ForgottenInfo { items: Vec<ForgottenFact> },
    /// 事実への操作をした。`list` は操作の後の一覧 (前の番号は使えなくなる)。`slow_next_turn` は、次の応答の前に
    /// 事実の表を作り直すので、始まるまで少し長くかかるか (04 §5.4)。
    FactsUpdated {
        op: FactOp,
        list: FactList,
        slow_next_turn: bool,
    },
    /// 事実への操作をしなかった。指した行が変わっていたときは、同じ読み取りで作った新しい一覧を持つ
    /// (04 §7.1。「一覧が更新されました」)。
    FactOpRejected {
        error: Message,
        list: Option<FactList>,
    },
    /// 忘れるの見本 (`PreviewForget` の答え)。
    ForgetPreviewInfo { preview: ForgetPreview },
    /// 事実を忘れた (`ForgetFact`)。`list` は操作の後の一覧 (前の番号は使えなくなる)。`redacted` は伏せ字か
    /// `[非表示]` の範囲を足したターンの数。`journal_pending` は、ジャーナルの書き直しが済んでいない (sleep-time に
    /// 試し直す、04 §7.8)。`broken` は `broken/` に残っている壊れた DB のファイルの数 (消すかを尋ねる、04 §7.2)。
    FactForgotten {
        list: FactList,
        redacted: usize,
        journal_pending: bool,
        broken: usize,
    },
    /// 忘れるの見本を作れなかった・実行しなかった。見本の後に計画が変わったときは、新しい見本を持つ (04 §7.1。
    /// 利用者が見ていない範囲を伏せないため)。指した行が変わっていたときは、新しい一覧を持つ。
    ForgetRejected {
        error: Message,
        preview: Option<ForgetPreview>,
        list: Option<FactList>,
    },
    /// 開いているセッションのターンに、伏せ字か `[非表示]` の範囲ができた (04 §7.3)。`turns` は読み直したターン
    /// (印の範囲を持つ)。画面は同じ ID のターンを置き換えて描き直す。
    TurnsRedacted { session: Id, turns: Vec<TurnView> },
    /// 忘れるの状態が変わった (合わないターン・ジャーナルの書き直しの残り、04 §7.2・§7.8)。
    ForgetState { state: ForgetState },
    /// `broken/` のファイルを消した (`DiscardBroken` の答え)。`failed` は消せなかった数。
    BrokenDiscarded { removed: usize, failed: usize },
}
