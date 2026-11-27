//! 区切りごとの 1 回の呼び出し (B2、04 §5.2・§5.2.1) の、I/O と LLM を使わない部分。
//!
//! - 要求の組み立て ([`Layout`]): 区切りの往復に前のふるいと文の分け方 v1 を当てて文番号を振り、応答を
//!   `[参照のみ]` の枠に入れ、訂正の候補を末尾に並べる。予算に収まらなければ往復の境で呼び出しに分ける。台帳に書く
//!   `sentences` と `inputs` (`input_hash`・規則の版・`through`・`corr`・`refs`) も作る。
//! - 出力の読み取り ([`read_output`]): 節ごとに型を確かめ、台帳の `output` と `status` を決める。止められた出力から
//!   完結した要素を採る。
//!
//! 呼び出しそのもの (HTTP・ストリーム・止め方・送り直し) は呼び手 (`evorag-core`) が行う。旧エンジンのモジュール
//! (`facts`・`fold`・`correction`・`note`・`extract`・`table`・`forget`・`mask`・`view`・`gates`) に依存しない。

mod assemble;
mod output;
mod prompt;
mod request;

pub use assemble::{AssembleError, LedgerMeta, Outcome, ledger_body};
pub use output::{InvalidReason, Parsed, Reading, SectionTally, StreamEnd, Tally, read_output};
pub use request::{
    BUDGET_MARGIN, Call, CallerInputs, Candidate, CountMismatch, Counting, Input, InputError,
    Layout, Pair, Plan, Request, Text, fits, user_budget,
};

/// プロンプトの変種 (台帳の `variant`、04 §5.2.1)。指示・辞書の説明・「プロンプトに必ず書くこと」・出力の型・
/// 利用者のメッセージの形のどれかを変えたら上げる (試験 `the_variant_pins_the_prompt` が落ちる)。S-17 の 4 で測った
/// SR の形 (文番号で指し、応答を `[参照のみ]` で入れる) のシステムのメッセージの文言に、`corrections` の節と仕様の
/// 必須の文言を足したもの (日本語は 2026-11-28 に検証用のモデルで測った。英語は未計測。`spikes/S-17/REPORT.md`
/// §5.6)。`sr1` (文言を縮めた形) は正しさが下がったので使わない。
pub const VARIANT: &str = "sr2";
/// B2 の要求のプロンプトの上限 (トークン、04 §4)。S-17 の 4 で 3,072 に決め、システムのメッセージを `sr2` にした
/// ときに 3,584 に上げた (Qwen3-4B で約 1,400 トークン。8 往復の区切りを 1 回の呼び出しに保つため)。
pub const REQUEST_TOKENS: usize = 3_584;
/// B2 の生成の上限 (`max_tokens`、04 §5.2.1。S-17 の 4 で決めた)。
pub const MAX_TOKENS: u32 = 2_048;
/// `keys` の数の上限 (04 §5.2 の `maxItems`)。
pub const KEYS_MAX: usize = 8;
/// 検索の鍵 1 つの長さの上限 (文字、04 §5.2)。
pub const KEY_MAX_CHARS: usize = 40;
/// `units` の数の上限 (04 §5.2 の `maxItems`。S-17 の 4 で決めた)。
pub const UNITS_MAX: usize = 24;
/// 属性 (`a`) の長さの上限 (文字、04 §5.2)。文法では縛らず、超える属性は関門で捨てる。
pub const ASPECT_MAX_CHARS: usize = 40;
/// `[参照のみ]` の枠に入れる応答の頭の長さ (文字、04 §5.2)。
pub const REPLY_CHARS: usize = 500;
/// 区切りの全体が予算に入らないときに縮めた応答の枠の長さ (文字、04 §5.2.1 の「予算での分け方」)。
pub const REPLY_SHORT_CHARS: usize = 200;
/// 訂正の候補に添える直前の応答の末尾の長さ (文字、04 §6)。予算に入らなければ半分にする。
pub const PREVIOUS_CHARS: usize = 800;

#[cfg(test)]
mod tests;
