//! 指示の当て方の純粋な関数 (04 §7、§5.2 の関門 8、§5.5、§6 の `forgotten`。08 §2.6 の M5.2)。
//!
//! v0.5 の指示 (`directive`、04 §7.5) を、候補・台帳・単位にどう当てるかを決める。I/O・LLM・書き込みを持たず、
//! 同じ入力から同じ結果を返す。導出 (`derive`)・忘れるの計画・当て直し・`doctor --deep` は、この関数を共有する
//! (04 §7.5 の「当て方の述語」は 1 つの関数にする)。
//!
//! - `set`: 指示の集合 ([`DirectiveSet`]) と、指示ごとの当て方。忘れるの当て方の述語 (関門 8、§7.5・§7.6)、
//!   訂正の `forgotten` (§6)、private (後継・根拠の位置)・採る・戻す・`end` の当たり、解く指示 (`private_off`・
//!   `unblock`) の解き方、採る・戻す・`end` を畳みの列に混ぜる位置 ([`Slot`])。
//! - `marks`: 忘れるの伏せ字と `[非表示]` の範囲をターンごとにまとめ ([`TurnRedactions`])、位置の写し直し
//!   (§7.3) と画面への印の範囲を求める。伏せ字にしない忘れるの `[非表示]` の範囲を本文から探す
//!   ([`locate_forgotten`]・[`hidden_in_evidence`])。
//! - `scrub`: 台帳の掃除 (§7.2、[`scrub_ledger`]) と、掃除できない台帳の生の走査 ([`scan_raw_ledger`])。
//! - `keys`: 語句・部分文字列の値の鍵 ([`SubstringKeys`]。§7.2 の掃除の 2・4、§5.5 の private の鍵のふるい)。
//! - `strings`: 保存した値の中の文字列を集める。
//!
//! **依存の規則**: このモジュールは、旧エンジンのモジュール (`facts`・`fold`・`correction`・`note`・`extract`・
//! `table`・`forget`・`mask`・`view`・`gates` など) に依存しない (旧エンジンは v0.5 の判定の前に外す、08 §2.6)。
//! 依存してよいのは、`directive`・`ledger`・`value_key`・`chars` と保存の層 (`evorag_store`) だけ。旧エンジンの
//! 純粋な部分を使うときは、このモジュールに写す (写した元を注に書く)。
//!
//! 旧エンジンの型と取り違えないように、`Forget`・`End`・`Private`・`Slot`・`Phase`・`Timed` はクレートの最上位に
//! 出さず、`apply::…` で呼ぶ。

mod keys;
mod marks;
mod scrub;
mod set;
mod strings;

#[cfg(test)]
mod cases;

pub use keys::{SUBSTRING_MAX_CHARS, SubstringKeys, locate_keys};
pub use marks::{Located, TurnRedactions, hidden_in_evidence, locate_forgotten};
pub use scrub::{
    RawLedger, RevText, ScrubContext, ScrubError, Scrubbed, TurnTexts, scan_raw_ledger,
    scrub_ledger,
};
pub use set::{
    AdoptHit, AdoptOp, Adoption, DirectiveAspect, DirectiveCounts, DirectiveSet, End, Forget,
    Phase, Private, PrivateHit, Probe, ProbeAspect, ResolvedEntity, Slot, Timed, UnitProbe,
    ValueKeys,
};
