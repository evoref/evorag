//! 画面の状態と操作 (07 §2・§3)。端末に依存しない (試験しやすくするため)。
//!
//! キーとコアのイベントを受けて状態を変え、コアへの命令 (`Action`) を返す。描画は `render`。

use evorag_types::api::{
    Command, Event, ModelRoleInfo, Role, RoleStatus, SessionView, StyleItem, TurnStatus, TurnView,
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
    /// 答え方の既定を選ぶ窓 (07 §3 の `/style`)。`choice` は項目ごとの値の位置。
    Style {
        row: usize,
        choice: [usize; 3],
        initial: [usize; 3],
    },
}

/// 状態の行に出す接続の情報。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub endpoint: String,
    pub model: String,
    pub n_ctx: u32,
    /// モデルの点検で、使えない機能を止めた (06 §3.3)。
    pub limited: bool,
}

/// 生成中の応答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Streaming {
    pub reply_to: Id,
    pub text: String,
    /// モデルが思考している (本文がまだ来ていない)。
    pub thinking: bool,
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
    /// 選んでいる出典 (応答の ID と番号、FR-110)。
    pub source: Option<(Id, usize)>,
    /// 次の答え方の既定の知らせで、選ぶ窓を開く (`/style` を値なしで打った)。
    style_picker: bool,
    ctrl_c_armed: bool,
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
            source: None,
            style_picker: false,
            ctrl_c_armed: false,
        }
    }

    pub fn generating(&self) -> bool {
        self.streaming.is_some()
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
            Event::SourcesInfo { memory } => {
                let k = if memory {
                    MessageKey::UiSourcesOn
                } else {
                    MessageKey::UiSourcesOff
                };
                let m = self.catalog.key(k);
                self.notice(m);
            }
            Event::SourceDetail {
                n,
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
                let m = self.catalog.with(
                    MessageKey::UiSourceDetail,
                    &[
                        ("n", &n.to_string()),
                        ("title", &title),
                        ("at", &at),
                        ("excerpt", &excerpt),
                    ],
                );
                self.notice(m);
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
            Mode::Chat => self.on_chat_key(key),
        }
    }

    /// Tab: 出典の付いた最後の応答の、次の番号を選ぶ。
    fn next_source(&mut self) {
        let Some(t) = self
            .turns
            .iter()
            .rev()
            .find(|t| t.role == Role::Assistant && t.sources > 0)
        else {
            let m = self.catalog.key(MessageKey::UiNoSources);
            self.notice(m);
            return;
        };
        // 番号はセッションの通し番号 (05 §5.1)。この応答の分だけを巡る。
        let first = t.source_base.max(1);
        let last = first + t.sources - 1;
        let n = match self.source {
            Some((id, n)) if id == t.id && n < last => n + 1,
            _ => first,
        };
        self.source = Some((t.id, n));
        let m = self
            .catalog
            .with(MessageKey::UiSourceSelected, &[("n", &n.to_string())]);
        self.notice(m);
    }

    fn on_chat_key(&mut self, key: Key) -> Vec<Action> {
        // 出典を選んでいる間は、Enter で表示し、Esc でやめる。ほかのキーでも選ぶのをやめる。
        if let Some((turn, n)) = self.source.take() {
            match key {
                Key::Tab => {
                    self.source = Some((turn, n));
                    self.next_source();
                    return Vec::new();
                }
                Key::Enter { newline: false } if self.input.is_empty() => {
                    return vec![Action::Command(Command::ShowSource { turn, n })];
                }
                Key::Esc if !self.generating() => return Vec::new(),
                _ => {}
            }
        }
        match key {
            Key::Tab => {
                self.next_source();
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
        assert!(a.on_key(Key::Enter { newline: false }).is_empty());
        assert!(a.notices.last().unwrap().contains("/memory"));
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
        assert!(a.notices.last().unwrap().contains("出典の付いた応答"));
        let mut answer = turn(2, Role::Assistant, TurnStatus::Completed, Some(id(1)));
        answer.sources = 2;
        a.turns = vec![turn(1, Role::User, TurnStatus::Confirmed, None), answer];
        a.on_key(Key::Tab);
        a.on_key(Key::Tab);
        assert_eq!(a.source, Some((id(2), 2)));
        a.on_key(Key::Tab);
        assert_eq!(a.source, Some((id(2), 1)), "wraps around");
        assert_eq!(
            a.on_key(Key::Enter { newline: false }),
            vec![Action::Command(Command::ShowSource { turn: id(2), n: 1 })]
        );
        assert_eq!(a.source, None);
        // Esc でやめる。ほかのキーは普通に効く。
        a.on_key(Key::Tab);
        assert!(a.on_key(Key::Esc).is_empty());
        assert_eq!(a.source, None);
        a.on_key(Key::Tab);
        a.on_key(Key::Char('x'));
        assert_eq!((a.source, a.input.text()), (None, "x"));

        a.on_event(Event::SourceDetail {
            n: 1,
            title: "旅行の計画".into(),
            at: Timestamp::from_millis(0),
            lines: vec![
                (Role::User, "宿はどこ？".into()),
                (Role::Assistant, "町家です".into()),
            ],
        });
        let shown = a.notices.last().unwrap();
        assert!(shown.starts_with("[1] 旅行の計画 ("), "{shown}");
        assert!(shown.contains("あなた: 宿はどこ？"), "{shown}");
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
}
