//! TUI (描画と入力)。`evorag-types` と `evorag-core` の公開 API だけを使う (02 §4.3 の R1)。
//!
//! 仕様: 07。画面はコアの API の命令を出し、イベントを描画するだけにする (07 §1)。

pub mod app;
pub mod catalog;
pub mod input;
pub mod render;
pub mod run;
pub mod slash;
