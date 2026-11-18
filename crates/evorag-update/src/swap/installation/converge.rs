//! 計画の続き: SHA-256 で役割を見分けて、置き終える・元に戻す (12 §7.3 の「続きと片付け」)。

use std::collections::BTreeMap;

use super::super::env::Env;
use super::super::error::SwapError;
use super::super::names::{FileName, Method, Op};
use super::super::state::Plan;
use super::Installation;
use super::report::{ResumeOutcome, ResumeReport};
use crate::Sha256Digest;

/// ファイルの中身の見分け。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Entry {
    Absent,
    File(Sha256Digest),
    /// リンク・フォルダ・大きすぎるファイル・(Unix) ほかの利用者のファイルなど (どの役割にも当たらない。
    /// 動かさず、置かない)。
    Other,
}

/// 名前ごとの中身。操作のたびに直す。
#[derive(Debug, Default, Clone)]
pub(super) struct Snapshot(BTreeMap<FileName, Entry>);

impl Snapshot {
    fn get(&self, name: FileName) -> Entry {
        self.0.get(&name).copied().unwrap_or(Entry::Absent)
    }

    fn holds(&self, name: FileName, digest: Sha256Digest) -> bool {
        self.get(name) == Entry::File(digest)
    }

    fn moved(&mut self, from: FileName, to: FileName) {
        let e = self.0.remove(&from).unwrap_or(Entry::Absent);
        self.0.insert(to, e);
    }

    fn copied(&mut self, from: FileName, to: FileName) {
        let e = self.get(from);
        self.0.insert(to, e);
    }

    fn removed(&mut self, name: FileName) {
        self.0.remove(&name);
    }
}

/// 置く先に置いた結果。
enum Placed {
    Done,
    /// 置くべき中身のファイルが無い。
    Missing,
}

/// 置くのを確かめるだけか、ファイルを動かすか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// 写しのスナップショットの上で確かめるだけ (ファイルに触れない)。
    Check,
    /// ファイルを動かす。
    Apply,
}

impl Installation {
    /// 名前ごとの中身を調べる。
    pub(super) fn scan(&self) -> Result<Snapshot, SwapError> {
        let mut snap = Snapshot::default();
        for name in FileName::ROLE_NAMES {
            snap.0.insert(name, self.inspect(name)?);
        }
        Ok(snap)
    }

    /// 計画に従って、役割のファイルを置き終える (続き) か、元に戻す。役割は SHA-256 で見分ける。
    ///
    /// 置く先に役割に当たらないファイルがある・`evorag(.exe)` に置けるものが無いときは、ファイルを
    /// 動かす前に断る (何も動かさず、計画を残す)。
    pub(super) fn converge(
        &self,
        mut plan: Plan,
        done: Option<Plan>,
        env: &mut Env<'_>,
    ) -> Result<ResumeReport, SwapError> {
        use FileName::{Exe, New, Prev, PrevOld, Rejected, Tmp};
        let mut check = None;
        let mut snap = self.scan()?;
        let (current, next) = (plan.current.sha256, plan.next.sha256);
        let mut roles = vec![current, next];
        roles.extend(plan.old_prev.as_ref().map(|r| r.sha256));
        let anywhere = |snap: &Snapshot, d| FileName::ROLE_NAMES.iter().any(|&n| snap.holds(n, d));
        let exe_was_current = snap.holds(Exe, current);
        let mut forward = if plan.reverting {
            false
        } else {
            match snap.get(Exe) {
                Entry::File(d) if d == next => true,
                Entry::File(d) if d == current => false,
                Entry::Absent if anywhere(&snap, next) => true,
                Entry::Absent if anywhere(&snap, current) => false,
                Entry::Absent => return Err(SwapError::NothingToRestore),
                Entry::File(_) | Entry::Other => return Err(SwapError::UnknownRole),
            }
        };
        let done = done.as_ref();
        let forward_targets = [(Exe, next), (Prev, current)];
        if forward {
            self.check_placements(&snap, &forward_targets, &roles, env)?;
            self.place(
                &mut snap,
                &forward_targets,
                &roles,
                Exe,
                next,
                Mode::Apply,
                env,
            )?;
            let verified = plan.op == Op::Rollback
                || plan.stage >= self.method.verify_stage()
                || self.running == Some(next);
            if !verified {
                match self.verify(&plan.next, env) {
                    Ok(()) => {
                        plan.stage = self.method.verify_stage();
                        self.save(Some(&plan), done, env)?;
                    }
                    Err(f) => {
                        check = Some(f);
                        forward = false;
                    }
                }
            }
        }
        let targets: Vec<(FileName, Sha256Digest)> = if forward {
            forward_targets.to_vec()
        } else {
            let prev = match plan.op {
                Op::Apply => plan.old_prev.as_ref().map(|r| r.sha256),
                Op::Rollback => Some(next),
            };
            let targets: Vec<_> = std::iter::once((Exe, current))
                .chain(prev.map(|d| (Prev, d)))
                .collect();
            self.check_placements(&snap, &targets, &roles, env)?;
            if !plan.reverting {
                plan.reverting = true;
                self.save(Some(&plan), done, env)?;
            }
            targets
        };
        let mut missing = Vec::new();
        for &(name, want) in &targets {
            match self.place(&mut snap, &targets, &roles, name, want, Mode::Apply, env)? {
                Placed::Done => {}
                Placed::Missing if name == Exe => return Err(SwapError::NothingToRestore),
                Placed::Missing => missing.push(name),
            }
        }
        // 残りを片付ける: 役割に当たるものは消し、当たらないものには触れない。
        let (mut untouched, mut left) = (Vec::new(), Vec::new());
        for name in [New, Tmp, PrevOld, Rejected, Prev] {
            if targets.iter().any(|&(n, _)| n == name) {
                continue;
            }
            let disposable = match snap.get(name) {
                Entry::Absent => continue,
                Entry::File(d) if roles.contains(&d) => true,
                // 置き換えの前 (段 1) で止まった計画の evorag.new と、元に戻すときの書きかけの写し。
                Entry::File(_) if name == New && plan.stage == 1 => true,
                Entry::File(_) if name == Tmp && !forward && exe_was_current => true,
                Entry::File(_) | Entry::Other => false,
            };
            if !disposable {
                untouched.push(name);
                continue;
            }
            match self.remove(name, env) {
                Ok(()) => snap.removed(name),
                Err(e) if e.is_crash() => return Err(e),
                Err(_) => left.push(name),
            }
        }
        let outcome = if forward {
            plan.reverting = false;
            self.finish(&plan, env).map_err(|e| {
                if e.is_crash() {
                    e
                } else {
                    SwapError::NotRecorded(Box::new(e))
                }
            })?;
            ResumeOutcome::Completed
        } else {
            self.save(None, done, env)?;
            ResumeOutcome::Reverted { check }
        };
        Ok(ResumeReport {
            op: plan.op,
            outcome,
            untouched,
            missing,
            left,
        })
    }

    /// `targets` を順に置けるかを、ファイルに触れずに確かめる (役割に当たらないファイルが置く先にある・
    /// `evorag(.exe)` に置けるものが無い)。
    fn check_placements(
        &self,
        snap: &Snapshot,
        targets: &[(FileName, Sha256Digest)],
        roles: &[Sha256Digest],
        env: &mut Env<'_>,
    ) -> Result<(), SwapError> {
        let mut sim = snap.clone();
        for &(name, want) in targets {
            if let Placed::Missing =
                self.place(&mut sim, targets, roles, name, want, Mode::Check, env)?
                && name == FileName::Exe
            {
                return Err(SwapError::NothingToRestore);
            }
        }
        Ok(())
    }

    /// `target` に、SHA-256 が `want` のファイルを置く。
    ///
    /// 置く先にあるファイルは、ほかの置く先で要る唯一のものならそこへ移し (Unix は写す)、要らない
    /// ものなら、Windows は `evorag.exe` なら `evorag.rejected` に改名し、ほかは消す。Unix は上から置く。
    /// Windows で `evorag.exe` を空けて置くときは「Windows の隙間」の手順
    /// ([`Self::move_aside_and_place`]) による。
    #[expect(
        clippy::too_many_arguments,
        reason = "the placement needs the whole context"
    )]
    fn place(
        &self,
        snap: &mut Snapshot,
        targets: &[(FileName, Sha256Digest)],
        roles: &[Sha256Digest],
        target: FileName,
        want: Sha256Digest,
        mode: Mode,
        env: &mut Env<'_>,
    ) -> Result<Placed, SwapError> {
        const SOURCES: [FileName; 5] = [
            FileName::New,
            FileName::Tmp,
            FileName::Rejected,
            FileName::PrevOld,
            FileName::Prev,
        ];
        if snap.holds(target, want) {
            return Ok(Placed::Done);
        }
        let real = mode == Mode::Apply;
        let wanted_at = |n: FileName| targets.iter().find(|&&(t, _)| t == n).map(|&(_, d)| d);
        let source = SOURCES.into_iter().find(|&n| {
            n != target && snap.holds(n, want) && wanted_at(n).is_none_or(|d| d != want)
        });
        let Some(source) = source else {
            return Ok(Placed::Missing);
        };
        // Windows: 置く前に、置く先のファイルを改名して空ける先。
        #[cfg(windows)]
        let mut aside: Option<FileName> = None;
        match snap.get(target) {
            Entry::Absent => {}
            Entry::Other => return Err(SwapError::ForeignFile { name: target }),
            Entry::File(d) if !roles.contains(&d) => {
                return Err(SwapError::ForeignFile { name: target });
            }
            Entry::File(d) => {
                let needed_at = targets
                    .iter()
                    .find(|&&(n, w)| n != target && w == d && !snap.holds(n, d))
                    .map(|&(n, _)| n);
                let other_copy = FileName::ROLE_NAMES
                    .iter()
                    .any(|&n| n != target && snap.holds(n, d));
                match needed_at {
                    Some(dest) if !other_copy => {
                        if snap.get(dest) != Entry::Absent {
                            return Err(SwapError::ForeignFile { name: dest });
                        }
                        match self.method {
                            #[cfg(windows)]
                            Method::TwoStep => aside = Some(dest),
                            Method::Replace => {
                                if real {
                                    self.copy(target, dest, env)?;
                                }
                                snap.copied(target, dest);
                            }
                        }
                    }
                    _ => match self.method {
                        #[cfg(windows)]
                        Method::TwoStep if target == FileName::Exe => {
                            let rejected = FileName::Rejected;
                            match snap.get(rejected) {
                                Entry::Absent => {}
                                Entry::File(r) if roles.contains(&r) => {
                                    if real {
                                        self.remove(rejected, env)?;
                                    }
                                    snap.removed(rejected);
                                }
                                Entry::File(_) | Entry::Other => {
                                    return Err(SwapError::ForeignFile { name: rejected });
                                }
                            }
                            aside = Some(rejected);
                        }
                        #[cfg(windows)]
                        Method::TwoStep => {
                            if real {
                                self.remove(target, env)?;
                            }
                            snap.removed(target);
                        }
                        // 上から置く (rename が置き換える)。
                        Method::Replace => {}
                    },
                }
            }
        }
        #[cfg(windows)]
        if let Some(aside) = aside {
            if real {
                self.move_aside_and_place(target, aside, source, env)?;
            }
            snap.moved(target, aside);
            snap.moved(source, target);
            return Ok(Placed::Done);
        }
        if real {
            self.rename(source, target, env)?;
        }
        snap.moved(source, target);
        Ok(Placed::Done)
    }
}
