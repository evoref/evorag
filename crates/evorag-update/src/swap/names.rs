//! インストール先のファイルの名前と、置き換えの方式 (12 §7.3)。

use std::fmt;

/// インストール先で、置き換えが扱うファイルの名前 (12 §7.3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileName {
    /// `evorag.exe` (Windows) か `evorag` (Linux・macOS)。
    Exe,
    /// `evorag.new`: 書いた新しいファイル。
    New,
    /// `evorag.tmp`: 置き換えの途中の写し・改名。
    Tmp,
    /// `evorag.prev`: 前の版。
    Prev,
    /// `evorag.prev.old`: apply の途中に退避した、前回の `evorag.prev`。
    PrevOld,
    /// `evorag.rejected`: 元に戻すときに、`evorag.exe` から外した新しいファイル。
    Rejected,
    /// `evorag.probe`: 書き込めるかを試すファイル。
    Probe,
}

impl FileName {
    /// 計画の続きで役割を調べる名前 (12 §7.3 の「続きと片付け」)。
    pub const ROLE_NAMES: [Self; 6] = [
        Self::Exe,
        Self::New,
        Self::Tmp,
        Self::Prev,
        Self::PrevOld,
        Self::Rejected,
    ];

    /// 計画が無いときに名前で消す残りのファイル (12 §7.3)。
    pub const LEFTOVERS: [Self; 5] = [
        Self::New,
        Self::Tmp,
        Self::PrevOld,
        Self::Rejected,
        Self::Probe,
    ];

    /// この OS でのファイルの名前。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exe => EXE_NAME,
            Self::New => "evorag.new",
            Self::Tmp => "evorag.tmp",
            Self::Prev => "evorag.prev",
            Self::PrevOld => "evorag.prev.old",
            Self::Rejected => "evorag.rejected",
            Self::Probe => "evorag.probe",
        }
    }
}

impl fmt::Display for FileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 動いている実行ファイルの名前 (12 §7.2 の始める前の確かめの 1)。
pub const EXE_NAME: &str = if cfg!(windows) {
    "evorag.exe"
} else {
    "evorag"
};

/// 置き換えの操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// `evorag update apply`。
    Apply,
    /// `evorag update rollback`。
    Rollback,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Apply => "apply",
            Self::Rollback => "rollback",
        })
    }
}

/// 置き換えの方式 (12 §7.3 の表)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Method {
    /// Windows: 自分を改名してから、上書きしない改名で置く。
    #[cfg(windows)]
    TwoStep,
    /// Linux・macOS: 写しを作り、`rename` で上から置いてフォルダを同期する。
    Replace,
}

impl Method {
    /// この OS の方式。
    pub(crate) const HOST: Self = {
        #[cfg(windows)]
        {
            Self::TwoStep
        }
        #[cfg(not(windows))]
        {
            Self::Replace
        }
    };

    /// apply の置き換えの後の確かめの段 (Windows は 5、Linux は 4)。
    pub(crate) const fn verify_stage(self) -> u32 {
        match self {
            #[cfg(windows)]
            Self::TwoStep => 5,
            Self::Replace => 4,
        }
    }
}

/// `evorag.exe` が無いとき (Windows の隙間) の戻し方 (12 §7.3)。表示の文言は呼ぶ側が文言キーで作る。
///
/// 「[`Self::to`] が無いときは、[`Self::from_first_of`] のうち最初にあるものを [`Self::to`] に改名する」。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct GapRecovery {
    /// 操作。
    pub op: Op,
    /// 改名する元の候補 (先にあるものを使う)。
    pub from_first_of: Vec<FileName>,
    /// 改名する先 (`evorag(.exe)`)。
    pub to: FileName,
}

impl GapRecovery {
    /// 操作ごとの戻し方。apply は `evorag.new`、無ければ `evorag.prev`。rollback は `evorag.prev`。
    #[must_use]
    pub fn for_op(op: Op) -> Self {
        let from_first_of = match op {
            Op::Apply => vec![FileName::New, FileName::Prev],
            Op::Rollback => vec![FileName::Prev],
        };
        Self {
            op,
            from_first_of,
            to: FileName::Exe,
        }
    }
}
