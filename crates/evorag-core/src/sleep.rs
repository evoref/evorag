//! 裏の処理 (04 §4、FR-301・FR-504)。
//!
//! 手順は 2 種類ある。
//! - **起動後の点検** (モデルの点検 06 §3.3、リランカーの較正 05 §3.7): TUI を開いた後に、入力が途切れ
//!   (`CHECK_QUIET`)、応答を作っていない間に流す。`memory.sleep_time` と `idle_seconds` には縛らない。
//! - **sleep-time の手順** (埋め込み、ノートなど。M2.5 から): 操作の無い時間が `idle` を過ぎてから。
//!
//! どちらも、利用者が操作したら ([`super::Core::touch`]) 動いている手順の future を落とす。LLM の要求は
//! 接続ごと閉じるので、llama-server の処理も止まる (06 §4)。止めたところまでの結果は、作り直せるものだけを
//! 残し、次の機会に続ける。電池で `on_battery = "stop"` のときと、空きが足りないときは、どちらも動かない。
//! 要る手順は状態から求める (記録の有無を見る。job が無いことを「要らない」とみなさない)。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use evorag_types::api::{BackgroundState, Event};

use crate::power::Power;
use crate::{Inner, emit, is_running};

/// 起動後の点検を始めるまでの、入力の途切れ (06 §4 の「入力している間は出さない」)。
const CHECK_QUIET: Duration = Duration::from_secs(2);
/// 終えた直後にまだ要る手順 (結果を保存できなかったなど) を、次に試すまでの間 (要求を送り続けないため)。
const BACKOFF: Duration = Duration::from_secs(300);

/// 電池で動いている間の扱い (07 §4.2 の `memory.on_battery`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnBattery {
    /// 裏の処理は、操作の無い時間が `idle` の 5 倍になってから始める。
    Reduce,
    /// 電池でも変えない。
    Normal,
    /// 裏の処理を止める。
    Stop,
}

impl OnBattery {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "reduce" => Some(Self::Reduce),
            "normal" => Some(Self::Normal),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

/// 裏の処理の設定 (07 §4.2 の `memory.sleep_time`・`idle_seconds`・`on_battery`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepConfig {
    pub enabled: bool,
    pub idle: Duration,
    pub on_battery: OnBattery,
}

impl Default for SleepConfig {
    /// 仕様の既定 (動かす、60 秒、`reduce`)。
    fn default() -> Self {
        Self {
            enabled: true,
            idle: Duration::from_secs(60),
            on_battery: OnBattery::Reduce,
        }
    }
}

impl SleepConfig {
    /// 試験のため: 操作が無ければすぐに動かす。
    pub fn immediate() -> Self {
        Self {
            idle: Duration::ZERO,
            on_battery: OnBattery::Normal,
            ..Self::default()
        }
    }
}

/// 手順 (04 §4 の手順の一覧のうち、この版にあるもの)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Step {
    /// モデルの性質の点検 (06 §3.3)。起動後の点検。
    Probe,
    /// リランクのしきい値の較正 (05 §3.7)。起動後の点検。
    Calibrate,
}

/// 起動後の点検を動かせる状態か。動かせないときは、その理由の状態。
fn check_ready(inner: &Inner) -> Result<(), BackgroundState> {
    if (inner.probe.power)() == Power::Battery && inner.sleep.on_battery == OnBattery::Stop {
        return Err(BackgroundState::PausedBattery);
    }
    if !crate::has_space(inner) {
        return Err(BackgroundState::PausedLowSpace);
    }
    if is_running(inner)
        || inner.activity.held.load(Ordering::SeqCst)
        || inner.activity.since() < CHECK_QUIET.min(inner.sleep.idle)
    {
        return Err(BackgroundState::Idle);
    }
    Ok(())
}

/// 要る起動後の点検 (状態から求める)。`skip` は、待たせている手順。
fn next_check(inner: &Inner, skip: &HashMap<Step, Instant>) -> Option<Step> {
    if !inner.background_checks {
        return None;
    }
    let waiting = |s: Step| skip.get(&s).is_some_and(|t| t.elapsed() < BACKOFF);
    if !waiting(Step::Probe) && crate::needs_probe(inner) {
        return Some(Step::Probe);
    }
    if !waiting(Step::Calibrate) && crate::needs_calibration(inner) {
        return Some(Step::Calibrate);
    }
    None
}

/// 要る手順があるか (状態の行の「較正中」と、試験の待ち合わせ)。
pub(crate) fn pending(inner: &Inner) -> bool {
    next_check(inner, &HashMap::new()).is_some()
}

/// 起動後の点検が止められていないか (電池で `stop`)。
pub(crate) fn checks_allowed(inner: &Inner) -> bool {
    !((inner.probe.power)() == Power::Battery && inner.sleep.on_battery == OnBattery::Stop)
}

fn set_state(inner: &Inner, state: BackgroundState) {
    let mut cur = inner
        .background_state
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if *cur != state {
        *cur = state;
        drop(cur);
        emit(inner, Event::BackgroundState { state });
    }
}

/// 待っているときの状態 (sleep-time の手順はまだ無いので、設定で切っていれば「切っている」)。
fn resting(inner: &Inner) -> BackgroundState {
    if inner.sleep.enabled {
        BackgroundState::Idle
    } else {
        BackgroundState::Off
    }
}

/// 裏の処理を回す。終了のときに止める (task を abort して待つ)。
pub(crate) async fn run(inner: Arc<Inner>) {
    let mut backoff: HashMap<Step, Instant> = HashMap::new();
    loop {
        // 操作の世代を、動かすと決める前に覚える (決めた後の操作を取りこぼさないため)。
        let generation = inner.activity.generation();
        match check_ready(&inner) {
            Ok(()) => match next_check(&inner, &backoff) {
                Some(step) => {
                    set_state(&inner, BackgroundState::Working);
                    let done = run_step(&inner, step, generation).await;
                    tracing::info!(?step, done, "background step");
                    set_state(&inner, resting(&inner));
                    if done {
                        // 終えたのにまだ要る (保存できなかったなど) なら、しばらく待たせる。
                        if next_check(&inner, &backoff) == Some(step) {
                            tracing::warn!(?step, "background step still needed; backing off");
                            backoff.insert(step, Instant::now());
                        }
                        continue;
                    }
                }
                None => set_state(&inner, resting(&inner)),
            },
            Err(BackgroundState::Idle) => set_state(&inner, resting(&inner)),
            Err(state) => set_state(&inner, state),
        }
        // 1 秒ごとに見直す。利用者の操作と、役割の変化 (`kick`) でも見直す。
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
            () = inner.activity.kicked() => {}
        }
    }
}

/// 手順を 1 つ動かす。利用者が操作したら future を落として止め、`false` を返す。
async fn run_step(inner: &Arc<Inner>, step: Step, generation: u64) -> bool {
    let touched = inner.activity.touched_notify().notified();
    tokio::pin!(touched);
    // 待つ準備をしてから、世代が変わっていないかを確かめる (その間の操作を取りこぼさない)。
    touched.as_mut().enable();
    if inner.activity.generation() != generation {
        return false;
    }
    inner.working.store(true, Ordering::SeqCst);
    let work = async {
        match step {
            Step::Probe => crate::probe_unprobed(inner).await,
            Step::Calibrate => crate::calibrate_now(inner).await,
        }
    };
    let done = tokio::select! {
        () = work => true,
        () = touched => false,
    };
    inner.working.store(false, Ordering::SeqCst);
    done
}
