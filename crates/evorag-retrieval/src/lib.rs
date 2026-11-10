//! 問いの準備、候補の収集と順位付け、文脈の組み立て。
//!
//! 仕様: 05, 02 §4.1。v0.1 は語彙の索引による想起 ([`recall`]) と、その正規化 ([`normalize`])。

pub mod normalize;
pub mod recall;

use evorag_index::lex::DocInput;
use evorag_types::Id;

/// 索引に入れる 1 件を作る (正規化と単位はこの crate の規則)。
pub fn doc_input(id: Id, session: Id, text: &str) -> DocInput {
    DocInput {
        id,
        session,
        terms: normalize::units(text).into_iter().map(|u| u.term).collect(),
    }
}
