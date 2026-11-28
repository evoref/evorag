//! 区切り 1 つの呼び出しの進め方 (04 §5.2.1 の「型が合わないとき」「出力が上限で切れたとき」「失敗の扱い」
//! 「止められたとき」)。I/O を持たない状態機械で、呼び手が要求の終わり方を渡すと、次にすることを返す。

use evorag_memory::b2::{InvalidReason, Outcome, Parsed, Reading};

/// 締切切れと 5xx がこの回数続いた区切りは、`failed` の台帳を書く (04 §5.2.1 の「失敗の扱い」)。
pub(super) const SERVER_ERROR_LIMIT: u32 = 3;
/// 続けてこの数の区切りが 4xx か 5xx で終わったら、区切りの呼び出しを止める (04 §5.2.1 の「続けて失敗したとき」。
/// 暫定)。
pub(super) const CONSECUTIVE_LIMIT: u32 = 3;

/// 1 回の要求の終わり方 (呼び手が I/O の結果を分類したもの)。
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Ended {
    /// 生成を終えたか (`stopped` が偽)、利用者の操作で止めた (真)。出力の読み取りの結果。
    Read { reading: Reading, stopped: bool },
    /// 締切切れ・5xx・408・429・途中の切断・ルーターのモデルの読み込みの失敗。`count` は、この区切りで続いた
    /// 回数 (この失敗を含む)。
    Transient { count: u32 },
    /// 4xx (408・429 を除く)。
    Rejected,
    /// 繋がらない。
    Unreachable,
    /// 構成の誤り (接続先の形・ループバック以外・ルーターのモデルの名前が無い)。
    Misconfigured,
}

/// 次にすること。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Next {
    /// 呼び出し `i` を送る (やり直しを含む)。
    Send(usize),
    /// 呼び出し `i` を往復の境で半分に分けて送り直す。分けたら [`Flow::halved`]、分けられなければ
    /// [`Flow::unsplittable`] を呼ぶ。
    Halve(usize),
    Finish(Finish),
}

/// 区切りの終わり方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Finish {
    /// 台帳を書く ([`Flow::into_results`] の結果で)。`gave_up` は、締切切れか 5xx が続いたので諦めた呼び出しが
    /// あること。
    Write {
        stopped: bool,
        gave_up: bool,
    },
    /// 止めたが、型の合った結果が 1 つも無い (台帳を書かない。続けての失敗にも数えない)。
    Stopped,
    /// 締切切れ・5xx で、書かずに次の機会に送り直す。
    Retry,
    /// 4xx で、書かずに続けての失敗に数える。
    Rejected,
    Unreachable,
    Misconfigured,
}

/// 区切り 1 つの呼び出しの進み具合。
#[derive(Debug, Default)]
pub(super) struct Flow {
    next: usize,
    retried: bool,
    gave_up: bool,
    results: Vec<Outcome>,
}

impl Flow {
    /// 呼び出しが `requests` 個の区切りを始める。呼び出しが無い (番号を振った文が無い) なら、すぐ書く。
    pub(super) fn start(&self, requests: usize) -> Next {
        self.advance(requests)
    }

    fn advance(&self, requests: usize) -> Next {
        if self.next < requests {
            Next::Send(self.next)
        } else {
            Next::Finish(Finish::Write {
                stopped: false,
                gave_up: self.gave_up,
            })
        }
    }

    fn done(&mut self, outcome: Outcome, requests: usize) -> Next {
        self.results.push(outcome);
        self.next += 1;
        self.retried = false;
        self.advance(requests)
    }

    /// 呼び出しの終わり方を受け取る。`requests` は今の呼び出しの数。
    pub(super) fn on(&mut self, ended: Ended, requests: usize) -> Next {
        match ended {
            Ended::Read {
                reading,
                stopped: true,
            } => {
                if let Reading::Complete(p) | Reading::Partial(p) = reading {
                    self.results.push(Outcome::Parsed(p));
                }
                let parsed = self.results.iter().any(|r| matches!(r, Outcome::Parsed(_)));
                Next::Finish(if parsed {
                    Finish::Write {
                        stopped: true,
                        gave_up: self.gave_up,
                    }
                } else {
                    Finish::Stopped
                })
            }
            Ended::Read {
                reading,
                stopped: false,
            } => match reading {
                Reading::Complete(p) | Reading::Partial(p) => {
                    self.done(Outcome::Parsed(p), requests)
                }
                Reading::Truncated => Next::Halve(self.next),
                Reading::Invalid(_) | Reading::Nothing if !self.retried => {
                    self.retried = true;
                    Next::Send(self.next)
                }
                Reading::Invalid(p) => self.done(Outcome::Failed(p), requests),
                Reading::Nothing => self.done(
                    Outcome::Failed(Parsed::failed(InvalidReason::NotJson)),
                    requests,
                ),
            },
            Ended::Transient { count } if count < SERVER_ERROR_LIMIT => Next::Finish(Finish::Retry),
            Ended::Transient { .. } => {
                self.gave_up = true;
                self.done(
                    Outcome::Failed(Parsed::failed(InvalidReason::ServerError)),
                    requests,
                )
            }
            Ended::Rejected => Next::Finish(Finish::Rejected),
            Ended::Unreachable => Next::Finish(Finish::Unreachable),
            Ended::Misconfigured => Next::Finish(Finish::Misconfigured),
        }
    }

    /// 呼び出しを半分に分けた (呼び出しの数は `requests` になった)。分けた前の半分から送る。
    pub(super) fn halved(&mut self, requests: usize) -> Next {
        self.retried = false;
        self.advance(requests)
    }

    /// 1 つの往復の呼び出しで、半分に分けられない (型が合わないときと同じに扱う)。
    pub(super) fn unsplittable(&mut self, requests: usize) -> Next {
        if self.retried {
            self.done(
                Outcome::Failed(Parsed::failed(InvalidReason::Truncated)),
                requests,
            )
        } else {
            self.retried = true;
            Next::Send(self.next)
        }
    }

    pub(super) fn into_results(self) -> Vec<Outcome> {
        self.results
    }
}

/// 書いた・書かなかった区切りが、続けての失敗の数をどう変えるか (04 §5.2.1 の「続けて失敗したとき」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Health {
    /// 型の合った出力がある (数え直す)。
    Answered,
    /// すべての呼び出しが型の合わない出力で `failed` になった (区切りの中身による失敗なので数を変えない)。
    Invalid,
    /// 4xx か、締切切れ・5xx が続いた (数える)。
    Error,
}

/// 続けての失敗の数を更新する。止めるなら真。
pub(super) fn tally(consecutive: &mut u32, health: Health) -> bool {
    match health {
        Health::Answered => *consecutive = 0,
        Health::Invalid => {}
        Health::Error => *consecutive += 1,
    }
    *consecutive >= CONSECUTIVE_LIMIT
}

/// 結果の並びの健康 ([`Finish::Write`] のとき)。
pub(super) fn health(results: &[Outcome], gave_up: bool) -> Health {
    if results.iter().any(|r| matches!(r, Outcome::Parsed(_))) {
        Health::Answered
    } else if gave_up {
        Health::Error
    } else {
        Health::Invalid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evorag_memory::b2::Parsed;

    fn ok() -> Reading {
        Reading::Complete(Parsed::empty())
    }

    fn read(reading: Reading) -> Ended {
        Ended::Read {
            reading,
            stopped: false,
        }
    }

    fn invalid() -> Reading {
        Reading::Invalid(Parsed::failed(InvalidReason::NotJson))
    }

    #[test]
    fn calls_are_sent_in_order_and_written() {
        let mut f = Flow::default();
        assert_eq!(f.start(2), Next::Send(0));
        assert_eq!(f.on(read(ok()), 2), Next::Send(1));
        assert_eq!(
            f.on(read(ok()), 2),
            Next::Finish(Finish::Write {
                stopped: false,
                gave_up: false
            })
        );
        assert_eq!(f.into_results().len(), 2);
        assert_eq!(
            Flow::default().start(0),
            Next::Finish(Finish::Write {
                stopped: false,
                gave_up: false
            }),
            "a segment without numbered sentences is written at once"
        );
    }

    #[test]
    fn an_invalid_output_is_retried_once() {
        let mut f = Flow::default();
        assert_eq!(f.on(read(invalid()), 1), Next::Send(0));
        let done = f.on(read(invalid()), 1);
        assert_eq!(
            done,
            Next::Finish(Finish::Write {
                stopped: false,
                gave_up: false
            })
        );
        let results = f.into_results();
        assert!(matches!(results[..], [Outcome::Failed(_)]));
        assert_eq!(health(&results, false), Health::Invalid);
    }

    #[test]
    fn a_cut_output_is_halved_or_retried_once() {
        let mut f = Flow::default();
        assert_eq!(f.on(read(Reading::Truncated), 1), Next::Halve(0));
        // 分けた: 前の半分から送る (やり直しの回数は戻る)。
        assert_eq!(f.halved(2), Next::Send(0));
        assert_eq!(f.on(read(Reading::Truncated), 2), Next::Halve(0));
        // 分けられない: 1 回だけやり直し、それでも切れたら `truncated`。
        assert_eq!(f.unsplittable(2), Next::Send(0));
        assert_eq!(f.on(read(Reading::Truncated), 2), Next::Halve(0));
        assert_eq!(f.unsplittable(2), Next::Send(1));
        assert!(matches!(f.into_results()[..], [Outcome::Failed(_)]));
    }

    #[test]
    fn a_stop_without_a_parsed_result_writes_nothing() {
        // 分けた呼び出しの 1 つ目が失敗し、2 つ目の途中で止めた。
        let mut f = Flow::default();
        assert_eq!(f.on(read(invalid()), 2), Next::Send(0));
        assert_eq!(f.on(read(invalid()), 2), Next::Send(1));
        assert_eq!(
            f.on(
                Ended::Read {
                    reading: Reading::Nothing,
                    stopped: true
                },
                2
            ),
            Next::Finish(Finish::Stopped)
        );
        // 1 つ目が合っていれば、止めても書く (`partial`)。
        let mut f = Flow::default();
        assert_eq!(f.on(read(ok()), 2), Next::Send(1));
        assert_eq!(
            f.on(
                Ended::Read {
                    reading: Reading::Nothing,
                    stopped: true
                },
                2
            ),
            Next::Finish(Finish::Write {
                stopped: true,
                gave_up: false
            })
        );
    }

    #[test]
    fn server_errors_retry_later_until_the_limit() {
        let mut f = Flow::default();
        assert_eq!(
            f.on(Ended::Transient { count: 1 }, 1),
            Next::Finish(Finish::Retry)
        );
        assert_eq!(
            f.on(Ended::Transient { count: 2 }, 1),
            Next::Finish(Finish::Retry)
        );
        let done = f.on(Ended::Transient { count: 3 }, 1);
        assert_eq!(
            done,
            Next::Finish(Finish::Write {
                stopped: false,
                gave_up: true
            })
        );
        assert_eq!(health(&f.into_results(), true), Health::Error);
    }

    #[test]
    fn rejections_and_misconfiguration_write_nothing() {
        let mut f = Flow::default();
        assert_eq!(f.on(Ended::Rejected, 1), Next::Finish(Finish::Rejected));
        assert_eq!(
            f.on(Ended::Misconfigured, 1),
            Next::Finish(Finish::Misconfigured)
        );
        assert_eq!(
            f.on(Ended::Unreachable, 1),
            Next::Finish(Finish::Unreachable)
        );
    }

    #[test]
    fn consecutive_errors_halt_and_answers_reset_them() {
        let mut n = 0;
        assert!(!tally(&mut n, Health::Error));
        assert!(
            !tally(&mut n, Health::Invalid),
            "invalid outputs do not count"
        );
        assert!(!tally(&mut n, Health::Error));
        assert_eq!(n, 2);
        assert!(!tally(&mut n, Health::Answered));
        assert_eq!(n, 0);
        for _ in 0..2 {
            assert!(!tally(&mut n, Health::Error));
        }
        assert!(tally(&mut n, Health::Error));
    }
}
