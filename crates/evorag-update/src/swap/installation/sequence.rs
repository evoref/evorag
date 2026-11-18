//! OS ごとの置き換えの手順 (12 §7.3 の apply と rollback の表) と、失敗したときに元に戻す流れ。

use evorag_store::fsutil;

use super::super::env::Env;
use super::super::error::{CheckFailure, Failure, SwapError};
use super::super::names::{FileName, Method, Op};
use super::super::state::{Plan, Role};
use super::converge::Entry;
use super::report::{ApplyReport, Build, PrevInfo};
use super::{Installation, new_plan, role};
use crate::Sha256Digest;

impl Installation {
    pub(crate) fn apply_bytes(
        &mut self,
        bytes: &[u8],
        sha256: Sha256Digest,
        current: &Build,
        next: &Build,
        env: &mut Env<'_>,
    ) -> Result<ApplyReport, SwapError> {
        let st = self.precheck()?;
        let current_sha = self.regular(FileName::Exe)?;
        if current_sha == sha256 {
            return Err(SwapError::SameFile);
        }
        let old_prev = match self.inspect(FileName::Prev)? {
            Entry::Absent => None,
            Entry::File(d) => Some(Role {
                sha256: d,
                version: None,
                db_format: None,
            }),
            Entry::Other => {
                return Err(SwapError::NotRegularFile {
                    name: FileName::Prev,
                });
            }
        };
        // 1: evorag.new を書いて書き切り、読み直して確かめる。
        if let Err(e) = self.write_new(bytes, sha256, env) {
            if !e.is_crash() {
                self.discard(FileName::New);
            }
            return Err(e);
        }
        let mut plan = new_plan(
            Op::Apply,
            self.dir_key(),
            role(current_sha, current),
            role(sha256, next),
            old_prev,
        );
        let done = st.done;
        if let Err(e) = self.save(Some(&plan), done.as_ref(), env) {
            if !e.is_crash() {
                self.discard(FileName::New);
            }
            return Err(e);
        }
        match self.method {
            #[cfg(windows)]
            Method::TwoStep => self.apply_two_step(&mut plan, done.as_ref(), env),
            Method::Replace => self.apply_replace(&mut plan, done.as_ref(), env),
        }
    }

    /// Windows の apply (12 §7.3 の表): 2 → 段 2 → 3・4 (隙間) → 段 4 → 5 (確かめ) → 段 5 → 6 → 済み。
    #[cfg(windows)]
    fn apply_two_step(
        &self,
        plan: &mut Plan,
        done: Option<&Plan>,
        env: &mut Env<'_>,
    ) -> Result<ApplyReport, SwapError> {
        use FileName::{Exe, New, Prev, PrevOld};
        // 2: 前回の evorag.prev を evorag.prev.old に (消さない)。何もしないときも段 2 を書く (置き換えを
        // 始めた後は、段 1 の「役割に当たらない evorag.new は消してよい」が当たらないように)。
        if plan.old_prev.is_some()
            && let Err(e) = self.rename(Prev, PrevOld, env)
        {
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 2;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 3・4 (隙間): 自分を evorag.prev に、evorag.new を evorag.exe に。間に状態の書き込みを入れない。
        if let Err(e) = self.move_aside_and_place(Exe, Prev, New, env) {
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 4;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 5: 置き換えの後の確かめ。
        if let Err(f) = self.verify(&plan.next, env) {
            return Err(self.abort_check(plan, done, f, env));
        }
        plan.stage = 5;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 6: evorag.prev.old を消す。消せなければ残し、次の片付けで消す。
        let mut report = ApplyReport::default();
        if plan.old_prev.is_some() {
            match self.remove(PrevOld, env) {
                Ok(()) => {}
                Err(e) if e.is_crash() => return Err(e),
                Err(_) => report.left.push(PrevOld),
            }
        }
        plan.stage = 6;
        self.finish_swapped(plan, env)?;
        Ok(report)
    }

    /// Linux・macOS の apply (12 §7.3 の表): 2 (写し) → 段 2 → 3 → 段 3 → 4 (確かめ) → 段 4 → 5 → 済み。
    fn apply_replace(
        &self,
        plan: &mut Plan,
        done: Option<&Plan>,
        env: &mut Env<'_>,
    ) -> Result<ApplyReport, SwapError> {
        use FileName::{Exe, New, Prev, Tmp};
        // 2: 今の evorag の写しを evorag.tmp に。
        if let Err(e) = self.copy(Exe, Tmp, env) {
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 2;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 3: evorag.new を evorag の上に。
        if let Err(e) = self.rename(New, Exe, env) {
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 3;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 4: 置き換えの後の確かめ。
        if let Err(f) = self.verify(&plan.next, env) {
            return Err(self.abort_check(plan, done, f, env));
        }
        plan.stage = 4;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 5: evorag.tmp を evorag.prev に (前回の evorag.prev を上書きする)。
        if let Err(e) = self.rename(Tmp, Prev, env) {
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 5;
        self.finish_swapped(plan, env)?;
        Ok(ApplyReport::default())
    }

    pub(super) fn rollback_files(
        &mut self,
        prev: &PrevInfo,
        current: &Build,
        env: &mut Env<'_>,
    ) -> Result<(), SwapError> {
        let st = self.precheck()?;
        let current_sha = self.regular(FileName::Exe)?;
        // Windows の 1: evorag.prev の SHA-256 を求め、確かめたものと同じであること。
        let prev_sha = match self.inspect(FileName::Prev)? {
            Entry::Absent => return Err(SwapError::NoPrev),
            Entry::Other => {
                return Err(SwapError::NotRegularFile {
                    name: FileName::Prev,
                });
            }
            Entry::File(d) => d,
        };
        if prev_sha != prev.sha256 {
            return Err(SwapError::Changed {
                name: FileName::Prev,
            });
        }
        if prev_sha == current_sha {
            return Err(SwapError::SameFile);
        }
        let mut plan = new_plan(
            Op::Rollback,
            self.dir_key(),
            role(current_sha, current),
            Role {
                sha256: prev_sha,
                version: Some(prev.version.to_string()),
                db_format: prev.db_format.clone(),
            },
            None,
        );
        let done = st.done;
        match self.method {
            #[cfg(windows)]
            Method::TwoStep => self.rollback_two_step(&mut plan, done.as_ref(), env),
            Method::Replace => self.rollback_replace(&mut plan, done.as_ref(), env),
        }
    }

    /// Windows の rollback (12 §7.3 の表): 段 1 → 2・3 (隙間) → 段 3 → 4 → 済み。
    #[cfg(windows)]
    fn rollback_two_step(
        &self,
        plan: &mut Plan,
        done: Option<&Plan>,
        env: &mut Env<'_>,
    ) -> Result<(), SwapError> {
        use FileName::{Exe, Prev, Tmp};
        self.save(Some(plan), done, env)?;
        // 2・3 (隙間): 自分を evorag.tmp に、evorag.prev を evorag.exe に。間に状態の書き込みを入れない。
        if let Err(e) = self.move_aside_and_place(Exe, Tmp, Prev, env) {
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 3;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 4: evorag.tmp を evorag.prev に。
        if let Err(e) = self.rename(Tmp, Prev, env) {
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 4;
        self.finish_swapped(plan, env)
    }

    /// Linux・macOS の rollback (12 §7.3 の表): 1 (写し) → 段 1 → 2 → 段 2 → 3 → 済み。
    fn rollback_replace(
        &self,
        plan: &mut Plan,
        done: Option<&Plan>,
        env: &mut Env<'_>,
    ) -> Result<(), SwapError> {
        use FileName::{Exe, Prev, Tmp};
        // 1: 今の evorag の写しを evorag.tmp に。計画はその後に書く。書けなければ写しを消す。
        if let Err(e) = self
            .copy(Exe, Tmp, env)
            .and_then(|()| self.save(Some(plan), done, env))
        {
            if !e.is_crash() {
                self.discard(Tmp);
            }
            return Err(e);
        }
        // 2: evorag.prev を evorag の上に。
        if let Err(e) = self.rename(Prev, Exe, env) {
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 2;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 3: evorag.tmp を evorag.prev に。
        if let Err(e) = self.rename(Tmp, Prev, env) {
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 3;
        self.finish_swapped(plan, env)
    }

    /// Windows: `target` (`evorag.exe` か、ほかの役割の名前) のファイルを `aside` に改名してから、
    /// `source` を `target` に改名する。上書きしない改名なので、`target` を先に空ける。
    ///
    /// `target` が `evorag.exe` のときは、2 つの改名の間が「Windows の隙間」(12 §7.3):
    /// - 1 つ目の改名の前に、`source` をほかのプログラムが掴んでいないことを確かめる (削除の権限・共有
    ///   なしで開いて閉じる。ロックは再試行する)。
    /// - 1 つ目の改名は再試行する。2 つ目の改名は 1 回だけ試す (`evorag.exe` が無い間を延ばさない)。
    /// - 2 つの改名の間には、状態の書き込み・同期・表示を入れない。
    ///
    /// 2 つ目の改名に失敗したら、すぐに `aside` を `target` に戻し (再試行してよい)、2 つ目の失敗を返す。
    #[cfg(windows)]
    pub(super) fn move_aside_and_place(
        &self,
        target: FileName,
        aside: FileName,
        source: FileName,
        env: &mut Env<'_>,
    ) -> Result<(), SwapError> {
        let gap = target == FileName::Exe;
        if gap {
            self.check_not_open(source)?;
        }
        self.rename(target, aside, env)?;
        let placed = if gap {
            self.rename_once(source, target, env)
        } else {
            self.rename(source, target, env)
        };
        match placed {
            Ok(()) => Ok(()),
            Err(e) if e.is_crash() => Err(e),
            Err(e) => {
                // evorag.exe を空けたままにしない。戻せなければ、続き (計画の reverting) が戻す。
                match self.rename(aside, target, env) {
                    Err(r) if r.is_crash() => Err(r),
                    _ => Err(e),
                }
            }
        }
    }

    /// 書きかけ・写しかけのファイルを捨てる (断るとき。消せなければ次の片付けで消す)。
    fn discard(&self, name: FileName) {
        let _ = fsutil::retry_locked(|| std::fs::remove_file(self.path(name)));
    }

    /// ファイルの入れ替えが済んだ後に、計画を済みにする。書けなければ [`SwapError::NotRecorded`]。
    fn finish_swapped(&self, plan: &Plan, env: &mut Env<'_>) -> Result<(), SwapError> {
        self.finish(plan, env).map_err(|e| {
            if e.is_crash() {
                e
            } else {
                SwapError::NotRecorded(Box::new(e))
            }
        })
    }

    /// 置き換えの途中の失敗: 計画を `reverting` にしてから元に戻す。
    fn abort(
        &self,
        plan: &mut Plan,
        done: Option<&Plan>,
        error: SwapError,
        env: &mut Env<'_>,
    ) -> SwapError {
        if error.is_crash() {
            return error;
        }
        self.revert_after(plan, done, error.into_failure(), env)
    }

    /// 置き換えの後の確かめに通らなかった: 元に戻す (Windows は `evorag.rejected` を経る)。
    fn abort_check(
        &self,
        plan: &mut Plan,
        done: Option<&Plan>,
        check: CheckFailure,
        env: &mut Env<'_>,
    ) -> SwapError {
        self.revert_after(plan, done, Failure::Check(check), env)
    }

    fn revert_after(
        &self,
        plan: &mut Plan,
        done: Option<&Plan>,
        cause: Failure,
        env: &mut Env<'_>,
    ) -> SwapError {
        plan.reverting = true;
        // 状態のファイルを書けなくても、ファイルは元に戻す (計画は次の機会に、SHA-256 で見分けて続ける)。
        if let Err(e) = self.save(Some(plan), done, env)
            && e.is_crash()
        {
            return e;
        }
        match self.converge(plan.clone(), done.cloned(), env) {
            Ok(_) => SwapError::Reverted(cause),
            Err(e) if e.is_crash() => e,
            Err(e) => SwapError::RevertFailed {
                cause,
                revert: Box::new(e),
            },
        }
    }
}
