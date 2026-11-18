//! 置き換えに渡す道具 (`--version` の動かし方) と、試験の強制終了の点。

use super::error::SwapError;
use super::names::FileName;
use super::probe::VersionProbe;

/// ファイルの操作の後に起きたこと。試験が、そこで強制終了したことにするために見る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// 新しいファイル (`evorag.new`・写し) を書き切った。
    Wrote(FileName),
    /// 改名した (Linux はフォルダの同期まで)。
    Renamed(FileName, FileName),
    /// 消した。
    Removed(FileName),
    /// 状態のファイルの一時名を書き切った (改名の前)。
    StateWritten,
    /// 状態のファイルを置いた。
    StateSaved,
}

/// 置き換えに渡す道具: `--version` の動かし方。
pub struct Env<'a> {
    probe: &'a dyn VersionProbe,
    #[cfg(test)]
    crash: Option<&'a mut dyn FnMut(Event) -> bool>,
}

impl std::fmt::Debug for Env<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env").finish_non_exhaustive()
    }
}

impl<'a> Env<'a> {
    /// `--version` を `probe` で動かす。
    #[must_use]
    pub fn new(probe: &'a dyn VersionProbe) -> Self {
        Self {
            probe,
            #[cfg(test)]
            crash: None,
        }
    }

    /// ファイルの操作のたびに `hook` を呼び、真を返したらそこで止める ([`SwapError::Crashed`])。
    /// 止めた後は、ファイルにも状態のファイルにも何もしない (強制終了の模擬)。
    ///
    /// 強制終了の点は、改名・写し・新しいファイルの書き切り・削除・状態のファイルの書き込みの後に取る。
    /// フォルダやファイルの同期の後は別の点にしない (同期は直前の操作に含める)。
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_crash_hook(mut self, hook: &'a mut dyn FnMut(Event) -> bool) -> Self {
        self.crash = Some(hook);
        self
    }

    pub(crate) fn probe(&self) -> &'a dyn VersionProbe {
        self.probe
    }

    /// 操作の後の点。試験の強制終了の点に当たれば `SwapError::Crashed` (試験のビルドだけ)。
    #[cfg_attr(
        not(test),
        expect(
            clippy::unnecessary_wraps,
            clippy::unused_self,
            reason = "the test build stops here"
        )
    )]
    pub(crate) fn event(&mut self, event: Event) -> Result<(), SwapError> {
        #[cfg(test)]
        if self.crash.as_mut().is_some_and(|hook| hook(event)) {
            return Err(SwapError::Crashed);
        }
        #[cfg(not(test))]
        let _ = event;
        Ok(())
    }
}
