//! 端末のループ (07 §2)。S-03 の作りを引き継ぐ。
//!
//! - raw mode・代替画面・bracketed paste を有効にし、終わるとき (パニックを含む) に元に戻す。
//! - IME の確定の文字列は `Char` の連続か `Paste` で来るので、たまった分をまとめて処理してから描画する。
//! - キーを離したときのイベント (Windows の crossterm は届ける) は入力として扱わない。

use std::io;
use std::time::Duration;

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
use crate::render;

fn key_of(ev: TermEvent) -> Option<Key> {
    match ev {
        TermEvent::Paste(s) => Some(Key::Paste(s)),
        TermEvent::Key(k) if k.kind != KeyEventKind::Release => {
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            Some(match k.code {
                KeyCode::Char('c') if ctrl => Key::CtrlC,
                KeyCode::Char('q') if ctrl => Key::CtrlQ,
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
                _ => return None,
            })
        }
        _ => None,
    }
}

/// 画面を開いて、終了の操作まで動かす。終わったら端末を元に戻して戻る (コアの終了は呼び出し側)。
pub fn run(core: &Core, mut events: UnboundedReceiver<Event>, mut app: App) -> io::Result<()> {
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
    let r = event_loop(&mut term, core, &mut events, &mut app);
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    r
}

fn event_loop(
    term: &mut DefaultTerminal,
    core: &Core,
    events: &mut UnboundedReceiver<Event>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        while let Ok(ev) = events.try_recv() {
            app.on_event(ev);
        }
        term.draw(|f| render::draw(app, f))?;
        // トークンの受信から描画までを短く保つ (NFR-104 の 50 ms)。
        if event::poll(Duration::from_millis(16))? {
            let mut actions = Vec::new();
            if let Some(k) = key_of(event::read()?) {
                actions.extend(app.on_key(k));
            }
            while event::poll(Duration::ZERO)? {
                if let Some(k) = key_of(event::read()?) {
                    actions.extend(app.on_key(k));
                }
            }
            for a in actions {
                match a {
                    Action::Command(c) => core.command(c),
                    Action::ListSessions => app.show_sessions(core.sessions(100)),
                    Action::Quit => return Ok(()),
                }
            }
        }
    }
}
