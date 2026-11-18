//! 置き換え・巻き戻し・片付けで断った・失敗した理由 (07 §5 の終了コード 1)。

use std::path::PathBuf;

use super::names::FileName;
use super::probe::ProbeError;

/// ファイルの操作の種類 (ログと表示のため)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FileOp {
    /// 読む・SHA-256 を求める。
    Read,
    /// 作って書く。
    Write,
    /// 改名する。
    Rename,
    /// 消す。
    Remove,
    /// フォルダを同期する。
    Sync,
}

impl std::fmt::Display for FileOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Rename => "rename",
            Self::Remove => "remove",
            Self::Sync => "sync",
        })
    }
}

/// 置き換えの後の確かめ (`--version`) に通らなかった理由。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CheckFailure {
    /// 1 行目が `evorag <版>` と違う。
    #[error("it printed `{first_line}` instead of `evorag {expected}`")]
    WrongVersion {
        /// 表示用に整えた 1 行目。
        first_line: String,
        /// 期待した版。
        expected: String,
    },
    /// 動かない・時間切れ。
    #[error("{0}")]
    Probe(#[from] ProbeError),
}

/// 置き換えを元に戻した・戻そうとした原因。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Failure {
    /// ファイルの操作に失敗した (ロックで 5 回まで試した後を含む)。
    #[error("cannot {op} {name}: {source}")]
    File {
        /// 操作。
        op: FileOp,
        /// ファイル。
        name: FileName,
        /// OS の失敗。
        #[source]
        source: std::io::Error,
    },
    /// 状態のファイルを書けない。
    #[error("cannot write the state file: {0}")]
    State(#[source] Box<SwapError>),
    /// 置き換えの後の確かめに通らなかった。
    #[error("the new evorag failed the --version check: {0}")]
    Check(#[source] CheckFailure),
}

/// 置き換え・巻き戻し・片付けの失敗。どれも 07 §5 の終了コード 1。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SwapError {
    /// 動いている実行ファイルの名前が `evorag(.exe)` でない (12 §7.2 の始める前の確かめの 1)。
    #[error("the running executable is named {found}, not {}", FileName::Exe)]
    ExeName {
        /// 動いている実行ファイルの名前。
        found: String,
    },
    /// ほかの `evorag update` が動いている (`update/lock`)。
    #[error("another evorag update is running")]
    Busy,
    /// 状態のファイルの計画が読めない。手で戻す手順を示す。
    #[error("the update state file cannot be read: {reason}")]
    StateUnreadable {
        /// 理由。
        reason: String,
    },
    /// 残った計画が、ほかのフォルダのもの。
    #[error("an unfinished update belongs to another folder: {}", dir.display())]
    ForeignPlan {
        /// 計画のフォルダ。
        dir: PathBuf,
    },
    /// 残った計画がある (先に続きを済ませる)。
    #[error("an unfinished update must be finished first")]
    PlanPending,
    /// `evorag(.exe)` が計画のどの役割にも当たらない。何も動かさず、計画を残した。手で戻す手順を示す。
    #[error("{} matches none of the files of the unfinished update", FileName::Exe)]
    UnknownRole,
    /// 置く先に、役割に当たらないファイルがある。触れずに止めた。
    #[error("{name} is not a file of this update and was left untouched")]
    ForeignFile {
        /// ファイル。
        name: FileName,
    },
    /// `evorag(.exe)` に置けるファイルが無い。手で戻す手順を示す。
    #[error(
        "no file of the unfinished update can be put back as {}",
        FileName::Exe
    )]
    NothingToRestore,
    /// 残ったファイルを消せない (12 §7.2 の始める前の確かめの 5)。
    #[error("cannot remove the leftover {name}: {source}")]
    Leftover {
        /// ファイル。
        name: FileName,
        /// OS の失敗。
        #[source]
        source: std::io::Error,
    },
    /// インストール先に書き込めない。
    #[error("the install folder {} is not writable: {source}", dir.display())]
    NotWritable {
        /// インストール先。
        dir: PathBuf,
        /// OS の失敗。
        #[source]
        source: std::io::Error,
    },
    /// ファイルがシンボリックリンク・リパースポイント・通常のファイルでない。
    #[error("{name} is a symbolic link, a reparse point or not a regular file")]
    NotRegularFile {
        /// ファイル。
        name: FileName,
    },
    /// Unix: インストール先のフォルダか `evorag` が、動いている利用者のものでない、または group か other が書ける。
    #[error("{} is {problem}", path.display())]
    UnsafeOwnership {
        /// フォルダかファイル。
        path: PathBuf,
        /// 問題。
        problem: OwnershipProblem,
    },
    /// `evorag.prev` が無い (rollback の 1)。
    #[error("there is no evorag.prev to roll back to")]
    NoPrev,
    /// `evorag.prev` の `--version` の 1 行目が `evorag <版>` でない (rollback の 2)。
    #[error("evorag.prev does not print an evorag version (first line `{first_line}`)")]
    PrevNotEvorag {
        /// 表示用に整えた 1 行目。
        first_line: String,
    },
    /// `evorag.prev` を動かせない (rollback の 2)。
    #[error("cannot run evorag.prev --version: {0}")]
    PrevProbe(#[source] ProbeError),
    /// 確かめた後に、ファイルが変わった。
    #[error("{name} changed after it was checked")]
    Changed {
        /// ファイル。
        name: FileName,
    },
    /// 置くものが今の `evorag(.exe)` と同じ。
    #[error("the replacement is the same file as the current {}", FileName::Exe)]
    SameFile,
    /// 書いた `evorag.new` を読み直した SHA-256 が合わない。
    #[error("the written evorag.new does not match its sha256")]
    WrittenDigestMismatch,
    /// 置き換えに失敗し、元に戻した (起動できる状態)。
    #[error("the update failed and was undone: {0}")]
    Reverted(#[source] Failure),
    /// 置き換えに失敗し、元に戻すのにも失敗した。計画を残した (次の update と起動の片付けが続ける)。
    #[error("the update failed ({cause}) and could not be undone: {revert}")]
    RevertFailed {
        /// 置き換えの失敗。
        cause: Failure,
        /// 元に戻すときの失敗。
        revert: Box<SwapError>,
    },
    /// ファイルの操作の失敗。
    #[error("cannot {op} {name}: {source}")]
    File {
        /// 操作。
        op: FileOp,
        /// ファイル。
        name: FileName,
        /// OS の失敗。
        #[source]
        source: std::io::Error,
    },
    /// インストール先・状態のフォルダ・状態のファイルの操作の失敗。
    #[error("cannot {what} {}: {source}", path.display())]
    Io {
        /// 操作。
        what: &'static str,
        /// パス。
        path: PathBuf,
        /// OS の失敗。
        #[source]
        source: std::io::Error,
    },
    /// 試験が差し込んだ強制終了 (そこで止まり、後の処理をしない)。
    #[doc(hidden)]
    #[error("stopped by a test")]
    Crashed,
}

impl SwapError {
    /// 07 §5 の終了コード (置き換えの失敗はどれも 1)。
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        1
    }

    /// §7.3 の「手で戻す手順」を示すべきか。
    #[must_use]
    pub const fn needs_manual_recovery(&self) -> bool {
        matches!(
            self,
            Self::StateUnreadable { .. }
                | Self::UnknownRole
                | Self::NothingToRestore
                | Self::RevertFailed { .. }
        )
    }

    pub(crate) fn file(op: FileOp, name: FileName) -> impl FnOnce(std::io::Error) -> Self {
        move |source| Self::File { op, name, source }
    }

    /// 元に戻す原因にする。
    pub(crate) fn into_failure(self) -> Failure {
        match self {
            Self::File { op, name, source } => Failure::File { op, name, source },
            other => Failure::State(Box::new(other)),
        }
    }
}

/// Unix のインストール先の持ち主・権限の問題 (12 §7.3 の「インストール先の確かめ」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OwnershipProblem {
    /// 動いている利用者のものでない (root で動いていて、持ち主が root でないときを含む)。
    NotOwnedByUser,
    /// group か other が書ける (sticky の無いフォルダ)。
    WritableByOthers,
}

impl std::fmt::Display for OwnershipProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotOwnedByUser => "not owned by the running user",
            Self::WritableByOthers => "writable by the group or others",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const _: () = {
        const fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<SwapError>();
    };
}
