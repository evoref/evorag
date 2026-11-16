//! 端末のループ (07 §2)。S-03 の作りを引き継ぐ。
//!
//! - raw mode・代替画面・bracketed paste を有効にし、終わるとき (パニックを含む) に元に戻す。
//! - IME の確定の文字列は `Char` の連続か `Paste` で来るので、たまった分をまとめて処理してから描画する。
//! - キーを離したときのイベント (Windows の crossterm は届ける) は入力として扱わない。

use std::io;
use std::time::{Duration, Instant};

use evorag_core::Core;
use evorag_types::api::{Command, Event};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyCode, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::{Action, App, Key};
use crate::draft::Draft;
use crate::render;
use evorag_types::MessageKey;

fn key_of(ev: TermEvent) -> Option<Key> {
    match ev {
        TermEvent::Paste(s) => Some(Key::Paste(s)),
        TermEvent::Key(k) if k.kind != KeyEventKind::Release => {
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            Some(match k.code {
                KeyCode::Char('c') if ctrl => Key::CtrlC,
                KeyCode::Char('q') if ctrl => Key::CtrlQ,
                KeyCode::Char('e') if ctrl => Key::CtrlE,
                KeyCode::Char(_) if ctrl => return None,
                KeyCode::Char(c) => Key::Char(c),
                KeyCode::Enter => Key::Enter {
                    // Windows Terminal は Shift+Enter を Enter と区別しないことがあるので、Alt+Enter も改行。
                    newline: k
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT),
                },
                KeyCode::Backspace => Key::Backspace,
                KeyCode::Delete => Key::Delete,
                KeyCode::Left => Key::Left,
                KeyCode::Right => Key::Right,
                KeyCode::Up => Key::Up,
                KeyCode::Down => Key::Down,
                KeyCode::Home => Key::Home,
                KeyCode::End => Key::End,
                KeyCode::PageUp => Key::PageUp,
                KeyCode::PageDown => Key::PageDown,
                KeyCode::Esc => Key::Esc,
                KeyCode::Tab => Key::Tab,
                _ => return None,
            })
        }
        _ => None,
    }
}

/// 画面を開いて、終了の操作まで動かす。終わったら端末を元に戻して戻る (コアの終了は呼び出し側)。
pub fn run(
    core: &Core,
    mut events: UnboundedReceiver<Event>,
    mut app: App,
    state_dir: Option<&std::path::Path>,
) -> io::Result<()> {
    // 前の起動で残った下書きを入力欄に戻す (04 §3)。
    let mut draft = state_dir.map(Draft::new);
    if let Some(text) = draft.as_mut().and_then(Draft::load) {
        app.input.set(&text);
        let m = app.catalog.key(MessageKey::UiDraftRestored);
        app.notices.push(m);
    }
    let mut term = ratatui::init();
    execute!(io::stdout(), EnableBracketedPaste)?;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        prev(info); // ratatui の hook: 端末を元に戻してから表示する
    }));
    // 最後に使ったセッションを開く。
    if let Some(s) = core.sessions(1).first() {
        core.command(Command::OpenSession { id: s.id });
    }
    let r = event_loop(&mut term, core, &mut events, &mut app, draft.as_mut());
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    r
}

/// 外部のエディタを開く間だけ、端末を元に戻す。
fn edit_outside(term: &mut DefaultTerminal, app: &mut App, draft: &mut Draft) {
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    let result = draft.edit(app.input.text());
    *term = ratatui::init();
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    let _ = term.clear();
    match result {
        Ok(text) => app.input.set(&text),
        Err(e) => {
            let m = app
                .catalog
                .with(MessageKey::UiEditorFailed, &[("detail", &e.to_string())]);
            app.notices.push(m);
        }
    }
}

fn event_loop(
    term: &mut DefaultTerminal,
    core: &Core,
    events: &mut UnboundedReceiver<Event>,
    app: &mut App,
    mut draft: Option<&mut Draft>,
) -> io::Result<()> {
    let mut last_save = Instant::now();
    let mut idle = crate::app::IdleWatch::new(Instant::now());
    loop {
        // 下書きを 5 秒ごとに保存する (04 §3)。
        if last_save.elapsed() >= Duration::from_secs(5) {
            if let Some(d) = draft.as_deref_mut() {
                let _ = d.save(app.input.text());
            }
            last_save = Instant::now();
        }
        while let Ok(ev) = events.try_recv() {
            app.on_event(ev);
        }
        // 入力が止まったら、想起を先に済ませてもらう (05 §4.5)。
        if idle.due(app.input.text(), Instant::now())
            && let Some(text) = app.idle_text()
        {
            core.command(Command::InputIdle { text });
            idle.sent();
        }
        term.draw(|f| render::draw(app, f))?;
        // トークンの受信から描画までを短く保つ (NFR-104 の 50 ms)。
        if event::poll(Duration::from_millis(16))? {
            let mut actions = Vec::new();
            let mut touched = false;
            if let Some(k) = key_of(event::read()?) {
                touched = true;
                actions.extend(app.on_key(k));
            }
            while event::poll(Duration::ZERO)? {
                if let Some(k) = key_of(event::read()?) {
                    touched = true;
                    actions.extend(app.on_key(k));
                }
            }
            // キー入力は裏の処理を止める (04 §4、FR-301)。
            if touched {
                core.touch();
            }
            for a in actions {
                match a {
                    Action::Command(c) => {
                        // 送ったら下書きを消す。
                        if matches!(c, Command::Send { .. })
                            && let Some(d) = draft.as_deref_mut()
                        {
                            d.clear();
                        }
                        core.command(c);
                    }
                    Action::OpenEditor => match draft.as_deref_mut() {
                        Some(d) => {
                            // 外部のエディタで書いている間は、裏の処理を出さない (06 §4)。
                            core.hold(true);
                            edit_outside(term, app, d);
                            core.hold(false);
                        }
                        None => {
                            let m = app
                                .catalog
                                .with(MessageKey::UiEditorFailed, &[("detail", "no state folder")]);
                            app.notices.push(m);
                        }
                    },
                    Action::ListSessions => app.show_sessions(core.sessions(100)),
                    Action::Quit => return Ok(()),
                }
            }
        }
    }
}
