//! `/forget` の確認の画面 (07 §3、04 §7.1) と、忘れるの結果の知らせ。
//!
//! - 確認の画面は 1 つにし、既定のまま Enter で実行できる。見本はコアが作る。選択を変えるたびに求め直し、届く
//!   までは見本の欄に「数えています」と出す。見本は、今の選択と同じものだけを使う (遅れて届いた古い選択の見本を
//!   使わない)。
//! - 実行したときに見本が古くなっていたら、コアは新しい見本を返す。画面は、それに置き換えて、もう一度 Enter を
//!   待つ。
//! - 忘れた後は、新しい一覧に番号を結び付け直し、`broken/` にファイルがあれば消すかを尋ねる (04 §7.2)。

use evorag_types::MessageKey;
use evorag_types::api::{
    Command, Event, FactKey, FactRow, FactSubject, ForgetOptions, ForgetPreview, ForgetWarning,
    Role,
};

use crate::app::{Action, App, Key, Mode, fact_rows};

/// 確認の画面の選択の行 (表示の順)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Redact,
    History,
    Echo,
    Others,
}

impl Choice {
    pub const ALL: [Choice; 4] = [
        Choice::Redact,
        Choice::History,
        Choice::Echo,
        Choice::Others,
    ];

    pub fn label(self) -> MessageKey {
        match self {
            Choice::Redact => MessageKey::UiForgetChoiceRedact,
            Choice::History => MessageKey::UiForgetChoiceHistory,
            Choice::Echo => MessageKey::UiForgetChoiceEcho,
            Choice::Others => MessageKey::UiForgetChoiceOthers,
        }
    }

    pub fn is_on(self, o: &ForgetOptions) -> bool {
        match self {
            Choice::Redact => o.redact,
            Choice::History => o.history,
            Choice::Echo => o.echo,
            Choice::Others => o.others,
        }
    }

    fn toggle(self, o: &mut ForgetOptions) {
        let v = match self {
            Choice::Redact => &mut o.redact,
            Choice::History => &mut o.history,
            Choice::Echo => &mut o.echo,
            Choice::Others => &mut o.others,
        };
        *v = !*v;
    }
}

/// `/forget` の確認の画面の状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetConfirm {
    pub target: FactKey,
    /// 対象の行 (主語・属性・値)。
    pub label: String,
    pub options: ForgetOptions,
    /// 今の選択の見本。届くまでは `None` (「数えています」)。
    pub preview: Option<ForgetPreview>,
    /// 選択の欄を開いている (畳んで置くのが既定)。
    pub choices_open: bool,
    /// 選択の欄で選んでいる行 ([`Choice::ALL`] の位置)。
    pub row: usize,
    /// 約束する範囲・しない範囲を開いている。
    pub promise_open: bool,
    /// 上から飛ばす行の数。
    pub scroll: usize,
    /// 実行を送り、結果を待っている。
    pub sending: bool,
}

impl ForgetConfirm {
    pub fn new(target: FactKey, label: String) -> Self {
        Self {
            target,
            label,
            options: ForgetOptions::default(),
            preview: None,
            choices_open: false,
            row: 0,
            promise_open: false,
            scroll: 0,
            sending: false,
        }
    }

    fn preview_command(&self) -> Action {
        Action::Command(Command::PreviewForget {
            target: self.target.clone(),
            options: self.options,
        })
    }
}

impl App {
    /// `/forget <F の番号>`: 確認の画面を開き、既定の選択の見本を求める。
    pub(crate) fn open_forget(&mut self, n: usize) -> Vec<Action> {
        // どれかのセッションで生成している間は受け付けない (04 §7.1)。ほかのセッションの生成はコアが断る。
        if self.generating() {
            let m = self.catalog.key(MessageKey::ForgetBusy);
            self.notice(m);
            return Vec::new();
        }
        match self.fact_row(n) {
            Some((g, r @ FactRow::Value { .. })) => {
                let label = self.row_label(&g, &r);
                let FactRow::Value { key, .. } = r else {
                    return Vec::new();
                };
                let confirm = ForgetConfirm::new(key, label);
                let ask = confirm.preview_command();
                self.mode = Mode::ConfirmForget(Box::new(confirm));
                vec![ask]
            }
            Some(_) => {
                self.not_allowed();
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// 確認の画面のキー。
    pub(crate) fn on_forget_key(&mut self, key: Key) -> Vec<Action> {
        let Mode::ConfirmForget(mut f) = self.mode.clone() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match key {
            Key::Esc => {
                self.mode = Mode::Chat;
                return out;
            }
            Key::Tab => f.choices_open = !f.choices_open,
            Key::Char('?') => f.promise_open = !f.promise_open,
            Key::Up if f.choices_open => f.row = f.row.saturating_sub(1),
            Key::Down if f.choices_open => f.row = (f.row + 1).min(Choice::ALL.len() - 1),
            Key::Up => f.scroll = f.scroll.saturating_sub(1),
            Key::Down => f.scroll += 1,
            Key::PageUp => f.scroll = f.scroll.saturating_sub(10),
            Key::PageDown => f.scroll += 10,
            Key::Home => f.scroll = 0,
            Key::Char(' ') if f.choices_open && !f.sending => {
                // 選択を変えたら、見本を求め直す (04 §7.1)。
                Choice::ALL[f.row].toggle(&mut f.options);
                f.preview = None;
                out.push(f.preview_command());
            }
            Key::Enter { newline: false } if !f.sending => {
                match f.preview.as_ref().map(|p| &p.plan) {
                    None => {
                        let m = self.catalog.key(MessageKey::ForgetCounting);
                        self.notice(m);
                    }
                    Some(Err(_)) => {
                        let m = self.catalog.key(MessageKey::ForgetBlocked);
                        self.notice(m);
                    }
                    Some(Ok(hash)) => {
                        out.push(Action::Command(Command::ForgetFact {
                            target: f.target.clone(),
                            options: f.options,
                            plan: hash.clone(),
                        }));
                        f.sending = true;
                    }
                }
            }
            _ => {}
        }
        self.mode = Mode::ConfirmForget(f);
        out
    }

    /// `broken/` のファイルを消すかの問い。
    pub(crate) fn on_broken_key(&mut self, key: Key) -> Vec<Action> {
        self.mode = Mode::Chat;
        match key {
            Key::Char('y' | 'Y') => vec![Action::Command(Command::DiscardBroken)],
            _ => Vec::new(),
        }
    }

    /// 忘れるの見本・結果と、伏せたターンのイベント。扱ったら `true`。
    pub(crate) fn on_forget_event(&mut self, ev: &Event) -> bool {
        match ev {
            Event::ForgetPreviewInfo { preview } => {
                if let Mode::ConfirmForget(f) = &mut self.mode
                    && f.target == preview.target
                    && f.options == preview.options
                {
                    f.preview = Some(preview.clone());
                }
            }
            Event::ForgetRejected {
                error,
                preview,
                list,
            } => {
                let m = self.catalog.message(error);
                self.notice(m.clone());
                // 見本が古くなっていた・伏せ字にできなくなった: 新しい見本に置き換えて、もう一度 Enter を待つ。
                if let (Some(p), Mode::ConfirmForget(f)) = (preview, &mut self.mode)
                    && f.target == p.target
                {
                    f.options = p.options;
                    f.preview = Some(p.clone());
                    f.sending = false;
                    return true;
                }
                if matches!(self.mode, Mode::ConfirmForget(_)) {
                    self.mode = Mode::Chat;
                }
                if let Some(list) = list {
                    // 「一覧が更新されました」と出し、新しい一覧を出す (04 §7.1)。
                    self.facts.history = None;
                    self.show_facts(list.clone(), Some(m));
                }
            }
            Event::FactForgotten {
                list,
                redacted,
                journal_pending,
                broken,
            } => {
                let kept = match &self.mode {
                    Mode::ConfirmForget(f) => !f.options.redact,
                    _ => false,
                };
                self.mode = Mode::Chat;
                let done = if kept {
                    self.catalog.key(MessageKey::UiForgetDoneKept)
                } else {
                    self.catalog
                        .with(MessageKey::UiForgetDone, &[("n", &redacted.to_string())])
                };
                self.notice(done.clone());
                let slow = self.catalog.key(MessageKey::UiFactsSlowNext);
                self.notice(slow);
                if *journal_pending {
                    let m = self.catalog.key(MessageKey::CliForgetJournalPending);
                    self.notice(m);
                }
                // 番号が変わるので、新しい一覧を出し、番号をそれに結び付ける。履歴と忘れた事実の番号は使えなくする。
                self.facts.history = None;
                self.facts.forgotten = None;
                self.show_facts(list.clone(), Some(done));
                if *broken > 0 {
                    self.mode = Mode::ConfirmBroken { n: *broken };
                }
            }
            Event::TurnsRedacted { session, turns } => {
                if self.session.as_ref().is_some_and(|s| s.id == *session) {
                    for new in turns {
                        // 本文と印だけを置き換える (帯と「無関係」の印は前のまま)。
                        if let Some(t) = self.turns.iter_mut().find(|t| t.id == new.id) {
                            t.text.clone_from(&new.text);
                            t.marks.clone_from(&new.marks);
                            t.unverified = new.unverified;
                        }
                    }
                }
            }
            Event::ForgetState { state } => {
                self.status.forget = Some(*state);
            }
            Event::BrokenDiscarded { removed, failed } => {
                if *removed > 0 {
                    let m = self.catalog.with(
                        MessageKey::UiForgetBrokenRemoved,
                        &[("n", &removed.to_string())],
                    );
                    self.notice(m);
                }
                if *failed > 0 {
                    let m = self.catalog.with(
                        MessageKey::UiForgetBrokenFailed,
                        &[("n", &failed.to_string())],
                    );
                    self.notice(m);
                }
            }
            _ => return false,
        }
        true
    }

    /// 影響するほかの事実の行: 最後に表示した一覧にあれば、その番号を添える。
    fn affected_label(&self, key: &FactKey) -> String {
        let numbered = self.facts.list.as_ref().and_then(|list| {
            fact_rows(list)
                .into_iter()
                .enumerate()
                .find(|(_, (_, r))| matches!(r, FactRow::Value { key: k, .. } if k == key))
                .map(|(i, (g, r))| format!("F{} {}", i + 1, self.row_label(g, r)))
        });
        numbered.unwrap_or_else(|| {
            let subject = match &key.subject {
                FactSubject::User => self.catalog.key(MessageKey::UiFactsSelf),
                FactSubject::Named(n) => n.clone(),
            };
            format!("{subject}・{}", self.catalog.attribute(&key.attribute))
        })
    }

    /// 確認の画面の本文 (07 §3)。
    pub fn forget_text(&self, f: &ForgetConfirm) -> String {
        let c = &self.catalog;
        let mut lines = vec![c.with(MessageKey::UiForgetTarget, &[("label", &f.label)])];
        lines.push(String::new());
        match &f.preview {
            None => lines.push(c.key(MessageKey::UiForgetCounting)),
            Some(p) => {
                match &p.plan {
                    Err(w) => {
                        let reason = c.key(match w {
                            ForgetWarning::UnreadableTurn => MessageKey::UiForgetWarnUnreadable,
                            ForgetWarning::StaleSource => MessageKey::UiForgetWarnStale,
                            ForgetWarning::ValueMismatch => MessageKey::UiForgetWarnValue,
                            ForgetWarning::MismatchedTurn => MessageKey::UiForgetWarnMismatched,
                        });
                        lines.push(c.with(MessageKey::UiForgetWarning, &[("reason", &reason)]));
                    }
                    Ok(_) => {
                        lines.push(c.key(if p.options.redact {
                            MessageKey::UiForgetSamplesRedact
                        } else {
                            MessageKey::UiForgetSamplesHide
                        }));
                        if p.samples.is_empty() {
                            lines.push(c.key(MessageKey::UiForgetNoSamples));
                        }
                        for s in &p.samples {
                            let who = c.key(match s.role {
                                Role::User => MessageKey::UiYou,
                                _ => MessageKey::UiAssistant,
                            });
                            lines.push(c.with(
                                MessageKey::UiForgetSampleBefore,
                                &[("who", &who), ("text", &s.before)],
                            ));
                            lines.push(
                                c.with(MessageKey::UiForgetSampleAfter, &[("text", &s.after)]),
                            );
                        }
                        if !p.options.redact {
                            lines.push(c.key(MessageKey::UiForgetKeptNote));
                        }
                    }
                }
                let counts = [
                    (p.history, MessageKey::UiForgetHistory),
                    (p.skipped_unreadable, MessageKey::UiForgetSkipped),
                    (p.unredacted_sources, MessageKey::UiForgetUnredacted),
                    (p.source_outside, MessageKey::UiForgetSourceOutside),
                    (p.whole_turns, MessageKey::UiForgetWhole),
                    (p.mismatched_turns, MessageKey::UiForgetMismatched),
                ];
                if p.other_turns > 0 {
                    lines.push(c.with(
                        MessageKey::UiForgetOthers,
                        &[
                            ("turns", &p.other_turns.to_string()),
                            ("matches", &p.other_matches.to_string()),
                        ],
                    ));
                }
                for (n, k) in counts {
                    if n > 0 {
                        lines.push(c.with(k, &[("n", &n.to_string())]));
                    }
                }
                if !p.affected.is_empty() {
                    lines.push(c.key(MessageKey::UiForgetAffected));
                    for a in &p.affected {
                        lines.push(format!("  {}", self.affected_label(a)));
                    }
                }
            }
        }
        lines.push(String::new());
        if f.choices_open {
            for (i, ch) in Choice::ALL.into_iter().enumerate() {
                let cursor = if i == f.row { "▶" } else { " " };
                let on = if ch.is_on(&f.options) { "[x]" } else { "[ ]" };
                lines.push(format!("{cursor} {on} {}", c.key(ch.label())));
            }
        } else {
            let summary: Vec<String> = Choice::ALL
                .into_iter()
                .filter(|ch| ch.is_on(&f.options))
                .map(|ch| c.key(ch.label()))
                .collect();
            let summary = if summary.is_empty() {
                "-".to_owned()
            } else {
                summary.join("・")
            };
            lines.push(c.with(MessageKey::UiForgetChoicesFolded, &[("summary", &summary)]));
        }
        lines.push(c.key(if f.promise_open {
            MessageKey::UiForgetPromise
        } else {
            MessageKey::UiForgetPromiseFolded
        }));
        lines.push(String::new());
        lines.push(c.key(if f.sending {
            MessageKey::UiForgetSending
        } else if f.choices_open {
            MessageKey::UiForgetHintOpen
        } else {
            MessageKey::UiForgetHint
        }));
        lines.join("\n")
    }
}
