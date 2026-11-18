//! インストール先の実行ファイルの置き換え・巻き戻し・続き・片付け (12 §7.2・§7.3)。
//!
//! 通信しない。データ根のロックは呼ぶ側が取る ([`Installation`] の呼ぶ順を参照)。

mod error;
mod installation;
mod names;
mod probe;
mod state;
#[cfg(test)]
mod tests;

pub use error::{CheckFailure, Failure, FileOp, OwnershipProblem, SwapError};
pub use installation::{
    ApplyReport, Build, CleanupReport, Env, Event, Installation, PlanSummary, PrevInfo, PrevSource,
    Purpose, ResumeOutcome, ResumeReport, Startup, Strictness, startup,
};
pub use names::{EXE_NAME, FileName, GapRecovery, Op};
pub use probe::{PROBE_MAX_OUTPUT, PROBE_TIMEOUT, ProbeError, ProcessProbe, VersionProbe};
