//! 描画 (07 §2.1)。描画のたびに、端末の本当のカーソルを入力欄の入力位置に置く (07 §2.3)。

use evorag_types::MessageKey;
use evorag_types::api::{Role, TurnStatus, TurnView};
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
        if t.role == Role::Assistant {
            lines.extend(crate::markdown::render(&t.text, body_w, "  "));
        } else {
            for l in wrap(&t.text, body_w) {
                lines.push(Line::from(format!("  {l}")));
            }
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
    for n in &app.notices {
        for l in wrap(n, width) {
            lines.push(Line::styled(l, Style::new().add_modifier(Modifier::DIM)));
        }
    }
    lines
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
        app.catalog.key(MessageKey::UiStatusIdle)
    };
    let st = &app.status;
    f.render_widget(
        Paragraph::new(format!(
            " {} │ {} │ n_ctx {} │ {state}",
            st.endpoint, st.model, st.n_ctx
        )),
        status,
    );

    match &app.mode {
        Mode::Chat => {}
        Mode::Help => {
            let r = centered(area, 78, 16);
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

    #[test]
    fn draws_turns_marks_status_and_places_the_cursor_at_the_input() {
        let mut app = App::new(
            Catalog::load("ja"),
            Status {
                endpoint: "127.0.0.1:8080".into(),
                model: "qwen".into(),
                n_ctx: 8192,
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
                id: Id::from_bytes([2; 16]),
                session: sid,
                role: Role::User,
                status: TurnStatus::Pending,
                text: "宿はどこ?".into(),
                model_key: None,
                reply_to: None,
                created_at: Timestamp::from_millis(36_000_000),
            }],
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
}
