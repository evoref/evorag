//! 描画 (07 §2.1)。描画のたびに、端末の本当のカーソルを入力欄の入力位置に置く (07 §2.3)。

use evorag_types::MessageKey;
use evorag_types::api::{
    BackgroundState, RecallState, Role, StyleItem, TurnMark, TurnMarkKind, TurnStatus, TurnView,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Mode};

const MAX_INPUT_ROWS: u16 = 6;

/// 幅 `width` で折り返す (全角の幅を数える)。
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(2);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let mut line = String::new();
        let mut col = 0;
        for g in raw.graphemes(true) {
            let w = UnicodeWidthStr::width(g);
            if col + w > width {
                out.push(std::mem::take(&mut line));
                col = 0;
            }
            line.push_str(g);
            col += w;
        }
        out.push(line);
    }
    out
}

/// 地方時の `HH:MM` (ずれが分からなければ `HH:MM UTC`、07 §2.1)。
fn hhmm(ms: i64) -> String {
    evorag_core::localtime::hhmm(ms)
}

fn header<'a>(app: &App, role: Role, time: Option<i64>, mark: Option<MessageKey>) -> Line<'a> {
    let who = match role {
        Role::User => app.catalog.key(MessageKey::UiYou),
        _ => app.catalog.key(MessageKey::UiAssistant),
    };
    let mut spans = vec![Span::styled(who, Style::new().add_modifier(Modifier::BOLD))];
    if let Some(t) = time {
        spans.push(Span::raw(format!("  {}", hhmm(t))));
    }
    if let Some(m) = mark {
        spans.push(Span::styled(
            format!("  {}", app.catalog.key(m)),
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

fn mark(app: &App, t: &TurnView) -> Option<MessageKey> {
    match t.status {
        TurnStatus::Interrupted => Some(MessageKey::UiMarkInterrupted),
        TurnStatus::Failed => Some(MessageKey::UiMarkFailed),
        _ if app.has_no_reply(t) => Some(MessageKey::UiMarkNoReply),
        _ => None,
    }
}

/// 会話の表示の行 (幅 `width` で折り返した後)。
pub fn conversation_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let body_w = width.saturating_sub(2);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for t in &app.turns {
        lines.push(header(
            app,
            t.role,
            Some(t.created_at.as_millis()),
            mark(app, t),
        ));
        // 伏せ字と `[非表示]` の印 (04 §7.3)。印の範囲が本文と合わなければ、本文を出さない。
        let hidden = app.catalog.key(MessageKey::UiMarkHidden);
        let marked = if t.unverified {
            None
        } else {
            marked_text(&t.text, &t.marks, &hidden)
        };
        match marked {
            None => lines.push(Line::styled(
                format!("  {}", app.catalog.key(MessageKey::UiMarkUnverified)),
                Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC),
            )),
            Some(Marked { text, overlay }) if t.role == Role::Assistant => {
                lines.extend(crate::markdown::render_marked(
                    &text, body_w, "  ", &overlay,
                ));
            }
            Some(Marked { text, overlay }) if !overlay.is_empty() => {
                lines.extend(crate::markdown::plain_marked(&text, body_w, "  ", &overlay));
            }
            Some(_) => {
                for l in wrap(&t.text, body_w) {
                    lines.push(Line::from(format!("  {l}")));
                }
            }
        }
        if t.role == Role::Assistant {
            band_lines(app, t, body_w, &mut lines);
        }
        lines.push(Line::from(""));
    }
    if let Some(s) = &app.streaming {
        lines.push(header(app, Role::Assistant, None, None));
        if s.thinking && s.text.is_empty() {
            lines.push(Line::styled(
                format!("  {}", app.catalog.key(MessageKey::UiThinking)),
                Style::new().add_modifier(Modifier::DIM),
            ));
        }
        lines.extend(crate::markdown::render(
            &format!("{}▍", s.text),
            body_w,
            "  ",
        ));
        lines.push(Line::from(""));
    }
    for n in app.notices.iter().flat_map(|n| n.split('\n')) {
        for l in wrap(n, width) {
            lines.push(Line::styled(l, Style::new().add_modifier(Modifier::DIM)));
        }
    }
    lines
}

/// 伏せ字の範囲の表示 (本文がすでに印なので、そのまま反転して見分ける)。
fn redacted_style() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// `[非表示]` の範囲の表示 (置き換えた印を、伏せ字と別の見え方にする)。
fn hidden_style() -> Style {
    Style::new().add_modifier(Modifier::DIM | Modifier::UNDERLINED)
}

/// 印を当てた表示の本文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marked {
    pub text: String,
    /// 表示の本文の中の、印の範囲と装飾。
    pub overlay: Vec<(std::ops::Range<usize>, Style)>,
}

/// 印を当てた表示の本文と、その中の印の範囲と装飾 (04 §7.3)。伏せ字の範囲は本文のまま、`[非表示]` の範囲は
/// `hidden` に置き換える。範囲が本文と合わない (始まりの順でない・重なる・本文の外・文字の途中) ときは `None`
/// (値を出してしまわないよう、呼び手は本文を出さない)。
pub fn marked_text(text: &str, marks: &[TurnMark], hidden: &str) -> Option<Marked> {
    let mut out = String::with_capacity(text.len());
    let mut overlay = Vec::with_capacity(marks.len());
    let mut pos = 0;
    for m in marks {
        let r = &m.range;
        if r.start < pos
            || r.start > r.end
            || !text.is_char_boundary(r.start)
            || !text.is_char_boundary(r.end)
            || r.end > text.len()
        {
            return None;
        }
        out.push_str(&text[pos..r.start]);
        let start = out.len();
        let style = match m.kind {
            TurnMarkKind::Redacted => {
                out.push_str(&text[r.clone()]);
                redacted_style()
            }
            TurnMarkKind::Hidden => {
                out.push_str(hidden);
                hidden_style()
            }
        };
        overlay.push((start..out.len(), style));
        pos = r.end;
    }
    out.push_str(&text[pos..]);
    Some(Marked { text: out, overlay })
}

/// 応答の下の候補の帯 (05 §5.2): セッション名・日時・発話の冒頭を薄く出す。選んでいるものは反転する。
fn band_lines(app: &App, t: &TurnView, width: usize, lines: &mut Vec<Line<'static>>) {
    if t.band.is_empty() {
        return;
    }
    let dim = Style::new().add_modifier(Modifier::DIM);
    let head = app.catalog.key(if t.band_nearby {
        MessageKey::UiBandNearby
    } else {
        MessageKey::UiBandRelated
    });
    lines.push(Line::styled(format!("  {head}"), dim));
    for b in &t.band {
        let at = evorag_core::localtime::datetime(b.at.as_millis());
        let mut text = app.catalog.with(
            MessageKey::UiBandItem,
            &[("title", &b.title), ("at", &at), ("head", &b.head)],
        );
        if t.irrelevant.contains(&b.target) {
            text.push_str(&app.catalog.key(MessageKey::UiBandMarked));
        }
        let picked = app.pick
            == Some(crate::app::Pick::Band {
                answer: t.id,
                target: b.target,
            });
        let style = if picked {
            dim.add_modifier(Modifier::REVERSED)
        } else {
            dim
        };
        for l in wrap(&text, width.saturating_sub(2)) {
            lines.push(Line::styled(format!("    {l}"), style));
        }
    }
}

/// `/private` の確認の画面の本文 (07 §3): 消しはしないこと、今のセッションのこれまでのやり取りには残ること、
/// llama-server には今の値を送らなくなるだけであること、多値の属性では兄弟の値が対象にならないこと、
/// `llm.allow_remote` の構成では発話の本文が外の機械に出うること。効く範囲を選べ、既定のまま Enter で実行する。
pub fn private_confirm_text(
    app: &App,
    label: &str,
    multi: bool,
    scope: evorag_types::api::PrivateScope,
) -> String {
    use evorag_types::api::PrivateScope;
    let c = &app.catalog;
    let mut text = c.with(MessageKey::UiPrivateBody, &[("label", label)]);
    if multi {
        text.push('\n');
        text.push_str(&c.key(MessageKey::UiPrivateMulti));
    }
    if app.status.remote_llm {
        text.push('\n');
        text.push_str(&c.key(MessageKey::UiPrivateRemote));
    }
    text.push('\n');
    for (s, k) in [
        (
            PrivateScope::Successors,
            MessageKey::UiPrivateScopeSuccessors,
        ),
        (PrivateScope::ValueOnly, MessageKey::UiPrivateScopeValueOnly),
    ] {
        let cursor = if s == scope { "▶" } else { " " };
        text.push_str(&format!("\n{cursor} {}", c.key(k)));
    }
    text.push_str("\n\n");
    text.push_str(&c.key(MessageKey::UiPrivateHint));
    text
}

/// `/private off` の確認の画面の本文。ほかの行や履歴の値の印も外れるときは、その件数と行を示す (04 §7.5)。
pub fn private_off_text(app: &App, label: &str, others: &[String], history: &[String]) -> String {
    let c = &app.catalog;
    let mut text = c.with(MessageKey::UiPrivateOffBody, &[("label", label)]);
    if !others.is_empty() {
        text.push_str("\n\n");
        text.push_str(&c.key(MessageKey::UiPrivateOffOthers));
        for o in others {
            text.push_str(&format!("\n  {o}"));
        }
    }
    if !history.is_empty() {
        text.push_str("\n\n");
        text.push_str(&c.with(
            MessageKey::UiPrivateOffHistory,
            &[("n", &history.len().to_string())],
        ));
        for h in history {
            text.push_str(&format!("\n  {h}"));
        }
    }
    text.push_str("\n\n");
    text.push_str(&c.key(MessageKey::UiPrivateOffHint));
    text
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(h.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [r] = Layout::horizontal([Constraint::Length(w.min(area.width))])
        .flex(Flex::Center)
        .areas(v);
    r
}

pub fn draw(app: &App, f: &mut Frame) {
    let area = f.area();
    let inner_w = area.width.saturating_sub(2) as usize;
    let layout = app.input.layout(inner_w);
    let rows = (layout.lines.len() as u16).clamp(1, MAX_INPUT_ROWS);
    let [chat, input, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(rows + 2),
        Constraint::Length(1),
    ])
    .areas(area);

    // 会話: 下に寄せ、スクロールの分だけ上へ。
    let title = match &app.session {
        Some(s) => app
            .catalog
            .with(MessageKey::UiTitleSession, &[("title", &s.title)]),
        None => app.catalog.key(MessageKey::UiTitleNoSession),
    };
    let lines = conversation_lines(app, inner_w);
    let h = chat.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(h);
    let scroll = app.scroll.min(max_scroll);
    let start = lines.len().saturating_sub(h + scroll);
    let shown: Vec<Line> = lines.into_iter().skip(start).take(h).collect();
    f.render_widget(
        Paragraph::new(shown).block(
            Block::new()
                .borders(Borders::ALL)
                .title(format!(" evorag ─ {title} ")),
        ),
        chat,
    );

    // 入力欄: カーソルが見えるようにスクロール。
    let in_scroll = (layout.row + 1).saturating_sub(rows as usize);
    let vis: Vec<Line> = layout
        .lines
        .iter()
        .skip(in_scroll)
        .take(rows as usize)
        .map(|l| Line::from(l.clone()))
        .collect();
    f.render_widget(
        Paragraph::new(vis).block(Block::new().borders(Borders::ALL)),
        input,
    );

    let state = if app.streaming.as_ref().is_some_and(|s| s.thinking) {
        app.catalog.key(MessageKey::UiStatusThinking)
    } else if app.generating() {
        app.catalog.key(MessageKey::UiStatusGenerating)
    } else {
        app.catalog.key(match app.status.background {
            Some(BackgroundState::Working) => MessageKey::UiStatusWorking,
            Some(BackgroundState::PausedBattery) => MessageKey::UiStatusBattery,
            Some(BackgroundState::PausedLowSpace) => MessageKey::UiStatusLowSpace,
            _ => MessageKey::UiStatusIdle,
        })
    };
    let st = &app.status;
    // 点検で止めた機能があれば示す (06 §3.3。理由は知らせと `evorag doctor`)。
    // 想起の欄 (07 §2.1)。較正に落ちたときは「→ evorag setup」を添える。
    let recall = st.recall.map(|r| {
        if let RecallState::Rebuilding { percent } = r {
            let text = app.catalog.with(
                MessageKey::UiRecallRebuilding,
                &[("percent", &percent.to_string())],
            );
            return format!(" {text} │");
        }
        let k = match r {
            RecallState::Rebuilding { .. } => MessageKey::UiRecallStandard,
            RecallState::Standard => MessageKey::UiRecallStandard,
            RecallState::Simple => MessageKey::UiRecallSimple,
            RecallState::CandidatesOnly {
                calibrating: true, ..
            } => MessageKey::UiRecallCalibrating,
            RecallState::CandidatesOnly { failed: true, .. } => MessageKey::UiRecallFailed,
            RecallState::CandidatesOnly { .. } => MessageKey::UiRecallCandidates,
        };
        format!(" {} │", app.catalog.key(k))
    });
    let mut limited = if st.limited {
        format!(" │ {}", app.catalog.key(MessageKey::UiStatusLimited))
    } else {
        String::new()
    };
    // 忘れるの指示と合わないターンと、ジャーナルの書き直しの残り (04 §7.2・§7.8)。
    if let Some(fs) = st.forget {
        if fs.mismatched > 0 {
            limited.push_str(" │ ");
            limited.push_str(&app.catalog.with(
                MessageKey::UiStatusForgetMismatched,
                &[("count", &fs.mismatched.to_string())],
            ));
        }
        if fs.journal_pending {
            limited.push_str(" │ ");
            limited.push_str(&app.catalog.key(MessageKey::UiStatusForgetJournal));
        }
    }
    f.render_widget(
        Paragraph::new(format!(
            " {} │ {} │{} n_ctx {} │ {state}{limited}",
            st.endpoint,
            st.model,
            recall.unwrap_or_default(),
            st.n_ctx
        )),
        status,
    );

    match &app.mode {
        Mode::Chat => {}
        Mode::Panel {
            title,
            body,
            scroll,
        } => {
            // 事実の一覧などのスクロールできる窓 (07 §3)。
            let r = centered(
                area,
                area.width.saturating_sub(4).min(100),
                area.height.saturating_sub(2),
            );
            f.render_widget(Clear, r);
            let hint = app.catalog.key(MessageKey::UiPanelHint);
            f.render_widget(
                Paragraph::new(body.clone())
                    .wrap(Wrap { trim: false })
                    .scroll((u16::try_from(*scroll).unwrap_or(u16::MAX), 0))
                    .block(
                        Block::new()
                            .borders(Borders::ALL)
                            .title(format!(" {title} "))
                            .title_bottom(format!(" {hint} ")),
                    ),
                r,
            );
        }
        Mode::ConfirmPrivate {
            label,
            multi,
            scope,
            ..
        } => {
            let r = centered(area, 78, 20);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(private_confirm_text(app, label, *multi, *scope))
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::new()
                            .borders(Borders::ALL)
                            .title(app.catalog.key(MessageKey::UiPrivateTitle)),
                    ),
                r,
            );
        }
        Mode::ConfirmPrivateOff {
            label,
            others,
            history,
            ..
        } => {
            let rows = u16::try_from(others.len() + history.len()).unwrap_or(u16::MAX);
            let r = centered(area, 78, rows.saturating_add(10).min(24));
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(private_off_text(app, label, others, history))
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::new()
                            .borders(Borders::ALL)
                            .title(app.catalog.key(MessageKey::UiPrivateOffTitle)),
                    ),
                r,
            );
        }
        Mode::ConfirmForget(fc) => {
            // `/forget` の確認の画面 (07 §3)。見本が長いのでスクロールできる。
            let r = centered(
                area,
                area.width.saturating_sub(4).min(100),
                area.height.saturating_sub(2),
            );
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(app.forget_text(fc))
                    .wrap(Wrap { trim: false })
                    .scroll((u16::try_from(fc.scroll).unwrap_or(u16::MAX), 0))
                    .block(
                        Block::new()
                            .borders(Borders::ALL)
                            .title(format!(" {} ", app.catalog.key(MessageKey::UiForgetTitle))),
                    ),
                r,
            );
        }
        Mode::ConfirmBroken { n } => {
            let r = centered(area, 72, 7);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(
                    app.catalog
                        .with(MessageKey::UiForgetBroken, &[("n", &n.to_string())]),
                )
                .wrap(Wrap { trim: false })
                .block(Block::new().borders(Borders::ALL)),
                r,
            );
        }
        Mode::Help => {
            let r = centered(area, 78, 26);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(app.catalog.key(MessageKey::UiHelpBody))
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::new()
                            .borders(Borders::ALL)
                            .title(app.catalog.key(MessageKey::UiHelpTitle)),
                    ),
                r,
            );
        }
        Mode::Detail {
            band,
            irrelevant,
            title,
            body,
            row,
            ..
        } => {
            let r = centered(area, 78, 18);
            f.render_widget(Clear, r);
            let mut text = format!("{body}\n");
            for (i, a) in crate::app::DetailAction::list(*band).iter().enumerate() {
                let label = app.catalog.key(match a {
                    crate::app::DetailAction::Attach => MessageKey::UiActionAttach,
                    crate::app::DetailAction::Irrelevant if *irrelevant => {
                        MessageKey::UiActionIrrelevantUndo
                    }
                    crate::app::DetailAction::Irrelevant => MessageKey::UiActionIrrelevant,
                });
                let cursor = if i == *row { "▶" } else { " " };
                text.push_str(&format!("\n{cursor} {label}"));
            }
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .block(Block::new().borders(Borders::ALL).title(title.clone())),
                r,
            );
        }
        Mode::ConfirmDelete => {
            let r = centered(area, 60, 6);
            f.render_widget(Clear, r);
            let title = app.session.as_ref().map_or("", |s| s.title.as_str());
            f.render_widget(
                Paragraph::new(
                    app.catalog
                        .with(MessageKey::UiDeleteConfirm, &[("title", title)]),
                )
                .wrap(Wrap { trim: false })
                .block(Block::new().borders(Borders::ALL)),
                r,
            );
        }
        Mode::Style { row, choice, .. } => {
            let r = centered(area, 72, 9);
            f.render_widget(Clear, r);
            let labels = [
                MessageKey::UiStyleLanguage,
                MessageKey::UiStyleLength,
                MessageKey::UiStyleFormat,
            ];
            let mut lines: Vec<Line> = StyleItem::ALL
                .into_iter()
                .enumerate()
                .map(|(i, item)| {
                    let values = item
                        .choices()
                        .iter()
                        .enumerate()
                        .map(|(j, v)| {
                            if j == choice[i] {
                                format!("[{v}]")
                            } else {
                                format!(" {v} ")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    let text = format!(" {}: {values}", app.catalog.key(labels[i]));
                    if i == *row {
                        Line::styled(text, Style::new().add_modifier(Modifier::REVERSED))
                    } else {
                        Line::raw(text)
                    }
                })
                .collect();
            lines.push(Line::raw(""));
            lines.push(Line::raw(app.catalog.key(MessageKey::UiStyleHint)));
            f.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    Block::new()
                        .borders(Borders::ALL)
                        .title(app.catalog.key(MessageKey::UiStyleTitle)),
                ),
                r,
            );
        }
        Mode::Sessions { list, selected } => {
            let r = centered(area, 60, (list.len() as u16 + 2).clamp(3, 20));
            f.render_widget(Clear, r);
            let block = Block::new()
                .borders(Borders::ALL)
                .title(app.catalog.key(MessageKey::UiSessionsTitle));
            if list.is_empty() {
                f.render_widget(
                    Paragraph::new(app.catalog.key(MessageKey::UiSessionsEmpty)).block(block),
                    r,
                );
            } else {
                let items: Vec<ListItem> = list
                    .iter()
                    .map(|s| ListItem::new(s.title.clone()))
                    .collect();
                let mut state = ListState::default().with_selected(Some(*selected));
                f.render_stateful_widget(
                    List::new(items)
                        .block(block)
                        .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
                    r,
                    &mut state,
                );
            }
        }
    }

    // 本当のカーソルを入力位置に置く (隠さない)。IME の候補の窓がここに出る。
    f.set_cursor_position(Position::new(
        input.x + 1 + layout.col as u16,
        input.y + 1 + (layout.row - in_scroll) as u16,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Key, Status};
    use crate::catalog::Catalog;
    use evorag_types::Id;
    use evorag_types::Timestamp;
    use evorag_types::api::{Event, SessionView};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn wrap_counts_wide_characters() {
        assert_eq!(wrap("あいうえ", 5), ["あい", "うえ"]);
        assert_eq!(wrap("a\nb", 10), ["a", "b"]);
    }

    /// verifies: AC-FR-901-1
    #[test]
    fn draws_turns_marks_status_and_places_the_cursor_at_the_input() {
        let mut app = App::new(
            Catalog::load("ja"),
            Status {
                endpoint: "127.0.0.1:8080".into(),
                model: "qwen".into(),
                n_ctx: 8192,
                limited: false,
                recall: None,
                background: None,
                remote_llm: false,
                forget: None,
            },
        );
        let sid = Id::from_bytes([1; 16]);
        app.on_event(Event::SessionOpened {
            session: SessionView {
                id: sid,
                title: "旅行".into(),
                created_at: Timestamp::from_millis(0),
                updated_at: Timestamp::from_millis(0),
            },
            turns: vec![TurnView {
                sources: 0,
                source_base: 1,
                id: Id::from_bytes([2; 16]),
                session: sid,
                role: Role::User,
                status: TurnStatus::Pending,
                text: "宿はどこ?".into(),
                model_key: None,
                reply_to: None,
                created_at: Timestamp::from_millis(36_000_000),
                band: Vec::new(),
                band_nearby: false,
                irrelevant: Vec::new(),
                marks: Vec::new(),
                unverified: false,
            }],
            unreadable: 0,
        });
        for c in "あい".chars() {
            app.on_key(Key::Char(c));
        }
        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        term.draw(|f| draw(&app, f)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        // 全角の文字の次の桁は空白として読めるので、空白を除いて比べる。
        let packed = text.replace(' ', "");
        assert!(packed.contains("旅行"), "{text}");
        assert!(
            packed.contains(&evorag_core::localtime::hhmm(36_000_000).replace(' ', "")),
            "{text}"
        );
        assert!(packed.contains("(応答なし)"), "{text}");
        assert!(packed.contains("待機中"), "{text}");
        // 入力欄は下から 4 行目 (枠の中の 1 行目)、全角 2 文字の後ろ (幅 4) にカーソル。
        let cursor = term.get_cursor_position().unwrap();
        assert_eq!((cursor.x, cursor.y), (1 + 4, 12 - 1 - 3 + 1));
    }
    /// verifies: AC-FR-412-1 (画面の層: 応答の下に帯を薄く出し、印を付けたものに印を出す)
    #[test]
    fn the_band_is_drawn_under_the_answer() {
        let mut app = App::new(Catalog::load("ja"), Status::default());
        let target = Id::from_bytes([5; 16]);
        app.turns = vec![TurnView {
            id: Id::from_bytes([2; 16]),
            session: Id::from_bytes([1; 16]),
            role: Role::Assistant,
            status: TurnStatus::Completed,
            text: "答え".into(),
            model_key: None,
            reply_to: None,
            created_at: Timestamp::from_millis(0),
            sources: 0,
            source_base: 1,
            band: vec![evorag_types::api::BandItem {
                target,
                title: "観葉植物".into(),
                at: Timestamp::from_millis(0),
                head: "モンステラの水やり".into(),
            }],
            band_nearby: false,
            irrelevant: vec![target],
            marks: Vec::new(),
            unverified: false,
        }];
        let text: Vec<String> = conversation_lines(&app, 80)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let joined = text.join("\n");
        assert!(
            joined.contains("関係しそうな記録 (この応答には使っていません)"),
            "{joined}"
        );
        assert!(joined.contains("観葉植物"), "{joined}");
        assert!(
            joined.contains("「モンステラの水やり」 [無関係]"),
            "{joined}"
        );
    }
}
