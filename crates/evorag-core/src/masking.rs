//! `[非表示]` の置き換えを、添える往復・リランクに渡す往復・帯と `/memory` と出典の詳細に出す本文・訂正の検証の
//! プロンプトに入れる直前の応答に当てる (04 §7.1・§7.5、05 §3.4)。保存した本文は変えない。
//!
//! 伏せる範囲は往復ごとに決まる ([`evorag_memory::Masker`])。事実の出典は利用者の発話なので、応答だけを渡された
//! ときは、その往復の発話 (の ID と改訂の番号) を DB から足して判定する。

use std::collections::BTreeMap;

use evorag_memory::{KIND_TURN, Masker, Turn, TurnRole, TurnText, hidden_marker, turn_rev};
use evorag_store::{Read, ReadTxn};
use evorag_types::Id;

use crate::prompt::PromptLanguage;

/// 伏せる材料と印。
pub(crate) struct Hide {
    masker: Masker,
    marker: &'static str,
}

/// 往復の鍵 (利用者の発話の ID。応答は、その発話)。
fn exchange_key(t: &Turn) -> Id {
    match t.body.role {
        TurnRole::User => t.envelope.id,
        _ => t.body.reply_to.unwrap_or(t.envelope.id),
    }
}

impl Hide {
    /// 記録から読む。読めなければ、何も伏せない材料にする (読めない事実は、表と想起でも使えない)。
    pub(crate) fn load(r: &ReadTxn<'_>, lang: PromptLanguage) -> Self {
        let masker = Masker::load(r).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "facts could not be read for hiding private values");
            Masker::default()
        });
        Self {
            masker,
            marker: hidden_marker(lang.as_str()),
        }
    }

    /// `turns` の本文に置き換えを当てる (往復ごとに)。
    pub(crate) fn apply(&self, r: &ReadTxn<'_>, turns: &mut [Turn]) {
        if self.masker.is_empty() {
            return;
        }
        let mut groups: BTreeMap<Id, Vec<usize>> = BTreeMap::new();
        for (i, t) in turns.iter().enumerate() {
            groups.entry(exchange_key(t)).or_default().push(i);
        }
        for (key, idx) in groups {
            let masked = {
                let mut texts: Vec<TurnText<'_>> = idx
                    .iter()
                    .map(|&i| TurnText {
                        id: turns[i].envelope.id,
                        rev: turn_rev(&turns[i].envelope),
                        text: &turns[i].body.text,
                    })
                    .collect();
                if !texts.iter().any(|t| t.id == key)
                    && let Ok(Some(Read::Ok(e))) = r.get(KIND_TURN, key)
                {
                    texts.push(TurnText {
                        id: key,
                        rev: turn_rev(&e),
                        text: "",
                    });
                }
                self.masker.mask(&texts, self.marker)
            };
            for (j, &i) in idx.iter().enumerate() {
                turns[i].body.text.clone_from(&masked[j]);
            }
        }
    }

    /// 1 つのターンの本文に置き換えを当てたもの。
    pub(crate) fn text(&self, r: &ReadTxn<'_>, turn: &Turn) -> String {
        let mut one = [turn.clone()];
        self.apply(r, &mut one);
        let [t] = one;
        t.body.text
    }
}
