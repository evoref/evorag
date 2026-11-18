//! 保存の形 (evorag-memory) から、コアの API の読み取り用の型 (evorag-types) へ写す。

use evorag_memory::{Session, Turn, TurnBody, TurnRole, TurnState};
use evorag_store::Envelope;
use evorag_store::forget::MarkKind;
use evorag_types::api::{Role, SessionView, TurnMark, TurnMarkKind, TurnStatus, TurnView};

pub fn session(s: &Session) -> SessionView {
    SessionView {
        id: s.envelope.id,
        title: s.body.title.clone(),
        created_at: s.envelope.created_at,
        updated_at: s.envelope.updated_at,
    }
}

pub fn role(r: TurnRole) -> Role {
    match r {
        TurnRole::User => Role::User,
        TurnRole::Assistant => Role::Assistant,
        TurnRole::System => Role::System,
    }
}

pub fn turn(t: &Turn) -> TurnView {
    turn_of(&t.envelope, &t.body)
}

pub fn turn_of(e: &Envelope, b: &TurnBody) -> TurnView {
    TurnView {
        id: e.id,
        session: b.session,
        role: role(b.role),
        status: match b.state {
            TurnState::Pending => TurnStatus::Pending,
            TurnState::Confirmed => TurnStatus::Confirmed,
            TurnState::Completed => TurnStatus::Completed,
            TurnState::Interrupted => TurnStatus::Interrupted,
            TurnState::Failed => TurnStatus::Failed,
        },
        text: b.text.clone(),
        model_key: b.model_key.clone(),
        reply_to: b.reply_to,
        created_at: e.created_at,
        sources: b.recall.as_ref().map_or(0, |r| r.sources.len()),
        source_base: crate::recall::source_base(b) as usize,
        // 帯と印は、記録を読める呼び手が足す (`decorate`)。
        band: Vec::new(),
        band_nearby: false,
        irrelevant: Vec::new(),
        // 印と「確かめられない」は、忘れるの指示を読める呼び手が足す (`display`)。
        marks: Vec::new(),
        unverified: false,
    }
}

/// 画面に出すターン: 忘れるの指示の印の範囲を添え、指示と合わないターン (04 §7.2) は本文を出さずに「確かめ
/// られない」とする。伏せる材料を読めなければ (`hide` が `None`)、指示の有無を確かめられないので本文を出さない。
pub(crate) fn display(hide: Option<&crate::masking::Hide>, t: &Turn) -> TurnView {
    let mut v = turn(t);
    match hide.and_then(|h| h.marks(t)) {
        Some(marks) => {
            v.marks = marks
                .into_iter()
                .map(|m| TurnMark {
                    range: m.range,
                    kind: match m.kind {
                        MarkKind::Redacted => TurnMarkKind::Redacted,
                        MarkKind::Hidden => TurnMarkKind::Hidden,
                    },
                })
                .collect();
        }
        None => {
            v.text.clear();
            v.unverified = true;
        }
    }
    v
}
