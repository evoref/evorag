//! 画面の状態と操作 (07 §2・§3)。端末に依存しない (試験しやすくするため)。
//!
//! キーとコアのイベントを受けて状態を変え、コアへの命令 (`Action`) を返す。描画は `render`。

use evorag_types::api::{
    BackgroundState, Command, Event, FactGroup, FactGroupKey, FactHistory, FactKey, FactList,
    FactOp, FactRow, FactRowRef, FactStatus, FactSubject, FactTense, FactsUnavailable,
    ForgottenFact, ModelRoleInfo, PrivateCause, PrivateScope, RecallState, Role, RoleStatus,
    SessionView, StyleItem, TurnStatus, TurnView,
};
use evorag_types::{Id, MessageKey};

use crate::catalog::Catalog;
use crate::input::InputBox;
use crate::slash::{self, Slash};

/// キーの種類 (crossterm に依存しない形)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter {
        newline: bool,
    },
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Esc,
    CtrlC,
    CtrlQ,
    /// 外部のエディタで書く (07 §2.4)。
    CtrlE,
    /// 出典の番号を選ぶ (07 §2.5)。
    Tab,
    Paste(String),
}

/// 画面からコアへ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Command(Command),
    /// セッションの一覧を照会して窓を開く (照会は呼び出し側がコアに行う)。
    ListSessions,
    /// 入力欄の内容を外部のエディタで開く (端末の操作は呼び出し側)。
    OpenEditor,
    Quit,
}

/// 役割の状態の名前の文言キー。
fn status_key(s: RoleStatus) -> MessageKey {
    match s {
        RoleStatus::Unset => MessageKey::UiRoleStatusUnset,
        RoleStatus::Waiting => MessageKey::UiRoleStatusWaiting,
        RoleStatus::Ready => MessageKey::UiRoleStatusReady,
        RoleStatus::Mismatch => MessageKey::UiRoleStatusMismatch,
    }
}

/// embed・rerank の役割が、使えない理由の知らせ (繋がらない、記録と違う、06 §3)。使えれば `None`。
pub fn role_notice(catalog: &Catalog, r: &ModelRoleInfo) -> Option<String> {
    let file = |m: &Option<(String, String)>| m.as_ref().map(|c| c.0.clone()).unwrap_or_default();
    match r.status {
        RoleStatus::Waiting => Some(catalog.with(MessageKey::CliRoleWaiting, &[("role", &r.role)])),
        RoleStatus::Mismatch => Some(catalog.with(
            MessageKey::CliRoleMismatch,
            &[
                ("role", &r.role),
                ("recorded", &file(&r.recorded)),
                ("connected", &file(&r.connected)),
            ],
        )),
        RoleStatus::Unset | RoleStatus::Ready => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Chat,
    Sessions {
        list: Vec<SessionView>,
        selected: usize,
    },
    ConfirmDelete,
    Help,
    /// 出典・帯の往復の詳細の窓 (FR-110、05 §5.2)。`row` は窓の下の操作の位置。
    Detail {
        answer: Id,
        target: Id,
        /// 帯の往復 (「添えて作り直す」を選べる)。出典なら偽。
        band: bool,
        irrelevant: bool,
        title: String,
        body: String,
        row: usize,
    },
    /// 答え方の既定を選ぶ窓 (07 §3 の `/style`)。`choice` は項目ごとの値の位置。
    Style {
        row: usize,
        choice: [usize; 3],
        initial: [usize; 3],
    },
    /// スクロールできる窓 (事実の一覧・置き換えの履歴・忘れた事実の一覧、07 §3)。`scroll` は上から飛ばす行の数。
    Panel {
        title: String,
        body: String,
        scroll: usize,
    },
    /// `/private` の確認の画面 (07 §3、04 §7.1)。既定のまま Enter で実行できる。
    ConfirmPrivate {
        target: FactKey,
        /// 対象の行 (主語・属性・値)。
        label: String,
        multi: bool,
        scope: PrivateScope,
    },
    /// `/private off` の確認の画面。ほかの行や履歴の値の印も外れるときは、それを示す (04 §7.5)。
    ConfirmPrivateOff {
        target: FactRowRef,
        label: String,
        others: Vec<String>,
        /// 一緒に印が外れる履歴の値 (主語・属性・値)。
        history: Vec<String>,
    },
}

/// 番号と行の対応 (画面が持つ、04 §7.1)。番号は最後に表示した一覧に結び付く。
#[derive(Debug, Clone, Default)]
pub struct FactRefs {
    /// 最後に表示した事実の一覧 (`F` の番号)。
    pub list: Option<FactList>,
    /// その前に表示した一覧 (前に見てから変わった行の印に使う。プロセスの間だけ覚える)。
    pub previous: Option<FactList>,
    /// 最後に表示した置き換えの履歴 (`H` の番号)。
    pub history: Option<FactHistory>,
    /// 最後に表示した忘れた事実の一覧 (`X` の番号)。
    pub forgotten: Option<Vec<ForgottenFact>>,
    /// `/memory` で、事実の一覧と一緒に窓に出す想起の答え (事実の一覧が届くまで待たせる)。
    memory: Option<String>,
    /// `/memory` の事実の一覧を待っている。
    awaiting: bool,
}

/// 一覧の行を番号の順に並べる (`F1` から)。
pub fn fact_rows(list: &FactList) -> Vec<(&FactGroup, &FactRow)> {
    list.groups
        .iter()
        .flat_map(|g| g.rows.iter().map(move |r| (g, r)))
        .collect()
}

/// 前の一覧と比べるときの行の同一性 (出典の増減・private の印の変化は数えない、04 §7.1)。
fn same_row(a: &FactRow, b: &FactRow) -> bool {
    match (a, b) {
        (FactRow::Value { key: x, .. }, FactRow::Value { key: y, .. }) => x == y,
        (FactRow::Vacant { group: x }, FactRow::Vacant { group: y })
        | (FactRow::RuleOnly { group: x, .. }, FactRow::RuleOnly { group: y, .. }) => x == y,
        _ => false,
    }
}

/// 行に効いている private の指示 (`/private off` で一緒に解かれる)。
fn row_directives(r: &FactRow) -> &[Id] {
    match r {
        FactRow::Value {
            private: Some(p), ..
        } => &p.directives,
        FactRow::RuleOnly { directives, .. } => directives,
        _ => &[],
    }
}

/// Tab で選んでいるもの (07 §2.5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// 応答 `answer` の出典 `n` (セッションの通し番号)。
    Source { answer: Id, n: usize },
    /// 応答 `answer` の帯の往復 `target`。
    Band { answer: Id, target: Id },
}

/// 詳細の窓の下の操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailAction {
    /// 添えて作り直す (帯だけ)。
    Attach,
    /// 「無関係」の印を付ける・取り消す。
    Irrelevant,
}

impl DetailAction {
    pub fn list(band: bool) -> &'static [DetailAction] {
        if band {
            &[DetailAction::Attach, DetailAction::Irrelevant]
        } else {
            &[DetailAction::Irrelevant]
        }
    }
}

/// 状態の行に出す接続の情報。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub endpoint: String,
    pub model: String,
    pub n_ctx: u32,
    /// モデルの点検で、使えない機能を止めた (06 §3.3)。
    pub limited: bool,
    /// 想起の状態 (07 §2.1。v0.1 のコアは送らない)。
    pub recall: Option<RecallState>,
    /// 裏の処理の状態 (07 §2.1、04 §4)。
    pub background: Option<BackgroundState>,
    /// 設定 `llm.allow_remote` (`/private` の確認に、発話の本文が外の機械に出うることを書く、07 §3)。
    pub remote_llm: bool,
}

/// 生成中の応答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Streaming {
    pub reply_to: Id,
    pub text: String,
    /// モデルが思考している (本文がまだ来ていない)。
    pub thinking: bool,
}

/// 入力が止まったとみなす長さ (05 §4.5)。
pub const INPUT_IDLE: std::time::Duration = std::time::Duration::from_millis(500);

/// 入力が止まったことの判定 (05 §4.5)。入力欄の文字が `INPUT_IDLE` の間変わらなければ、その文字について
/// 1 回だけ知らせる。
pub struct IdleWatch {
    text: String,
    since: std::time::Instant,
    sent: bool,
}

impl IdleWatch {
    pub fn new(now: std::time::Instant) -> Self {
        Self {
            text: String::new(),
            since: now,
            sent: false,
        }
    }

    /// 今の入力欄の文字を見る。止まってから `INPUT_IDLE` が過ぎ、まだ知らせていなければ `true`。
    pub fn due(&mut self, text: &str, now: std::time::Instant) -> bool {
        if text != self.text {
            text.clone_into(&mut self.text);
            self.since = now;
            self.sent = false;
            return false;
        }
        !self.sent && now.duration_since(self.since) >= INPUT_IDLE
    }

    /// 知らせた (同じ文字では、もう知らせない)。
    pub fn sent(&mut self) {
        self.sent = true;
    }
}

pub struct App {
    pub catalog: Catalog,
    pub session: Option<SessionView>,
    pub turns: Vec<TurnView>,
    pub streaming: Option<Streaming>,
    /// 会話の下に出す知らせ (新しいものが最後)。
    pub notices: Vec<String>,
    pub input: InputBox,
    /// 会話の表示を下から何行上へ動かしたか。
    pub scroll: usize,
    pub mode: Mode,
    pub status: Status,
    /// Tab で選んでいる出典か帯の往復 (FR-110、05 §5.2)。
    pub pick: Option<Pick>,
    /// 次の答え方の既定の知らせで、選ぶ窓を開く (`/style` を値なしで打った)。
    style_picker: bool,
    ctrl_c_armed: bool,
    /// 事実の一覧などの番号と行の対応。
    pub facts: FactRefs,
}

impl App {
    pub fn new(catalog: Catalog, status: Status) -> Self {
        Self {
            catalog,
            session: None,
            turns: Vec::new(),
            streaming: None,
            notices: Vec::new(),
            input: InputBox::default(),
            scroll: 0,
            mode: Mode::Chat,
            status,
            pick: None,
            style_picker: false,
            ctrl_c_armed: false,
            facts: FactRefs::default(),
        }
    }

    fn subject_label(&self, s: &FactSubject) -> String {
        match s {
            FactSubject::User => self.catalog.key(MessageKey::UiFactsSelf),
            FactSubject::Named(n) => n.clone(),
        }
    }

    /// 行の主語・属性・値 (確認の画面と、ほかの行の表示に使う)。
    fn row_label(&self, g: &FactGroup, r: &FactRow) -> String {
        let subject = self.subject_label(&g.group.subject);
        let attr = self.catalog.attribute(&g.attr.name);
        match r {
            FactRow::Value { value, .. } => format!("{subject}・{attr}: {value}"),
            _ => format!("{subject}・{attr}"),
        }
    }

    /// 値の行の印 (矛盾・予定・private とその理由・履歴の数)。
    fn value_marks(&self, g: &FactGroup, r: &FactRow, first: bool) -> String {
        let c = &self.catalog;
        let mut marks = String::new();
        let FactRow::Value { key, private, .. } = r else {
            return marks;
        };
        if key.status == FactStatus::Contested {
            marks.push_str(&c.key(MessageKey::UiFactsMarkContested));
        }
        if key.tense == FactTense::Planned {
            marks.push_str(&c.key(MessageKey::UiFactsMarkPlanned));
        }
        for cause in private.iter().flat_map(|p| p.causes.iter()) {
            marks.push_str(&c.key(match cause {
                PrivateCause::Chosen {
                    scope: PrivateScope::Successors,
                } => MessageKey::UiFactsMarkPrivateSuccessors,
                PrivateCause::Chosen {
                    scope: PrivateScope::ValueOnly,
                } => MessageKey::UiFactsMarkPrivateValueOnly,
                PrivateCause::Successor => MessageKey::UiFactsMarkPrivateSuccessor,
                PrivateCause::KeptAfterDelete => MessageKey::UiFactsMarkPrivateKept,
            }));
        }
        if first && g.history > 0 {
            marks.push_str(&c.with(
                MessageKey::UiFactsMarkHistory,
                &[("n", &g.history.to_string())],
            ));
        }
        marks
    }

    /// 事実の一覧を文にする (07 §3)。`previous` は前に表示した一覧 (変わった行に印を付ける)。
    fn facts_text(&self, list: &FactList, previous: Option<&FactList>) -> String {
        let c = &self.catalog;
        let mut lines = vec![c.key(MessageKey::UiFactsHeader)];
        if let Some(n) = list.note {
            lines.push(c.key(match n {
                FactsUnavailable::SleepTimeOff => MessageKey::UiFactsNoteSleepOff,
                FactsUnavailable::StoppedByProbe => MessageKey::UiFactsNoteProbe,
                FactsUnavailable::KeyUnavailable => MessageKey::UiFactsNoteKey,
            }));
        }
        let rows = fact_rows(list);
        if rows.is_empty() {
            lines.push(c.key(MessageKey::UiFactsEmpty));
        }
        let before: Vec<&FactRow> = previous
            .map(|p| fact_rows(p).into_iter().map(|(_, r)| r).collect())
            .unwrap_or_default();
        let mut last_group: Option<&FactGroupKey> = None;
        for (i, (g, r)) in rows.iter().enumerate() {
            let n = (i + 1).to_string();
            let subject = self.subject_label(&g.group.subject);
            let attr = c.attribute(&g.attr.name);
            let first = last_group != Some(&g.group);
            last_group = Some(&g.group);
            let line = match r {
                FactRow::Value { value, .. } => c.with(
                    MessageKey::UiFactsRow,
                    &[
                        ("n", &n),
                        ("subject", &subject),
                        ("attr", &attr),
                        ("value", value),
                        ("marks", &self.value_marks(g, r, first)),
                    ],
                ),
                FactRow::Vacant { .. } => c.with(
                    MessageKey::UiFactsVacant,
                    &[
                        ("n", &n),
                        ("subject", &subject),
                        ("attr", &attr),
                        ("history", &g.history.to_string()),
                    ],
                ),
                FactRow::RuleOnly { .. } => c.with(
                    MessageKey::UiFactsRuleOnly,
                    &[("n", &n), ("subject", &subject), ("attr", &attr)],
                ),
            };
            // 前に見てから変わった行 (プロセスの間だけ、画面が前に出した一覧と比べる、04 §7.1)。
            if previous.is_some() && !before.iter().any(|b| same_row(b, r)) {
                lines.push(format!("{line}{}", c.key(MessageKey::UiFactsMarkChanged)));
            } else {
                lines.push(line);
            }
        }
        lines.push(String::new());
        lines.push(c.key(MessageKey::UiFactsHint));
        lines.join("\n")
    }

    /// 置き換えの履歴を文にする (`H` の番号)。
    fn history_text(&self, h: &FactHistory) -> String {
        let c = &self.catalog;
        let mut lines = vec![c.with(
            MessageKey::UiFactsHistoryTitle,
            &[
                ("subject", &self.subject_label(&h.group.subject)),
                ("attr", &c.attribute(&h.attr.name)),
            ],
        )];
        if h.items.is_empty() {
            lines.push(c.key(MessageKey::UiFactsHistoryEmpty));
        }
        for (i, it) in h.items.iter().enumerate() {
            let marks = if it.private {
                c.key(MessageKey::UiFactsMarkPrivate)
            } else {
                String::new()
            };
            let at = evorag_core::localtime::datetime(it.superseded_at.as_millis());
            lines.push(c.with(
                MessageKey::UiFactsHistoryRow,
                &[
                    ("n", &(i + 1).to_string()),
                    ("value", &it.value),
                    ("at", &at),
                    ("marks", &marks),
                ],
            ));
        }
        lines.push(String::new());
        lines.push(c.key(MessageKey::UiFactsHistoryHint));
        lines.join("\n")
    }

    /// 忘れた事実の一覧を文にする (`X` の番号。値は出さない、04 §7.6)。
    fn forgotten_text(&self, items: &[ForgottenFact]) -> String {
        let c = &self.catalog;
        let mut lines = vec![c.key(MessageKey::UiFactsForgottenTitle)];
        if items.is_empty() {
            lines.push(c.key(MessageKey::UiFactsForgottenEmpty));
        }
        for (i, f) in items.iter().enumerate() {
            let how = c.key(if f.redacted {
                MessageKey::UiFactsForgottenRedacted
            } else {
                MessageKey::UiFactsForgottenKept
            });
            let mut sessions = f.sessions.join("、");
            if f.deleted_turns > 0 {
                if !sessions.is_empty() {
                    sessions.push('、');
                }
                sessions.push_str(&c.with(
                    MessageKey::UiFactsForgottenDeleted,
                    &[("n", &f.deleted_turns.to_string())],
                ));
            }
            let at = evorag_core::localtime::datetime(f.at.as_millis());
            lines.push(c.with(
                MessageKey::UiFactsForgottenRow,
                &[
                    ("n", &(i + 1).to_string()),
                    ("subject", &self.subject_label(&f.subject)),
                    ("attr", &c.attribute(&f.attr.name)),
                    ("at", &at),
                    ("values", &f.values.to_string()),
                    ("how", &how),
                    ("sessions", &sessions),
                ],
            ));
        }
        lines.push(String::new());
        lines.push(c.key(MessageKey::UiFactsForgottenHint));
        lines.join("\n")
    }

    /// 窓を開く。確認の画面などほかの窓を開いているときは、それを閉じない (番号の対応だけが新しくなる)。
    fn open_panel(&mut self, title: String, body: String) {
        if matches!(self.mode, Mode::Chat | Mode::Panel { .. }) {
            self.mode = Mode::Panel {
                title,
                body,
                scroll: 0,
            };
        }
    }

    /// 事実の一覧を窓に出し、`F` の番号をこの一覧に結び付ける。`head` は一覧の前に置く文。
    fn show_facts(&mut self, list: FactList, head: Option<String>) {
        let previous = self.facts.list.take();
        let text = self.facts_text(&list, previous.as_ref());
        let body = match head {
            Some(h) => format!("{h}\n\n{text}"),
            None => text,
        };
        if previous.is_some() {
            self.facts.previous = previous;
        }
        self.facts.list = Some(list);
        let title = self.catalog.key(MessageKey::UiFactsTitle);
        self.open_panel(title, body);
    }

    /// 最後に表示した事実の一覧の行 `n` (`F` の番号)。一覧が無い・番号が無ければ知らせて `None`。
    fn fact_row(&mut self, n: usize) -> Option<(FactGroup, FactRow)> {
        let Some(list) = &self.facts.list else {
            let m = self
                .catalog
                .with(MessageKey::FactsNoList, &[("list", "/memory")]);
            self.notice(m);
            return None;
        };
        let found = fact_rows(list)
            .get(n - 1)
            .map(|(g, r)| ((*g).clone(), (*r).clone()));
        if found.is_none() {
            let m = self
                .catalog
                .with(MessageKey::FactsBadNumber, &[("n", &format!("F{n}"))]);
            self.notice(m);
        }
        found
    }

    fn not_allowed(&mut self) {
        let m = self.catalog.key(MessageKey::FactsNotAllowed);
        self.notice(m);
    }

    /// 事実への操作のコマンド (07 §3)。番号は最後に表示した一覧に結び付く (04 §7.1)。
    fn fact_command(&mut self, cmd: Slash) -> Vec<Action> {
        let c = |cmd| vec![Action::Command(cmd)];
        match cmd {
            Slash::MemoryHistory(n) => match self.fact_row(n) {
                Some((g, _)) => c(Command::ShowFactHistory { group: g.group }),
                None => Vec::new(),
            },
            Slash::MemoryAccept(n) => match self.fact_row(n) {
                Some((_, FactRow::Value { key, .. })) if key.status == FactStatus::Contested => {
                    c(Command::AcceptFact { target: key })
                }
                Some(_) => {
                    self.not_allowed();
                    Vec::new()
                }
                None => Vec::new(),
            },
            Slash::MemoryRevert(n) => {
                let Some(h) = &self.facts.history else {
                    let m = self
                        .catalog
                        .with(MessageKey::FactsNoList, &[("list", "/memory history F1")]);
                    self.notice(m);
                    return Vec::new();
                };
                match h.items.get(n - 1) {
                    Some(it) => c(Command::RevertFact {
                        target: it.key.clone(),
                    }),
                    None => {
                        let m = self
                            .catalog
                            .with(MessageKey::FactsBadNumber, &[("n", &format!("H{n}"))]);
                        self.notice(m);
                        Vec::new()
                    }
                }
            }
            Slash::MemoryForgotten => c(Command::ShowForgotten),
            Slash::MemoryUnblock(n) => {
                let Some(items) = &self.facts.forgotten else {
                    let m = self
                        .catalog
                        .with(MessageKey::FactsNoList, &[("list", "/memory forgotten")]);
                    self.notice(m);
                    return Vec::new();
                };
                match items.get(n - 1) {
                    Some(f) => c(Command::UnblockFact { target: f.target }),
                    None => {
                        let m = self
                            .catalog
                            .with(MessageKey::FactsBadNumber, &[("n", &format!("X{n}"))]);
                        self.notice(m);
                        Vec::new()
                    }
                }
            }
            Slash::Private(n) => {
                match self.fact_row(n) {
                    Some((g, r @ FactRow::Value { private: None, .. })) => {
                        let label = self.row_label(&g, &r);
                        let FactRow::Value { key, .. } = r else {
                            return Vec::new();
                        };
                        // 既定は「後継まで」(04 §7.5、OQ-47)。
                        self.mode = Mode::ConfirmPrivate {
                            target: key,
                            label,
                            multi: g.attr.multi,
                            scope: PrivateScope::Successors,
                        };
                    }
                    Some(_) => self.not_allowed(),
                    None => {}
                }
                Vec::new()
            }
            Slash::PrivateOff(n) => {
                let Some((g, r)) = self.fact_row(n) else {
                    return Vec::new();
                };
                let target = match &r {
                    FactRow::Value {
                        key,
                        private: Some(_),
                        ..
                    } => FactRowRef::Value(key.clone()),
                    FactRow::RuleOnly { group, .. } => FactRowRef::RuleOnly(group.clone()),
                    _ => {
                        self.not_allowed();
                        return Vec::new();
                    }
                };
                // ほかの行の印も外れるときは、その行を示す (04 §7.5 の「private の解除」)。
                let mine = row_directives(&r);
                let others: Vec<String> = self
                    .facts
                    .list
                    .as_ref()
                    .map(|list| {
                        fact_rows(list)
                            .into_iter()
                            .enumerate()
                            .filter(|(i, (_, o))| {
                                *i != n - 1 && row_directives(o).iter().any(|d| mine.contains(d))
                            })
                            .map(|(i, (og, o))| format!("F{} {}", i + 1, self.row_label(og, o)))
                            .collect()
                    })
                    .unwrap_or_default();
                // 履歴 (`Superseded`) の値の印も外れる (04 §7.5)。
                let history: Vec<String> = self
                    .facts
                    .list
                    .as_ref()
                    .map(|list| {
                        list.groups
                            .iter()
                            .flat_map(|hg| {
                                hg.private_history
                                    .iter()
                                    .filter(|h| h.directives.iter().any(|d| mine.contains(d)))
                                    .map(move |h| (hg, h))
                            })
                            .map(|(hg, h)| {
                                format!(
                                    "{}・{}: {}",
                                    self.subject_label(&hg.group.subject),
                                    self.catalog.attribute(&hg.attr.name),
                                    h.value
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let label = self.row_label(&g, &r);
                self.mode = Mode::ConfirmPrivateOff {
                    target,
                    label,
                    others,
                    history,
                };
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// 事実の窓・確認の画面のキー。
    fn on_fact_key(&mut self, key: Key) -> Vec<Action> {
        match self.mode.clone() {
            Mode::Panel {
                title,
                body,
                scroll,
            } => {
                let scroll = match key {
                    Key::Esc | Key::Enter { .. } | Key::Char('q') => {
                        self.mode = Mode::Chat;
                        return Vec::new();
                    }
                    Key::Up => scroll.saturating_sub(1),
                    Key::Down => scroll + 1,
                    Key::PageUp => scroll.saturating_sub(10),
                    Key::PageDown => scroll + 10,
                    Key::Home => 0,
                    _ => scroll,
                };
                // 本文の行の数を超えて送らない (描画の側でも窓の高さに合わせて止める)。
                let max = body.lines().count().saturating_sub(1);
                self.mode = Mode::Panel {
                    title,
                    body,
                    scroll: scroll.min(max),
                };
                Vec::new()
            }
            Mode::ConfirmPrivate {
                target,
                label,
                multi,
                scope,
            } => match key {
                Key::Esc => {
                    self.mode = Mode::Chat;
                    Vec::new()
                }
                Key::Up | Key::Down | Key::Tab => {
                    let scope = match scope {
                        PrivateScope::Successors => PrivateScope::ValueOnly,
                        PrivateScope::ValueOnly => PrivateScope::Successors,
                    };
                    self.mode = Mode::ConfirmPrivate {
                        target,
                        label,
                        multi,
                        scope,
                    };
                    Vec::new()
                }
                Key::Enter { .. } => {
                    self.mode = Mode::Chat;
                    vec![Action::Command(Command::SetFactPrivate { target, scope })]
                }
                _ => Vec::new(),
            },
            Mode::ConfirmPrivateOff { target, .. } => match key {
                Key::Esc => {
                    self.mode = Mode::Chat;
                    Vec::new()
                }
                Key::Enter { .. } => {
                    self.mode = Mode::Chat;
                    vec![Action::Command(Command::UnsetFactPrivate { target })]
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// 事実の一覧と操作の結果のイベント。扱ったら `true`。
    fn on_fact_event(&mut self, ev: &Event) -> bool {
        match ev {
            Event::FactsInfo { list } => {
                let head = if std::mem::take(&mut self.facts.awaiting) {
                    self.facts.memory.take()
                } else {
                    None
                };
                self.show_facts(list.clone(), head);
            }
            Event::FactHistoryInfo { history } => {
                let body = self.history_text(history);
                self.facts.history = Some(history.clone());
                let title = self.catalog.key(MessageKey::UiFactsTitle);
                self.open_panel(title, body);
            }
            Event::ForgottenInfo { items } => {
                let body = self.forgotten_text(items);
                self.facts.forgotten = Some(items.clone());
                let title = self.catalog.key(MessageKey::UiFactsTitle);
                self.open_panel(title, body);
            }
            Event::FactsUpdated {
                op,
                list,
                slow_next_turn,
            } => {
                let done = self.catalog.key(match op {
                    FactOp::Private => MessageKey::UiFactsDonePrivate,
                    FactOp::PrivateOff => MessageKey::UiFactsDonePrivateOff,
                    FactOp::Accept => MessageKey::UiFactsDoneAccept,
                    FactOp::Revert => MessageKey::UiFactsDoneRevert,
                    FactOp::Unblock => MessageKey::UiFactsDoneUnblock,
                });
                self.notice(done.clone());
                if *slow_next_turn {
                    let m = self.catalog.key(MessageKey::UiFactsSlowNext);
                    self.notice(m);
                }
                // 印を解いた行は、忘れた事実の一覧から外れる (`X` の番号は使えなくする)。
                self.facts.forgotten = None;
                if *op == FactOp::Unblock {
                    return true;
                }
                // 番号が変わるので、新しい一覧を出し、番号をそれに結び付ける。履歴の番号は使えなくする。
                self.facts.history = None;
                self.show_facts(list.clone(), Some(done));
            }
            Event::FactOpRejected { error, list } => {
                let m = self.catalog.message(error);
                self.notice(m.clone());
                if let Some(list) = list {
                    // 「一覧が更新されました」と出し、同じ読み取りで作った新しい一覧を出す (04 §7.1)。
                    self.facts.history = None;
                    self.show_facts(list.clone(), Some(m));
                } else if std::mem::take(&mut self.facts.awaiting) {
                    // `/memory` の事実の一覧を出せなかった: 想起の答えだけを出す。
                    if let Some(memory) = self.facts.memory.take() {
                        let title = self.catalog.key(MessageKey::UiFactsTitle);
                        self.open_panel(title, memory);
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// `/memory` の答えを文にする (FR-310、07 §3)。
    fn memory_text(&self, r: &evorag_types::api::MemoryReport) -> String {
        use evorag_types::api::MemoryPlace;
        let c = &self.catalog;
        let mut lines = Vec::new();
        if r.answer.is_none() {
            lines.push(c.key(MessageKey::UiMemoryNoAnswer));
        } else {
            lines.push(c.key(MessageKey::UiMemoryHeader));
            if r.items.is_empty() {
                lines.push(c.key(MessageKey::UiMemoryNothing));
            }
            let num = |m: i32| format!("{:.2}", f64::from(m) / 1000.0);
            for it in &r.items {
                let score = match (it.score_milli, it.delta_milli) {
                    (Some(raw), Some(d)) => c.with(
                        MessageKey::UiMemoryScore,
                        &[
                            (
                                "delta",
                                &format!("{}{}", if d >= 0 { "+" } else { "" }, num(d)),
                            ),
                            ("raw", &num(raw)),
                        ],
                    ),
                    (Some(raw), None) => {
                        c.with(MessageKey::UiMemoryScoreRaw, &[("raw", &num(raw))])
                    }
                    _ => String::new(),
                };
                let args = [
                    ("title", it.title.as_str()),
                    ("head", it.head.as_str()),
                    ("score", score.as_str()),
                ];
                lines.push(match it.place {
                    MemoryPlace::Attached => {
                        let n = it.n.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
                        let mut a = args.to_vec();
                        a.push(("n", n.as_str()));
                        c.with(MessageKey::UiMemoryAttached, &a)
                    }
                    MemoryPlace::Band => c.with(MessageKey::UiMemoryBand, &args),
                    MemoryPlace::Below => c.with(MessageKey::UiMemoryBelow, &args),
                });
            }
            for d in &r.degraded {
                let k = match d.as_str() {
                    "no_embed" => Some(MessageKey::UiMemoryDegradedNoEmbed),
                    "embed_deadline" => Some(MessageKey::UiMemoryDegradedEmbedDeadline),
                    "no_rerank" => Some(MessageKey::UiMemoryDegradedNoRerank),
                    "rerank_slow" => Some(MessageKey::UiMemoryDegradedRerankSlow),
                    "rerank_deadline" => Some(MessageKey::UiMemoryDegradedRerankDeadline),
                    "rerank_error" => Some(MessageKey::UiMemoryDegradedRerankError),
                    "no_threshold" => Some(MessageKey::UiMemoryDegradedNoThreshold),
                    "no_index" => Some(MessageKey::UiMemoryDegradedNoIndex),
                    _ => None,
                };
                lines.push(match k {
                    Some(k) => c.key(k),
                    None => c.with(MessageKey::UiMemoryDegradedOther, &[("reason", d)]),
                });
            }
        }
        if !r.sources.is_empty() {
            lines.push(c.key(MessageKey::UiMemorySources));
            for s in &r.sources {
                lines.push(c.with(
                    MessageKey::UiMemorySource,
                    &[
                        ("n", &s.n.to_string()),
                        ("title", &s.title),
                        ("head", &s.head),
                    ],
                ));
            }
        }
        lines.join("\n")
    }

    pub fn generating(&self) -> bool {
        self.streaming.is_some()
    }

    /// 入力が止まったときに先に済ませる処理へ渡す本文 (05 §4.5)。そのまま Enter で送れば発話になる
    /// ときだけ (空・スラッシュコマンド・生成中・セッションが無いときは `None`)。
    pub fn idle_text(&self) -> Option<String> {
        let text = self.input.text();
        if text.trim().is_empty()
            || slash::parse(text).is_some()
            || self.generating()
            || self.session.is_none()
        {
            return None;
        }
        Some(slash::unescape(text).to_owned())
    }

    fn notice(&mut self, text: String) {
        self.notices.push(text);
        if self.notices.len() > 5 {
            self.notices.remove(0);
        }
    }

    /// 応答を待たずに残っている発話か (起動したときに「応答なし」と示す、AC-FR-201-1)。
    pub fn has_no_reply(&self, t: &TurnView) -> bool {
        t.role == Role::User
            && t.status == TurnStatus::Pending
            && self.streaming.as_ref().is_none_or(|s| s.reply_to != t.id)
    }

    /// コアのイベントを受ける。
    pub fn on_event(&mut self, ev: Event) {
        if self.on_fact_event(&ev) {
            return;
        }
        match ev {
            Event::SessionOpened {
                session,
                turns,
                unreadable,
            } => {
                self.session = Some(session);
                self.turns = turns;
                self.streaming = None;
                self.scroll = 0;
                // 新しい版が書いた、この版では読めない記録 (03 §4.2 の規則 11)。
                if unreadable > 0 {
                    let m = self.catalog.with(
                        MessageKey::UiNoticeUnreadableTurns,
                        &[("count", &unreadable.to_string())],
                    );
                    self.notice(m);
                }
            }
            Event::UserTurnSaved { turn } => {
                self.turns.push(turn);
                self.scroll = 0;
            }
            Event::SendRejected { text, error } => {
                // 送れなかった発話は入力欄に戻す (04 §3 の 1)。
                if self.input.is_empty() {
                    self.input.set(&text);
                }
                let m = self.catalog.message(&error);
                self.notice(m);
            }
            Event::ResponseStarted { reply_to } => {
                self.streaming = Some(Streaming {
                    reply_to,
                    text: String::new(),
                    thinking: false,
                });
            }
            Event::Token { reply_to, text } => {
                if let Some(s) = self.streaming.as_mut().filter(|s| s.reply_to == reply_to) {
                    s.text.push_str(&text);
                    s.thinking = false;
                }
            }
            Event::Thinking { reply_to } => {
                if let Some(s) = self.streaming.as_mut().filter(|s| s.reply_to == reply_to) {
                    s.thinking = true;
                }
            }
            Event::ResponseFinished { turn, notice } => {
                self.streaming = None;
                // 発話は応答を書いたので「確定」になった。
                if let Some(u) = self.turns.iter_mut().find(|t| Some(t.id) == turn.reply_to)
                    && u.status == TurnStatus::Pending
                {
                    u.status = TurnStatus::Confirmed;
                }
                self.turns.push(turn);
                if let Some(n) = notice {
                    let m = self.catalog.message(&n);
                    self.notice(m);
                }
            }
            Event::Rejected { error } => {
                let m = self.catalog.message(&error);
                self.notice(m);
            }
            Event::SessionRenamed { session } => {
                let m = self
                    .catalog
                    .with(MessageKey::UiNoticeRenamed, &[("title", &session.title)]);
                self.session = Some(session);
                self.notice(m);
            }
            Event::SessionDeleted { .. } => {
                self.session = None;
                self.turns.clear();
                self.streaming = None;
                let m = self.catalog.key(MessageKey::UiNoticeDeleted);
                self.notice(m);
            }
            Event::MemoryInfo { report } => {
                let text = self.memory_text(&report);
                // `/memory` では、事実の一覧と一緒に窓に出す (一覧は通知の欄に出さない、07 §3)。
                if self.facts.awaiting {
                    self.facts.memory = Some(text);
                } else {
                    self.notice(text);
                }
            }
            Event::ModelInfo { roles, n_ctx } => {
                self.status.n_ctx = n_ctx;
                // 役割ごとに「記録 / 今」を示す (ファイル名と model_key の先頭、06 §3.1)。
                let show = |m: &Option<(String, String)>| match m {
                    Some((file, key)) => format!("{file} ({})", &key[..key.len().min(8)]),
                    None => "-".to_owned(),
                };
                let lines: Vec<String> = roles
                    .iter()
                    .map(|r| {
                        let status = self.catalog.key(status_key(r.status));
                        self.catalog.with(
                            MessageKey::UiModelRole,
                            &[
                                ("role", &r.role),
                                ("recorded", &show(&r.recorded)),
                                ("connected", &show(&r.connected)),
                                ("status", &status),
                            ],
                        )
                    })
                    .collect();
                self.notice(lines.join("\n"));
            }
            Event::RoleChanged { info, recorded_now } => {
                let file = info
                    .connected
                    .as_ref()
                    .map(|c| c.0.clone())
                    .unwrap_or_default();
                let m = match info.status {
                    RoleStatus::Ready if recorded_now => self.catalog.with(
                        MessageKey::CliRoleRecorded,
                        &[("role", &info.role), ("file", &file)],
                    ),
                    RoleStatus::Ready => self.catalog.with(
                        MessageKey::UiRoleReady,
                        &[("role", &info.role), ("file", &file)],
                    ),
                    _ => match role_notice(&self.catalog, &info) {
                        Some(m) => m,
                        None => return,
                    },
                };
                self.notice(m);
            }
            Event::BackgroundState { state } => {
                self.status.background = Some(state);
            }
            Event::RecallStatus { state } => {
                let before = self.status.recall.replace(state);
                // 較正が終わったときだけ知らせる (状態の行は常に今の状態を出す)。
                if let Some(RecallState::CandidatesOnly {
                    calibrating: true, ..
                }) = before
                {
                    let k = match state {
                        RecallState::Standard => MessageKey::UiCalibrated,
                        _ => MessageKey::UiCalibrationFailed,
                    };
                    let m = self.catalog.key(k);
                    self.notice(m);
                }
            }
            Event::ProbeIssues {
                role,
                thinking_not_off,
                structured_broken,
            } => {
                let mut reasons = Vec::new();
                if thinking_not_off {
                    reasons.push(self.catalog.key(MessageKey::UiProbeThinkingNotOff));
                }
                if structured_broken {
                    reasons.push(self.catalog.key(MessageKey::UiProbeStructuredBroken));
                }
                self.status.limited = true;
                let m = self.catalog.with(
                    MessageKey::UiProbeIssues,
                    &[("role", &role), ("reasons", &reasons.join("、"))],
                );
                self.notice(m);
            }
            Event::FactsChanged {
                added,
                replaced,
                contested,
            } => {
                // 件数だけを短く出す (値は出さない、04 §5.3 の「知らせ」)。
                let parts: Vec<String> = [
                    (MessageKey::UiFactsAdded, added),
                    (MessageKey::UiFactsReplaced, replaced),
                    (MessageKey::UiFactsContested, contested),
                ]
                .into_iter()
                .filter(|(_, n)| *n > 0)
                .map(|(k, n)| self.catalog.with(k, &[("n", &n.to_string())]))
                .collect();
                if !parts.is_empty() {
                    let m = self
                        .catalog
                        .with(MessageKey::UiFactsChanged, &[("parts", &parts.join("・"))]);
                    self.notice(m);
                }
            }
            Event::SourcesInfo { memory } => {
                let k = if memory {
                    MessageKey::UiSourcesOn
                } else {
                    MessageKey::UiSourcesOff
                };
                let m = self.catalog.key(k);
                self.notice(m);
            }
            Event::IrrelevantChanged {
                answer,
                target,
                irrelevant,
            } => {
                if let Some(t) = self.turns.iter_mut().find(|t| t.id == answer) {
                    t.irrelevant.retain(|x| *x != target);
                    if irrelevant {
                        t.irrelevant.push(target);
                    }
                }
                if let Mode::Detail {
                    target: shown,
                    irrelevant: flag,
                    ..
                } = &mut self.mode
                    && *shown == target
                {
                    *flag = irrelevant;
                }
                let k = if irrelevant {
                    MessageKey::UiIrrelevantMarked
                } else {
                    MessageKey::UiIrrelevantCleared
                };
                let m = self.catalog.key(k);
                self.notice(m);
            }
            Event::SourceDetail {
                n,
                answer,
                target,
                irrelevant,
                title,
                at,
                lines,
            } => {
                let excerpt = lines
                    .iter()
                    .map(|(role, text)| {
                        let who = match role {
                            Role::User => self.catalog.key(MessageKey::UiYou),
                            _ => self.catalog.key(MessageKey::UiAssistant),
                        };
                        let text: String = text.chars().take(300).collect();
                        format!("  {who}: {text}")
                    })
                    .collect::<Vec<_>>()
                    .join(
                        "
",
                    );
                let at = evorag_core::localtime::datetime(at.as_millis());
                let band = n == 0;
                let body = self.catalog.with(
                    if band {
                        MessageKey::UiBandDetail
                    } else {
                        MessageKey::UiSourceDetail
                    },
                    &[
                        ("n", &n.to_string()),
                        ("title", &title),
                        ("at", &at),
                        ("excerpt", &excerpt),
                    ],
                );
                let title = if band {
                    self.catalog.key(MessageKey::UiBandRelated)
                } else {
                    self.catalog
                        .with(MessageKey::UiDetailSource, &[("n", &n.to_string())])
                };
                self.mode = Mode::Detail {
                    answer,
                    target,
                    band,
                    irrelevant,
                    title,
                    body,
                    row: 0,
                };
            }
            Event::StyleInfo { values, changed } => {
                if std::mem::take(&mut self.style_picker) {
                    let mut choice = [0; 3];
                    for (i, item) in StyleItem::ALL.into_iter().enumerate() {
                        // 知らない値 (新しい版で足した値) は auto (先頭) として扱う。
                        choice[i] = item
                            .choices()
                            .iter()
                            .position(|c| *c == values[i])
                            .unwrap_or(0);
                    }
                    self.mode = Mode::Style {
                        row: 0,
                        choice,
                        initial: choice,
                    };
                    return;
                }
                if changed {
                    let m = self.catalog.key(MessageKey::UiStyleChanged);
                    self.notice(m);
                }
                let m = self.catalog.with(
                    MessageKey::UiStyleInfo,
                    &[
                        ("language", &values[0]),
                        ("length", &values[1]),
                        ("format", &values[2]),
                    ],
                );
                self.notice(m);
            }
            Event::BackgroundPaused { reason } => {
                let m = self.catalog.message(&reason);
                self.notice(m);
            }
            Event::Rated { good } => {
                let k = if good {
                    MessageKey::UiNoticeRatedGood
                } else {
                    MessageKey::UiNoticeRatedBad
                };
                let m = self.catalog.key(k);
                self.notice(m);
            }
            _ => {}
        }
    }

    /// セッションの一覧の照会の結果で、窓を開く。
    pub fn show_sessions(&mut self, list: Vec<SessionView>) {
        self.mode = Mode::Sessions { list, selected: 0 };
    }

    /// キーを受ける。
    pub fn on_key(&mut self, key: Key) -> Vec<Action> {
        if key != Key::CtrlC {
            self.ctrl_c_armed = false;
        }
        match self.mode.clone() {
            Mode::Help => {
                if matches!(key, Key::Esc | Key::Enter { .. } | Key::Char('q')) {
                    self.mode = Mode::Chat;
                }
                Vec::new()
            }
            Mode::Style {
                row,
                mut choice,
                initial,
            } => {
                let n = |i: usize| StyleItem::ALL[i].choices().len();
                match key {
                    Key::Esc => self.mode = Mode::Chat,
                    Key::Up | Key::Down => {
                        let row = if key == Key::Up {
                            row.saturating_sub(1)
                        } else {
                            (row + 1).min(2)
                        };
                        self.mode = Mode::Style {
                            row,
                            choice,
                            initial,
                        };
                    }
                    Key::Left | Key::Right => {
                        choice[row] = if key == Key::Left {
                            (choice[row] + n(row) - 1) % n(row)
                        } else {
                            (choice[row] + 1) % n(row)
                        };
                        self.mode = Mode::Style {
                            row,
                            choice,
                            initial,
                        };
                    }
                    Key::Enter { newline: false } => {
                        self.mode = Mode::Chat;
                        // 変えた項目だけを送る。
                        return (0..3)
                            .filter(|&i| choice[i] != initial[i])
                            .map(|i| {
                                let item = StyleItem::ALL[i];
                                Action::Command(Command::Style {
                                    set: Some((item, item.choices()[choice[i]].to_owned())),
                                })
                            })
                            .collect();
                    }
                    _ => {}
                }
                Vec::new()
            }
            Mode::ConfirmDelete => {
                self.mode = Mode::Chat;
                match key {
                    Key::Char('y' | 'Y') => vec![Action::Command(Command::DeleteSession)],
                    _ => Vec::new(),
                }
            }
            Mode::Sessions { list, selected } => match key {
                Key::Esc => {
                    self.mode = Mode::Chat;
                    Vec::new()
                }
                Key::Up => {
                    self.mode = Mode::Sessions {
                        selected: selected.saturating_sub(1),
                        list,
                    };
                    Vec::new()
                }
                Key::Down => {
                    let n = list.len();
                    self.mode = Mode::Sessions {
                        selected: (selected + 1).min(n.saturating_sub(1)),
                        list,
                    };
                    Vec::new()
                }
                Key::Enter { .. } => {
                    self.mode = Mode::Chat;
                    match list.get(selected) {
                        Some(s) => vec![Action::Command(Command::OpenSession { id: s.id })],
                        None => Vec::new(),
                    }
                }
                _ => Vec::new(),
            },
            Mode::Detail { .. } => self.on_detail_key(key),
            Mode::Panel { .. } | Mode::ConfirmPrivate { .. } | Mode::ConfirmPrivateOff { .. } => {
                self.on_fact_key(key)
            }
            Mode::Chat => self.on_chat_key(key),
        }
    }

    /// Tab: 出典か帯の付いた最後の応答の、次の出典・帯の往復を選ぶ (07 §2.5)。
    fn next_pick(&mut self) {
        let Some(t) = self
            .turns
            .iter()
            .rev()
            .find(|t| t.role == Role::Assistant && (t.sources > 0 || !t.band.is_empty()))
        else {
            let m = self.catalog.key(MessageKey::UiNoSources);
            self.notice(m);
            return;
        };
        // 番号はセッションの通し番号 (05 §5.1)。この応答の出典を巡り、続けて帯を巡る。
        let first = t.source_base.max(1);
        let mut all: Vec<Pick> = (first..first + t.sources)
            .map(|n| Pick::Source { answer: t.id, n })
            .collect();
        all.extend(t.band.iter().map(|b| Pick::Band {
            answer: t.id,
            target: b.target,
        }));
        let next = match self.pick.and_then(|p| all.iter().position(|x| *x == p)) {
            Some(i) => all[(i + 1) % all.len()],
            None => all[0],
        };
        self.pick = Some(next);
        let m = match next {
            Pick::Source { n, .. } => self
                .catalog
                .with(MessageKey::UiSourceSelected, &[("n", &n.to_string())]),
            Pick::Band { target, .. } => {
                let title = t
                    .band
                    .iter()
                    .find(|b| b.target == target)
                    .map(|b| b.title.clone())
                    .unwrap_or_default();
                self.catalog
                    .with(MessageKey::UiBandSelected, &[("title", &title)])
            }
        };
        self.notice(m);
    }

    /// 詳細の窓の操作 (05 §5.2)。
    fn on_detail_key(&mut self, key: Key) -> Vec<Action> {
        let Mode::Detail {
            answer,
            target,
            band,
            row,
            ..
        } = self.mode.clone()
        else {
            return Vec::new();
        };
        let actions = DetailAction::list(band);
        let run = |a: DetailAction| match a {
            DetailAction::Attach => Command::AttachBand { answer, target },
            DetailAction::Irrelevant => Command::ToggleIrrelevant { answer, target },
        };
        let chosen = match key {
            Key::Esc => {
                self.mode = Mode::Chat;
                return Vec::new();
            }
            Key::Up | Key::Down => {
                let n = actions.len();
                let next = if key == Key::Up {
                    (row + n - 1) % n
                } else {
                    (row + 1) % n
                };
                if let Mode::Detail { row, .. } = &mut self.mode {
                    *row = next;
                }
                return Vec::new();
            }
            Key::Enter { .. } => actions.get(row).copied(),
            Key::Char('a') if band => Some(DetailAction::Attach),
            Key::Char('x') => Some(DetailAction::Irrelevant),
            _ => None,
        };
        match chosen {
            Some(DetailAction::Attach) => {
                // 作り直しが始まるので、窓を閉じる。
                self.mode = Mode::Chat;
                vec![Action::Command(run(DetailAction::Attach))]
            }
            Some(a) => vec![Action::Command(run(a))],
            None => Vec::new(),
        }
    }

    fn on_chat_key(&mut self, key: Key) -> Vec<Action> {
        // 出典・帯を選んでいる間は、Enter で表示し、Esc でやめる。ほかのキーでも選ぶのをやめる。
        if let Some(p) = self.pick.take() {
            match key {
                Key::Tab => {
                    self.pick = Some(p);
                    self.next_pick();
                    return Vec::new();
                }
                Key::Enter { newline: false } if self.input.is_empty() => {
                    let c = match p {
                        Pick::Source { answer, n } => Command::ShowSource { turn: answer, n },
                        Pick::Band { answer, target } => Command::ShowBand { answer, target },
                    };
                    return vec![Action::Command(c)];
                }
                Key::Esc if !self.generating() => return Vec::new(),
                _ => {}
            }
        }
        match key {
            Key::Tab => {
                self.next_pick();
                Vec::new()
            }
            Key::CtrlQ => vec![Action::Quit],
            Key::CtrlE => vec![Action::OpenEditor],
            Key::CtrlC => {
                if self.generating() {
                    vec![Action::Command(Command::Interrupt)]
                } else if !self.input.is_empty() {
                    self.input.take();
                    Vec::new()
                } else if self.ctrl_c_armed {
                    vec![Action::Quit]
                } else {
                    self.ctrl_c_armed = true;
                    let m = self.catalog.key(MessageKey::UiQuitHint);
                    self.notice(m);
                    Vec::new()
                }
            }
            Key::Esc => {
                if self.generating() {
                    vec![Action::Command(Command::Interrupt)]
                } else {
                    Vec::new()
                }
            }
            Key::Enter { newline: true } => {
                self.input.insert("\n");
                Vec::new()
            }
            Key::Enter { newline: false } => self.submit(),
            Key::Char(c) => {
                let mut b = [0u8; 4];
                self.input.insert(c.encode_utf8(&mut b));
                Vec::new()
            }
            Key::Paste(s) => {
                self.input.insert(&s);
                Vec::new()
            }
            Key::Backspace => {
                self.input.backspace();
                Vec::new()
            }
            Key::Delete => {
                self.input.delete();
                Vec::new()
            }
            Key::Left => {
                self.input.left();
                Vec::new()
            }
            Key::Right => {
                self.input.right();
                Vec::new()
            }
            Key::Home => {
                self.input.home();
                Vec::new()
            }
            Key::End => {
                self.input.end();
                Vec::new()
            }
            Key::PageUp => {
                self.scroll += 10;
                Vec::new()
            }
            Key::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                Vec::new()
            }
            Key::Up | Key::Down => Vec::new(),
        }
    }

    fn submit(&mut self) -> Vec<Action> {
        let text = self.input.text().to_owned();
        if text.trim().is_empty() {
            return Vec::new();
        }
        if let Some(cmd) = slash::parse(&text) {
            self.input.take();
            return self.slash(cmd);
        }
        if self.generating() {
            // 生成中も書けるが、送るのは完了か中断の後 (07 §2.2)。入力欄に残す。
            let m = self.catalog.key(MessageKey::Busy);
            self.notice(m);
            return Vec::new();
        }
        self.input.take();
        let text = slash::unescape(&text).to_owned();
        let mut actions = Vec::new();
        if self.session.is_none() {
            let title = self.catalog.key(MessageKey::UiDefaultSessionTitle);
            actions.push(Action::Command(Command::NewSession { title }));
        }
        actions.push(Action::Command(Command::Send { text }));
        actions
    }

    fn slash(&mut self, cmd: Slash) -> Vec<Action> {
        let c = |cmd| vec![Action::Command(cmd)];
        match cmd {
            Slash::Help => {
                self.mode = Mode::Help;
                Vec::new()
            }
            Slash::New(title) => c(Command::NewSession {
                title: title.unwrap_or_else(|| self.catalog.key(MessageKey::UiDefaultSessionTitle)),
            }),
            Slash::Sessions => vec![Action::ListSessions],
            Slash::Rename(t) => c(Command::RenameSession { title: t }),
            Slash::Delete => {
                if self.session.is_some() {
                    self.mode = Mode::ConfirmDelete;
                } else {
                    let m = self.catalog.key(MessageKey::NoSession);
                    self.notice(m);
                }
                Vec::new()
            }
            Slash::Retry => c(Command::Retry),
            Slash::Good => c(Command::Rate {
                good: true,
                reason: None,
            }),
            Slash::Bad(reason) => c(Command::Rate {
                good: false,
                reason,
            }),
            Slash::Quit => vec![Action::Quit],
            Slash::Model(name) => c(Command::Model { name }),
            Slash::Sources(memory) => c(Command::Sources { memory }),
            Slash::Memory => {
                // 事実の一覧はデータ根の全体のものなので、セッションを開いていなくても出す (04 §7.1)。
                self.facts.memory = None;
                self.facts.awaiting = true;
                if self.session.is_some() {
                    vec![
                        Action::Command(Command::Memory),
                        Action::Command(Command::ShowFacts),
                    ]
                } else {
                    c(Command::ShowFacts)
                }
            }
            Slash::BadNumber(n) => {
                let m = self.catalog.with(MessageKey::FactsBadNumber, &[("n", &n)]);
                self.notice(m);
                Vec::new()
            }
            cmd @ (Slash::MemoryAccept(_)
            | Slash::MemoryHistory(_)
            | Slash::MemoryRevert(_)
            | Slash::MemoryForgotten
            | Slash::MemoryUnblock(_)
            | Slash::Private(_)
            | Slash::PrivateOff(_)) => self.fact_command(cmd),
            Slash::Style(set) => {
                self.style_picker = set.is_none();
                c(Command::Style { set })
            }
            Slash::NotYet(name) => {
                let m = self.catalog.with(MessageKey::UiNotYet, &[("name", &name)]);
                self.notice(m);
                Vec::new()
            }
            Slash::Invalid(name) => {
                let m = self
                    .catalog
                    .with(MessageKey::UiUnknownCommand, &[("name", &name)]);
                self.notice(m);
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evorag_types::{Message, Timestamp};

    fn app() -> App {
        App::new(Catalog::load("ja"), Status::default())
    }

    fn id(n: u8) -> Id {
        Id::from_bytes([n; 16])
    }

    fn turn(n: u8, role: Role, status: TurnStatus, reply_to: Option<Id>) -> TurnView {
        TurnView {
            id: id(n),
            session: id(0),
            role,
            status,
            text: format!("t{n}"),
            model_key: None,
            reply_to,
            created_at: Timestamp::from_millis(0),
            sources: 0,
            source_base: 1,
            band: Vec::new(),
            band_nearby: false,
            irrelevant: Vec::new(),
        }
    }

    fn session() -> SessionView {
        SessionView {
            id: id(0),
            title: "s".into(),
            created_at: Timestamp::from_millis(0),
            updated_at: Timestamp::from_millis(0),
        }
    }

    fn type_text(a: &mut App, s: &str) {
        for ch in s.chars() {
            a.on_key(Key::Char(ch));
        }
    }

    /// 事実が変わった知らせは、0 でない件数だけを並べる (04 §5.3 の「知らせ」)。
    #[test]
    fn facts_changes_show_counts_only() {
        let mut a = app();
        a.on_event(Event::FactsChanged {
            added: 2,
            replaced: 0,
            contested: 1,
        });
        let last = a.notices.last().expect("a notice");
        assert!(last.contains("事実 +2・矛盾 1"), "{last}");
        assert!(!last.contains("置き換え"), "{last}");
        let before = a.notices.len();
        a.on_event(Event::FactsChanged {
            added: 0,
            replaced: 0,
            contested: 0,
        });
        assert_eq!(a.notices.len(), before);
    }

    /// verifies: AC-FR-415-1 (入力欄の文字が 500 ms 変わらなければ、その文字で 1 回だけ知らせる)
    #[test]
    fn idle_input_is_signalled_once_per_text_after_500_ms() {
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        let mut w = IdleWatch::new(t0);
        assert!(!w.due("あ", ms(0)));
        assert!(!w.due("あ", ms(499)));
        assert!(w.due("あ", ms(500)));
        w.sent();
        assert!(!w.due("あ", ms(2000)));
        // 変わったら数え直す。
        assert!(!w.due("あい", ms(2100)));
        assert!(!w.due("あい", ms(2500)));
        assert!(w.due("あい", ms(2600)));
        // 知らせなかった (送れない文字だった) ときは、次に見たときにまた `true`。
        assert!(w.due("あい", ms(2700)));
    }

    /// verifies: AC-FR-415-1 (送れば発話になる文字だけを知らせる)
    #[test]
    fn only_sendable_text_is_offered_for_precompute() {
        let mut a = app();
        type_text(&mut a, "こんにちは");
        assert_eq!(a.idle_text(), None, "no session yet");
        a.session = Some(session());
        assert_eq!(a.idle_text().as_deref(), Some("こんにちは"));
        a.input.take();
        type_text(&mut a, "/help");
        assert_eq!(a.idle_text(), None);
        a.input.take();
        type_text(&mut a, "   ");
        assert_eq!(a.idle_text(), None);
    }

    /// verifies: AC-FR-310-1 (添えた往復と番号、帯、しきい値との差と生の値、縮退の理由、出典の対応を出す)
    #[test]
    fn memory_report_lists_items_reasons_and_sources() {
        use evorag_types::api::{MemoryItem, MemoryPlace, MemoryReport, MemorySource, RecallState};
        let a = app();
        let item = |place, n, score, delta| MemoryItem {
            target: id(1),
            place,
            n,
            title: "観葉植物".into(),
            head: "モンステラの水やり".into(),
            score_milli: score,
            delta_milli: delta,
        };
        let text = a.memory_text(&MemoryReport {
            answer: Some(id(9)),
            items: vec![
                item(MemoryPlace::Attached, Some(3), Some(4200), Some(1200)),
                item(MemoryPlace::Band, None, Some(2600), Some(-400)),
                item(MemoryPlace::Below, None, Some(-5000), None),
            ],
            degraded: vec!["no_embed".into(), "mystery".into()],
            recall: RecallState::Standard,
            sources: vec![MemorySource {
                n: 3,
                title: "観葉植物".into(),
                head: "モンステラの水やり".into(),
            }],
        });
        assert!(
            text.contains(
                "[3] 観葉植物「モンステラの水やり」 添えた (しきい値との差 +1.20、値 4.20)"
            ),
            "{text}"
        );
        assert!(
            text.contains("帯 観葉植物「モンステラの水やり」 (しきい値との差 -0.40、値 2.60)"),
            "{text}"
        );
        assert!(
            text.contains("下 観葉植物「モンステラの水やり」 (値 -5.00)"),
            "{text}"
        );
        assert!(text.contains("evorag setup で埋め込みのサーバ"), "{text}");
        assert!(text.contains("縮退: mystery"), "{text}");
        assert!(
            text.contains("このセッションの出典:\n  [3] 観葉植物「モンステラの水やり」"),
            "{text}"
        );
        let none = a.memory_text(&MemoryReport {
            answer: None,
            items: Vec::new(),
            degraded: Vec::new(),
            recall: RecallState::Simple,
            sources: Vec::new(),
        });
        assert_eq!(none, "このセッションには、まだ応答がありません。");
    }

    #[test]
    fn enter_sends_and_creates_a_session_when_none_is_open() {
        let mut a = app();
        type_text(&mut a, "こんにちは");
        let actions = a.on_key(Key::Enter { newline: false });
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[0],
            Action::Command(Command::NewSession { .. })
        ));
        assert_eq!(
            actions[1],
            Action::Command(Command::Send {
                text: "こんにちは".into()
            })
        );
        assert!(a.input.is_empty());
    }

    #[test]
    fn shift_enter_inserts_a_newline_and_slash_commands_are_parsed() {
        let mut a = app();
        a.on_event(Event::SessionOpened {
            session: session(),
            turns: vec![],
            unreadable: 0,
        });
        type_text(&mut a, "一");
        a.on_key(Key::Enter { newline: true });
        type_text(&mut a, "二");
        assert_eq!(a.input.text(), "一\n二");
        a.input.take();
        type_text(&mut a, "/retry");
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::Retry)]
        );
        type_text(&mut a, "/memory");
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![
                Action::Command(Command::Memory),
                Action::Command(Command::ShowFacts)
            ]
        );
        type_text(&mut a, "/forget 1");
        assert!(a.on_key(Key::Enter { newline: false }).is_empty());
        assert!(a.notices.last().unwrap().contains("/forget"));
    }

    /// verifies: AC-FR-105-1 (画面の層: 確認の後にだけ消す)
    #[test]
    fn delete_asks_for_confirmation_first() {
        let mut a = app();
        a.on_event(Event::SessionOpened {
            session: session(),
            turns: vec![],
            unreadable: 0,
        });
        type_text(&mut a, "/delete");
        assert!(a.on_key(Key::Enter { newline: false }).is_empty());
        assert_eq!(a.mode, Mode::ConfirmDelete);
        assert!(a.on_key(Key::Char('n')).is_empty());
        assert_eq!(a.mode, Mode::Chat);
        type_text(&mut a, "/delete");
        a.on_key(Key::Enter { newline: false });
        assert_eq!(
            a.on_key(Key::Char('y')),
            vec![Action::Command(Command::DeleteSession)]
        );
    }

    #[test]
    fn tokens_stream_into_the_pending_reply_and_esc_interrupts() {
        let mut a = app();
        a.on_event(Event::SessionOpened {
            session: session(),
            turns: vec![],
            unreadable: 0,
        });
        a.on_event(Event::UserTurnSaved {
            turn: turn(1, Role::User, TurnStatus::Pending, None),
        });
        a.on_event(Event::ResponseStarted { reply_to: id(1) });
        a.on_event(Event::Thinking { reply_to: id(1) });
        assert!(a.streaming.as_ref().unwrap().thinking, "thinking is shown");
        assert!(
            !a.has_no_reply(&a.turns[0].clone()),
            "streaming is not a missing reply"
        );
        a.on_event(Event::Token {
            reply_to: id(1),
            text: "こん".into(),
        });
        a.on_event(Event::Token {
            reply_to: id(1),
            text: "にちは".into(),
        });
        assert_eq!(a.streaming.as_ref().unwrap().text, "こんにちは");
        assert!(!a.streaming.as_ref().unwrap().thinking);
        // 生成中の Enter は送らず、入力欄に残す。
        type_text(&mut a, "次");
        assert!(a.on_key(Key::Enter { newline: false }).is_empty());
        assert_eq!(a.input.text(), "次");
        assert_eq!(
            a.on_key(Key::Esc),
            vec![Action::Command(Command::Interrupt)]
        );
        a.on_event(Event::ResponseFinished {
            turn: turn(2, Role::Assistant, TurnStatus::Interrupted, Some(id(1))),
            notice: Some(Message::new(MessageKey::Interrupted)),
        });
        assert!(!a.generating());
        assert_eq!(a.turns[0].status, TurnStatus::Confirmed);
        assert_eq!(a.turns.len(), 2);
    }

    /// verifies: AC-FR-201-1 (画面の層: 応答の無い発話に印を付ける)
    #[test]
    fn a_pending_utterance_after_restart_is_marked_as_no_reply() {
        let mut a = app();
        a.on_event(Event::SessionOpened {
            session: session(),
            turns: vec![turn(1, Role::User, TurnStatus::Pending, None)],
            unreadable: 0,
        });
        assert!(a.has_no_reply(&a.turns[0].clone()));
    }

    /// verifies: AC-FR-215-3 (画面の層: 読めなかった件数を知らせる)
    #[test]
    fn unreadable_turns_are_announced_when_a_session_opens() {
        let mut a = app();
        a.on_event(Event::SessionOpened {
            session: session(),
            turns: vec![turn(1, Role::User, TurnStatus::Pending, None)],
            unreadable: 2,
        });
        assert_eq!(a.turns.len(), 1, "the readable turns are still shown");
        let shown = a.notices.last().unwrap();
        assert!(shown.contains("2 件"), "{shown}");
        assert!(shown.contains("記録は残っています"), "{shown}");

        // 読めない記録が無ければ、何も知らせない。
        let mut b = app();
        b.on_event(Event::SessionOpened {
            session: session(),
            turns: vec![],
            unreadable: 0,
        });
        assert!(b.notices.is_empty());
    }

    #[test]
    fn rejected_send_returns_the_text_to_the_input() {
        let mut a = app();
        a.on_event(Event::SendRejected {
            text: "戻る".into(),
            error: Message::new(MessageKey::SendNotSaved).arg("detail", "disk"),
        });
        assert_eq!(a.input.text(), "戻る");
        assert!(a.notices[0].contains("disk"));
    }

    /// FR-107: Ctrl+E で外部のエディタを開く。
    #[test]
    fn ctrl_e_opens_the_external_editor() {
        let mut a = app();
        assert_eq!(a.on_key(Key::CtrlE), vec![Action::OpenEditor]);
    }

    #[test]
    fn ctrl_c_twice_on_an_empty_input_quits() {
        let mut a = app();
        assert!(a.on_key(Key::CtrlC).is_empty());
        assert_eq!(a.on_key(Key::CtrlC), vec![Action::Quit]);
        let mut a = app();
        a.on_key(Key::CtrlC);
        a.on_key(Key::Char('x'));
        a.input.take();
        assert!(
            a.on_key(Key::CtrlC).is_empty(),
            "the second press must be consecutive"
        );
    }

    #[test]
    fn session_picker_opens_the_selected_session() {
        let mut a = app();
        type_text(&mut a, "/sessions");
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::ListSessions]
        );
        let mut other = session();
        other.id = id(9);
        a.show_sessions(vec![session(), other]);
        a.on_key(Key::Down);
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::OpenSession { id: id(9) })]
        );
        assert_eq!(a.mode, Mode::Chat);
    }

    /// verifies: AC-FR-110-1 (画面の層: Tab で番号を選び、Enter で照会し、詳細を示す)
    #[test]
    fn tab_selects_a_source_and_enter_shows_it() {
        let mut a = app();
        assert!(a.on_key(Key::Tab).is_empty());
        assert!(a.notices.last().unwrap().contains("出典や関係しそうな記録"));
        let mut answer = turn(2, Role::Assistant, TurnStatus::Completed, Some(id(1)));
        answer.sources = 2;
        a.turns = vec![turn(1, Role::User, TurnStatus::Confirmed, None), answer];
        a.on_key(Key::Tab);
        a.on_key(Key::Tab);
        assert_eq!(
            a.pick,
            Some(Pick::Source {
                answer: id(2),
                n: 2
            })
        );
        a.on_key(Key::Tab);
        assert_eq!(
            a.pick,
            Some(Pick::Source {
                answer: id(2),
                n: 1
            }),
            "wraps around"
        );
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::ShowSource { turn: id(2), n: 1 })]
        );
        assert_eq!(a.pick, None);
        // Esc でやめる。ほかのキーは普通に効く。
        a.on_key(Key::Tab);
        assert!(a.on_key(Key::Esc).is_empty());
        assert_eq!(a.pick, None);
        a.on_key(Key::Tab);
        a.on_key(Key::Char('x'));
        assert_eq!((a.pick, a.input.text()), (None, "x"));

        a.on_event(Event::SourceDetail {
            n: 1,
            answer: id(9),
            target: id(8),
            irrelevant: false,
            title: "旅行の計画".into(),
            at: Timestamp::from_millis(0),
            lines: vec![
                (Role::User, "宿はどこ？".into()),
                (Role::Assistant, "町家です".into()),
            ],
        });
        let Mode::Detail {
            body, band, title, ..
        } = &a.mode
        else {
            panic!("{:?}", a.mode)
        };
        assert!(body.starts_with("[1] 旅行の計画 ("), "{body}");
        assert!(body.contains("あなた: 宿はどこ？"), "{body}");
        assert!(!band);
        assert_eq!(title, "出典 [1]");
        // 出典の詳細でも「無関係」の印を付けられる (x)。添えて作り直すは帯だけ。
        assert!(a.on_key(Key::Char('a')).is_empty());
        assert_eq!(
            a.on_key(Key::Char('x')),
            vec![Action::Command(Command::ToggleIrrelevant {
                answer: id(9),
                target: id(8)
            })]
        );
        a.on_key(Key::Esc);
        assert_eq!(a.mode, Mode::Chat);
    }

    fn band_item(n: u8) -> evorag_types::api::BandItem {
        evorag_types::api::BandItem {
            target: id(n),
            title: format!("会話 {n}"),
            at: Timestamp::from_millis(0),
            head: "モンステラの水やり".into(),
        }
    }

    /// verifies: AC-FR-412-1 (画面の層: 帯を Tab で選び、詳細で a を選ぶと作り直しの命令が出る)
    #[test]
    fn the_band_is_picked_after_the_sources_and_attached_with_a() {
        let mut a = app();
        let mut answer = turn(2, Role::Assistant, TurnStatus::Completed, Some(id(1)));
        answer.sources = 1;
        answer.band = vec![band_item(5), band_item(6)];
        a.turns = vec![turn(1, Role::User, TurnStatus::Confirmed, None), answer];
        a.on_key(Key::Tab);
        a.on_key(Key::Tab);
        assert_eq!(
            a.pick,
            Some(Pick::Band {
                answer: id(2),
                target: id(5)
            })
        );
        assert!(a.notices.last().unwrap().contains("会話 5"));
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::ShowBand {
                answer: id(2),
                target: id(5)
            })]
        );
        a.on_event(Event::SourceDetail {
            n: 0,
            answer: id(2),
            target: id(5),
            irrelevant: false,
            title: "会話 5".into(),
            at: Timestamp::from_millis(0),
            lines: vec![(Role::User, "モンステラの水やり".into())],
        });
        assert!(matches!(
            a.mode,
            Mode::Detail {
                band: true,
                row: 0,
                ..
            }
        ));
        // ↓ と Enter で 2 つ目の操作 (無関係の印)。
        a.on_key(Key::Down);
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::ToggleIrrelevant {
                answer: id(2),
                target: id(5)
            })]
        );
        // 印が付くと、知らせを出し、帯に印を出し、窓の操作が「取り消す」になる。
        a.on_event(Event::IrrelevantChanged {
            answer: id(2),
            target: id(5),
            irrelevant: true,
        });
        assert!(a.notices.last().unwrap().contains("判定は変わりません"));
        assert_eq!(a.turns[1].irrelevant, [id(5)]);
        assert!(matches!(
            a.mode,
            Mode::Detail {
                irrelevant: true,
                ..
            }
        ));
        // a で添えて作り直す。窓は閉じる。
        assert_eq!(
            a.on_key(Key::Char('a')),
            vec![Action::Command(Command::AttachBand {
                answer: id(2),
                target: id(5)
            })]
        );
        assert_eq!(a.mode, Mode::Chat);
    }

    /// verifies: AC-FR-409-1 (画面の層)
    #[test]
    fn sources_command_switches_memory() {
        let mut a = app();
        type_text(&mut a, "/sources off");
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::Sources {
                memory: Some(false)
            })]
        );
        a.on_event(Event::SourcesInfo { memory: false });
        assert!(a.notices.last().unwrap().contains("添えません"));
    }

    /// verifies: AC-FR-111-1 (画面の層)
    #[test]
    fn style_opens_a_picker_and_sends_only_the_changed_items() {
        let mut a = app();
        type_text(&mut a, "/style");
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::Style { set: None })]
        );
        // 知らない値 (新しい版で足した値) は auto として選び始める。
        a.on_event(Event::StyleInfo {
            values: ["ja".into(), "auto".into(), "odd".into()],
            changed: false,
        });
        assert!(matches!(a.mode, Mode::Style { row: 0, .. }));
        a.on_key(Key::Down);
        a.on_key(Key::Right);
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::Style {
                set: Some((StyleItem::Length, "short".into()))
            })]
        );
        assert_eq!(a.mode, Mode::Chat);
        // 変えた後の知らせ: すべてのセッションに効くことと、次の応答が少し遅いこと。
        a.on_event(Event::StyleInfo {
            values: ["ja".into(), "short".into(), "auto".into()],
            changed: true,
        });
        let n = a.notices.join("\n");
        assert!(n.contains("すべてのセッションに効きます"), "{n}");
        assert!(n.contains("長さ short"), "{n}");

        // 値を付けて打てば、窓を開かずに送る。Esc で窓を閉じると何も送らない。
        type_text(&mut a, "/style language en");
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::Style {
                set: Some((StyleItem::Language, "en".into()))
            })]
        );
        type_text(&mut a, "/style");
        a.on_key(Key::Enter { newline: false });
        a.on_event(Event::StyleInfo {
            values: ["en".into(), "short".into(), "auto".into()],
            changed: false,
        });
        a.on_key(Key::Right);
        assert_eq!(a.on_key(Key::Esc), Vec::new());
        assert_eq!(a.mode, Mode::Chat);
    }

    /// verifies: AC-FR-903-1 (07 §3 の v0.1 のコマンドがすべて /help に出て、使える)
    #[test]
    fn help_lists_every_command() {
        let a = app();
        let help = a.catalog.key(MessageKey::UiHelpBody);
        for (name, example) in [
            ("/help", "/help"),
            ("/new", "/new 題"),
            ("/sessions", "/sessions"),
            ("/rename", "/rename 題"),
            ("/delete", "/delete"),
            ("/retry", "/retry"),
            ("/good", "/good"),
            ("/bad", "/bad 理由"),
            ("/sources", "/sources off"),
            ("/style", "/style length short"),
            ("/model", "/model"),
            ("/quit", "/quit"),
        ] {
            assert!(help.contains(name), "{name} is missing from /help");
            assert!(
                !matches!(
                    slash::parse(example),
                    None | Some(Slash::Invalid(_) | Slash::NotYet(_))
                ),
                "{example} must work"
            );
        }
    }

    // 事実の一覧と事実への操作 (04 §7.1・§7.4〜§7.6、07 §3)。

    use evorag_types::api::{
        AttrInfo, FactGroupKey, FactHistory, FactHistoryRow, FactKey, FactList, FactRow,
        FactRowRef, FactStatus, FactSubject, FactTense, ForgottenFact, ForgottenRef, PrivateCause,
        PrivateInfo, PrivateScope,
    };

    fn fkey(attribute: &str, vk: char, status: FactStatus) -> FactKey {
        FactKey {
            subject: FactSubject::User,
            attribute: attribute.into(),
            vk: vk.to_string().repeat(64),
            status,
            tense: FactTense::Current,
            fact: None,
        }
    }

    fn fgroup(attribute: &str) -> FactGroupKey {
        FactGroupKey {
            subject: FactSubject::User,
            attribute: attribute.into(),
        }
    }

    fn value_row(key: FactKey, value: &str, private: Option<PrivateInfo>) -> FactRow {
        FactRow::Value {
            key,
            value: value.into(),
            private,
        }
    }

    fn group(attribute: &str, multi: bool, rows: Vec<FactRow>, history: usize) -> FactGroup {
        FactGroup {
            group: fgroup(attribute),
            attr: AttrInfo {
                name: attribute.into(),
                multi,
            },
            rows,
            history,
            private_history: Vec::new(),
        }
    }

    /// 住んでいる所 (今の値と矛盾の値)、食べないもの (private の 2 行が同じ指示を共有)、ペット (今の値なし)、
    /// 趣味 (指示だけが残る)。
    fn sample_list() -> FactList {
        let shared = PrivateInfo {
            causes: vec![PrivateCause::Chosen {
                scope: PrivateScope::Successors,
            }],
            directives: vec![id(50)],
        };
        let successor = PrivateInfo {
            causes: vec![PrivateCause::Successor],
            directives: vec![id(50)],
        };
        let mut list = FactList {
            groups: vec![
                group(
                    "residence",
                    false,
                    vec![
                        value_row(fkey("residence", 'a', FactStatus::Live), "大阪", None),
                        value_row(fkey("residence", 'b', FactStatus::Contested), "東京", None),
                    ],
                    1,
                ),
                group(
                    "dietary",
                    true,
                    vec![
                        value_row(fkey("dietary", 'c', FactStatus::Live), "えび", Some(shared)),
                        value_row(
                            fkey("dietary", 'd', FactStatus::Live),
                            "かに",
                            Some(successor),
                        ),
                    ],
                    0,
                ),
                group(
                    "pet",
                    false,
                    vec![FactRow::Vacant {
                        group: fgroup("pet"),
                    }],
                    2,
                ),
                group(
                    "hobby",
                    true,
                    vec![FactRow::RuleOnly {
                        group: fgroup("hobby"),
                        directives: vec![id(60)],
                    }],
                    0,
                ),
            ],
            note: None,
        };
        // 同じ指示で private の履歴の値。
        list.groups[1].private_history = vec![evorag_types::api::PrivateHistoryValue {
            value: "たこ".into(),
            directives: vec![id(50)],
        }];
        list
    }

    fn panel(a: &App) -> String {
        match &a.mode {
            Mode::Panel { body, .. } => body.clone(),
            other => panic!("not a panel: {other:?}"),
        }
    }

    fn run(a: &mut App, line: &str) -> Vec<Action> {
        a.mode = Mode::Chat;
        type_text(a, line);
        a.on_key(Key::Enter { newline: false })
    }

    /// `/memory` は、セッションが無くても事実の一覧を照会し、一覧を通知の欄ではなく窓に出す。番号は値の行のほか、
    /// 今の値が消えた属性と指示だけが残る属性にも振り、印と理由を付ける。前に見てから変わった行に印を付ける。
    ///
    /// verifies: AC-FR-310-2
    #[test]
    fn the_fact_list_opens_in_a_panel_with_numbers_and_marks() {
        let mut a = app();
        assert_eq!(
            run(&mut a, "/memory"),
            vec![Action::Command(Command::ShowFacts)],
            "no session: only the facts"
        );
        let notices = a.notices.len();
        a.on_event(Event::FactsInfo {
            list: sample_list(),
        });
        assert_eq!(a.notices.len(), notices, "not in the notices");
        let body = panel(&a);
        for want in [
            "F1 あなた・住んでいる所: 大阪 (履歴 1 件)",
            "F2 あなた・住んでいる所: 東京 [矛盾]",
            "F3 あなた・食べないもの: えび [private: 選んだ値 (後の値にも効く)]",
            "F4 あなた・食べないもの: かに [private: 指示の後に言われた値]",
            "F5 あなた・ペット: (今の値なし。履歴 2 件",
            "F6 あなた・趣味: (値なし。",
        ] {
            assert!(body.contains(want), "{want}\n{body}");
        }
        assert!(
            !body.contains("前に見てから"),
            "the first list has no marks"
        );
        // 窓はスクロールでき、Esc で閉じる。
        a.on_key(Key::Down);
        assert!(matches!(a.mode, Mode::Panel { scroll: 1, .. }));
        a.on_key(Key::Esc);
        assert_eq!(a.mode, Mode::Chat);

        // セッションがあれば、想起の答えと一緒に 1 つの窓に出す。前の一覧から変わった行に印を付ける。
        a.session = Some(session());
        assert_eq!(
            run(&mut a, "/memory"),
            vec![
                Action::Command(Command::Memory),
                Action::Command(Command::ShowFacts)
            ]
        );
        a.on_event(Event::MemoryInfo {
            report: evorag_types::api::MemoryReport {
                answer: None,
                items: Vec::new(),
                degraded: Vec::new(),
                recall: RecallState::Simple,
                sources: Vec::new(),
            },
        });
        let mut next = sample_list();
        next.groups[0].rows[0] = value_row(fkey("residence", 'e', FactStatus::Live), "京都", None);
        a.on_event(Event::FactsInfo { list: next });
        let body = panel(&a);
        assert!(
            body.starts_with("このセッションには、まだ応答がありません。"),
            "{body}"
        );
        assert!(
            body.contains("F1 あなた・住んでいる所: 京都 (履歴 1 件) ← 前に見てから変わりました"),
            "{body}"
        );
        assert!(
            body.contains("F2 あなた・住んでいる所: 東京 [矛盾]\n"),
            "{body}"
        );
    }

    /// `/private F<n>` は確認の画面を出し、既定 (後継まで) のまま Enter で実行できる。↑↓ で「この値だけ」を選べる。
    /// 多値の属性では兄弟の値が対象にならないことを、`llm.allow_remote` の構成では外へ出うることを書く。
    ///
    /// verifies: AC-FR-308-1
    #[test]
    fn private_asks_with_the_scope_and_sends_the_row_reference() {
        let mut a = app();
        assert!(run(&mut a, "/private F1").is_empty());
        assert!(a.notices.last().unwrap().contains("/memory"), "list first");
        a.on_event(Event::FactsInfo {
            list: sample_list(),
        });
        assert!(run(&mut a, "/private 1").is_empty());
        let Mode::ConfirmPrivate {
            label,
            scope,
            multi,
            ..
        } = &a.mode
        else {
            panic!("{:?}", a.mode)
        };
        assert_eq!(label, "あなた・住んでいる所: 大阪");
        assert_eq!(*scope, PrivateScope::Successors);
        assert!(!multi);
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::SetFactPrivate {
                target: fkey("residence", 'a', FactStatus::Live),
                scope: PrivateScope::Successors,
            })]
        );
        assert_eq!(a.mode, Mode::Chat);

        // 「この値だけ」を選ぶ。Esc なら何も送らない。
        a.status.remote_llm = true;
        assert!(run(&mut a, "/private F2").is_empty());
        let text = match &a.mode {
            Mode::ConfirmPrivate {
                label,
                multi,
                scope,
                ..
            } => crate::render::private_confirm_text(&a, label, *multi, *scope),
            other => panic!("{other:?}"),
        };
        assert!(text.contains("消しません"), "{text}");
        assert!(text.contains("これまでのやり取りには残ります"), "{text}");
        assert!(text.contains("送らなくなるだけ"), "{text}");
        assert!(text.contains("llm.allow_remote"), "{text}");
        a.on_key(Key::Down);
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::SetFactPrivate {
                target: fkey("residence", 'b', FactStatus::Contested),
                scope: PrivateScope::ValueOnly,
            })]
        );
        run(&mut a, "/private F3");
        assert_eq!(a.mode, Mode::Chat, "already private");
        assert!(a.notices.last().unwrap().contains("できません"));
        run(&mut a, "/private F5");
        assert_eq!(a.mode, Mode::Chat, "a vacant row has no value");
        run(&mut a, "/private F9");
        assert!(a.notices.last().unwrap().contains("F9"));
        // 多値の属性では、兄弟の値が対象にならないことを書く。
        a.facts.list.as_mut().unwrap().groups[1].rows[0] =
            value_row(fkey("dietary", 'c', FactStatus::Live), "えび", None);
        run(&mut a, "/private F3");
        let Mode::ConfirmPrivate {
            label,
            multi: true,
            scope,
            ..
        } = &a.mode
        else {
            panic!("{:?}", a.mode)
        };
        let text = crate::render::private_confirm_text(&a, label, true, *scope);
        assert!(text.contains("兄弟の値"), "{text}");
        assert!(a.on_key(Key::Esc).is_empty());
        assert_eq!(a.mode, Mode::Chat);
    }

    /// `/private off` は、同じ指示を共有するほかの行も外れることを確認の画面で示す。指示だけが残る属性の行も
    /// 解ける。private でない行には使えない (04 §7.5)。
    ///
    /// verifies: AC-FR-308-2
    #[test]
    fn private_off_lists_the_other_rows_sharing_a_directive() {
        let mut a = app();
        a.on_event(Event::FactsInfo {
            list: sample_list(),
        });
        assert!(run(&mut a, "/private off F3").is_empty());
        let Mode::ConfirmPrivateOff {
            label,
            others,
            history,
            ..
        } = &a.mode
        else {
            panic!("{:?}", a.mode)
        };
        assert_eq!(label, "あなた・食べないもの: えび");
        assert_eq!(others, &["F4 あなた・食べないもの: かに".to_owned()]);
        // 同じ指示で private の履歴の値も、件数と行を示す。
        assert_eq!(history, &["あなた・食べないもの: たこ".to_owned()]);
        let text = crate::render::private_off_text(&a, label, others, history);
        assert!(text.contains("履歴の値 1 件"), "{text}");
        assert!(text.contains("たこ"), "{text}");
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::UnsetFactPrivate {
                target: FactRowRef::Value(fkey("dietary", 'c', FactStatus::Live)),
            })]
        );
        run(&mut a, "/private off F6");
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::UnsetFactPrivate {
                target: FactRowRef::RuleOnly(fgroup("hobby")),
            })]
        );
        assert!(run(&mut a, "/private off F1").is_empty());
        assert_eq!(a.mode, Mode::Chat);
        assert!(a.notices.last().unwrap().contains("できません"));
    }

    /// accept は矛盾の行にだけ。history は値の無い行からも届く。revert は履歴の番号 (H) を取る。
    #[test]
    fn accept_history_and_revert_use_their_own_numbers() {
        let mut a = app();
        a.on_event(Event::FactsInfo {
            list: sample_list(),
        });
        assert_eq!(
            run(&mut a, "/memory accept F2"),
            vec![Action::Command(Command::AcceptFact {
                target: fkey("residence", 'b', FactStatus::Contested),
            })]
        );
        assert!(run(&mut a, "/memory accept F1").is_empty(), "not contested");
        assert!(run(&mut a, "/memory revert H1").is_empty());
        assert!(a.notices.last().unwrap().contains("/memory history"));
        assert_eq!(
            run(&mut a, "/memory history F5"),
            vec![Action::Command(Command::ShowFactHistory {
                group: fgroup("pet"),
            })]
        );
        let old = fkey("pet", 'f', FactStatus::Superseded);
        a.on_event(Event::FactHistoryInfo {
            history: FactHistory {
                group: fgroup("pet"),
                attr: AttrInfo {
                    name: "pet".into(),
                    multi: false,
                },
                items: vec![FactHistoryRow {
                    key: old.clone(),
                    value: "柴犬".into(),
                    superseded_at: Timestamp::from_millis(0),
                    private: false,
                }],
            },
        });
        assert!(panel(&a).contains("H1 柴犬 ("), "{}", panel(&a));
        assert_eq!(
            run(&mut a, "/memory revert 1"),
            vec![Action::Command(Command::RevertFact { target: old })]
        );
        assert!(run(&mut a, "/memory revert H2").is_empty());
        assert!(a.notices.last().unwrap().contains("H2"));
    }

    /// 指した行が変わっていたときは「一覧が更新されました」と出し、新しい一覧を出して番号をそれに結び付ける
    /// (04 §7.1)。操作が済んだときも、新しい一覧を出す。
    ///
    /// verifies: AC-FR-310-2
    #[test]
    fn an_outdated_list_is_replaced_and_numbers_follow_the_new_one() {
        let mut a = app();
        a.on_event(Event::FactsInfo {
            list: sample_list(),
        });
        let mut fresh = sample_list();
        fresh.groups.remove(0);
        a.on_event(Event::FactOpRejected {
            error: evorag_types::Message::new(MessageKey::FactsOutdated),
            list: Some(fresh),
        });
        let body = panel(&a);
        assert!(body.starts_with("一覧が更新されました。"), "{body}");
        assert!(body.contains("F1 あなた・食べないもの: えび"), "{body}");
        // 番号は新しい一覧のもの。
        assert!(run(&mut a, "/private off F1").is_empty());
        assert!(matches!(
            &a.mode,
            Mode::ConfirmPrivateOff { label, .. } if label == "あなた・食べないもの: えび"
        ));
        a.mode = Mode::Chat;
        a.on_event(Event::FactsUpdated {
            op: evorag_types::api::FactOp::Accept,
            list: sample_list(),
            slow_next_turn: true,
        });
        let body = panel(&a);
        assert!(body.starts_with("その値を今の値にしました。"), "{body}");
        assert!(a.notices.join("\n").contains("少し長くかかります"));
        assert!(run(&mut a, "/memory accept F2").len() == 1);
    }

    /// 一覧の知らせが確認の画面の間に届いても、確認の画面を閉じない (番号の対応だけが新しくなる)。
    #[test]
    fn fact_events_do_not_replace_a_confirmation() {
        let mut a = app();
        a.on_event(Event::FactsInfo {
            list: sample_list(),
        });
        run(&mut a, "/private F1");
        assert!(matches!(a.mode, Mode::ConfirmPrivate { .. }));
        let mut fresh = sample_list();
        fresh.groups.remove(0);
        a.on_event(Event::FactsUpdated {
            op: evorag_types::api::FactOp::PrivateOff,
            list: fresh.clone(),
            slow_next_turn: true,
        });
        assert!(
            matches!(a.mode, Mode::ConfirmPrivate { .. }),
            "{:?}",
            a.mode
        );
        assert_eq!(a.facts.list.as_ref(), Some(&fresh), "the numbers follow");
        a.on_event(Event::FactsInfo {
            list: sample_list(),
        });
        assert!(matches!(a.mode, Mode::ConfirmPrivate { .. }));
        // 開いていた窓は、新しい一覧に替える。
        a.mode = Mode::Chat;
        a.on_event(Event::FactsInfo {
            list: sample_list(),
        });
        assert!(matches!(a.mode, Mode::Panel { .. }));
    }

    /// `/memory` で事実の一覧を出せなかったときも、想起の答えは出す。
    #[test]
    fn the_recall_part_of_memory_shows_when_the_facts_fail() {
        let mut a = app();
        a.session = Some(session());
        run(&mut a, "/memory");
        a.on_event(Event::MemoryInfo {
            report: evorag_types::api::MemoryReport {
                answer: None,
                items: Vec::new(),
                degraded: Vec::new(),
                recall: RecallState::Simple,
                sources: Vec::new(),
            },
        });
        a.on_event(Event::FactOpRejected {
            error: evorag_types::Message::new(MessageKey::FactsNotSaved).arg("detail", "x"),
            list: None,
        });
        assert!(panel(&a).contains("まだ応答がありません"), "{}", panel(&a));
        assert!(!a.facts.awaiting);
    }

    /// 忘れた事実の一覧は値を出さず、`X` の番号で印を解ける (04 §7.6)。
    #[test]
    fn forgotten_facts_are_listed_without_values_and_unblocked_by_number() {
        let mut a = app();
        assert!(run(&mut a, "/memory unblock X1").is_empty());
        assert!(a.notices.last().unwrap().contains("/memory forgotten"));
        assert_eq!(
            run(&mut a, "/memory forgotten"),
            vec![Action::Command(Command::ShowForgotten)]
        );
        let target = ForgottenRef { directive: id(70) };
        a.on_event(Event::ForgottenInfo {
            items: vec![ForgottenFact {
                target,
                subject: FactSubject::Named("妻".into()),
                attr: AttrInfo {
                    name: "hobby".into(),
                    multi: true,
                },
                at: Timestamp::from_millis(0),
                values: 2,
                redacted: false,
                sessions: vec!["旅行".into()],
                deleted_turns: 1,
            }],
        });
        let body = panel(&a);
        assert!(body.contains("X1 妻・趣味 ("), "{body}");
        assert!(
            body.contains("値 2 件、発話はそのまま) 出典: 旅行、消したターン 1 件"),
            "{body}"
        );
        assert_eq!(
            run(&mut a, "/memory unblock X1"),
            vec![Action::Command(Command::UnblockFact { target })]
        );
        a.on_event(Event::FactsUpdated {
            op: evorag_types::api::FactOp::Unblock,
            list: sample_list(),
            slow_next_turn: false,
        });
        assert_eq!(a.mode, Mode::Chat, "no list for an unblock");
        assert!(a.facts.forgotten.is_none(), "the X numbers are gone");
    }
}
