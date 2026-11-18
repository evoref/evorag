//! 開き方・片付けの厳しさ・版の情報と、操作の結果 (12 §7.2・§7.3)。

use crate::{Sha256Digest, Version};

use super::super::error::CheckFailure;
use super::super::names::{FileName, Op};

/// 何のために開くか。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Purpose {
    /// `evorag update apply`・`rollback`: 動いている実行ファイルの名前が `evorag(.exe)` でなければ断る。
    Update,
    /// TUI の起動の片付け: 名前を問わない。
    Startup,
}

/// 名前で片付けて、消せない・消さないファイルが残ったときの扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Strictness {
    /// 断る (`apply`・`rollback` の始める前の確かめの 5)。
    Strict,
    /// 残して報告する (起動の片付け。次の機会に回す)。
    Lenient,
}

/// 置き換える版の情報 (状態のファイルに、示すためだけに書く)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Build {
    /// 版。
    pub version: Version,
    /// その版が書く DB の形式の版 (分かれば)。
    pub db_format: Option<String>,
}

impl Build {
    /// 版の情報。
    #[must_use]
    pub const fn new(version: Version, db_format: Option<String>) -> Self {
        Self { version, db_format }
    }
}

/// 残った計画の要約 (示すため)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PlanSummary {
    /// 操作。
    pub op: Op,
    /// 済んだ段。
    pub stage: u32,
    /// 元に戻している途中か。
    pub reverting: bool,
}

/// `evorag.prev` の版 (12 §7.2 の rollback の 2)。[`super::Installation::identify_prev`] だけが作る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrevInfo {
    pub(super) sha256: Sha256Digest,
    pub(super) version: Version,
    pub(super) db_format: Option<String>,
    pub(super) source: PrevSource,
}

/// `evorag.prev` の版をどこから知ったか。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrevSource {
    /// 状態のファイルの最後に済んだ操作の記録。
    Record,
    /// `evorag.prev --version` の出力 (DB の形式の版は `db format:` の行か、過去の版の表)。
    Probe,
}

impl PrevInfo {
    /// SHA-256。
    #[must_use]
    pub const fn sha256(&self) -> Sha256Digest {
        self.sha256
    }

    /// 版。
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// DB の形式の版 (分からなければ `None`)。
    #[must_use]
    pub fn db_format(&self) -> Option<&str> {
        self.db_format.as_deref()
    }

    /// どこから知ったか。
    #[must_use]
    pub const fn source(&self) -> PrevSource {
        self.source
    }
}

/// `apply` の結果。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct ApplyReport {
    /// 消せずに残ったファイル (次の片付けで消す)。
    pub left: Vec<FileName>,
}

/// 続きの向き。
#[derive(Debug)]
#[non_exhaustive]
pub enum ResumeOutcome {
    /// 置き換えを終えた (`evorag(.exe)` が新しいもの)。
    Completed,
    /// 元に戻した (`evorag(.exe)` が置き換えの前のもの)。操作はやり直さない。
    Reverted {
        /// 置き換えの後の確かめに通らなかったときは、その理由。
        check: Option<CheckFailure>,
    },
}

/// 計画の続きの結果 (12 §7.3 の「続きと片付け」)。
#[derive(Debug)]
#[non_exhaustive]
pub struct ResumeReport {
    /// 計画の操作。
    pub op: Op,
    /// 向き。
    pub outcome: ResumeOutcome,
    /// 役割に当たらないので触れなかったファイル。
    pub untouched: Vec<FileName>,
    /// 置くべき役割のファイルが見つからなかった名前 (`evorag.prev` など)。計画は済みにした。
    pub missing: Vec<FileName>,
    /// 消せずに残したファイル。
    pub left: Vec<FileName>,
    /// 動いているイメージが、続きの後の `evorag(.exe)` と違う (SHA-256 で比べる。動いているイメージの
    /// SHA-256 を求められないときも真)。
    ///
    /// 例: 起動の片付けが元に戻したので、`evorag(.exe)` は `current` だが、動いているのは `next`。
    /// 真のとき、呼ぶ側はデータの移行 (DB の形式の更新など) の前に止まり、evorag をもう一度起動するよう
    /// 利用者に示すこと (動いている版と、置いてある版が違うため)。
    pub running_replaced: bool,
}

/// 名前での片付けの結果。
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct CleanupReport {
    /// `evorag.prev` に戻したファイル。
    pub restored_prev: Option<FileName>,
    /// 消したファイル。
    pub removed: Vec<FileName>,
    /// 消せずに残したファイル (起動の片付けのとき)。
    pub left: Vec<FileName>,
    /// 中身が `evorag(.exe)` とも `evorag.prev` とも違うので、消さずに残した `evorag.tmp`
    /// (起動の片付けのとき。apply・rollback の前は断る)。
    pub kept: Vec<FileName>,
    /// `evorag(.exe)` が無いので、何もしなかった。
    pub skipped_without_exe: bool,
}

/// 起動の片付けの結果。
#[derive(Debug)]
#[non_exhaustive]
pub enum Startup {
    /// ほかの `update` が動いているので、片付けずに進む。
    Busy,
    /// 計画の続きを進めた。
    Resumed(ResumeReport),
    /// 計画が無いので、名前で片付けた。
    Cleaned(CleanupReport),
}
