//! NAS 処理の無応答検知（ウォッチドッグ）。
//!
//! SMB が途中で切れると、NAS 上のファイルへの read/write が OS のタイムアウト（数分〜無期限）まで
//! 戻らないことがある。そのまま待つと NAS ワーカー全体が止まり、再試行もファイル操作も効かなくなる。
//!
//! そこで NAS に触れるブロッキング処理はすべて [`run_watched`] で実行し、
//! - 処理側は区切りごとに [`Watch::tick`] を呼ぶ（進捗の合図）
//! - 監視側は進捗が一定時間止まったら [`Watch::should_abort`] で協調的な中断を求め、
//!   それでも戻らなければ処理を「切り離して」呼び出し元に [`WatchError::Stalled`] を返す
//!
//! 切り離した処理は OS 側で戻った時点で静かに終わる。結果は使わない（NAS へのファイル確定は
//! 読み戻し照合済みの内容だけなので、遅れて確定しても安全。DB は再試行側が改めて検証してから更新する）。

use crate::state::Shared;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 再試行・ユーザー操作の要求時に、この時間以上止まっている処理は待たずに切り離す。
const QUICK_ABANDON: Duration = Duration::from_secs(3);
/// 中断を求めてから、処理が自分で戻るのを待つ時間。
const COOPERATIVE_GRACE: Duration = Duration::from_secs(5);
/// 1 回の呼び出しが長くかかり得る段階（NAS 側のフラッシュなど）では、無応答とみなす時間をこの倍率で延ばす。
const SLOW_STEP_FACTOR: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Finished,
    Detached,
}

pub struct Watch {
    started: Instant,
    last_tick_ms: AtomicU64,
    abort: AtomicBool,
    slow_step: AtomicBool,
    phase: Mutex<Phase>,
    hung_counter: Arc<AtomicUsize>,
}

impl Watch {
    fn new(hung_counter: Arc<AtomicUsize>) -> Self {
        Self {
            started: Instant::now(),
            last_tick_ms: AtomicU64::new(0),
            abort: AtomicBool::new(false),
            slow_step: AtomicBool::new(false),
            phase: Mutex::new(Phase::Running),
            hung_counter,
        }
    }

    /// 進捗の合図（I/O の区切りごとに呼ぶ）。
    pub fn tick(&self) {
        self.last_tick_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::SeqCst);
    }

    /// 途中で進捗を出せない長い 1 ステップ（フラッシュ等）の間だけ true にする。
    /// 無応答とみなすまでの時間が延びる（再試行・ユーザー操作時の早期切り離しはそのまま）。
    pub fn set_slow_step(&self, slow: bool) {
        self.tick();
        self.slow_step.store(slow, Ordering::SeqCst);
    }

    /// 監視側から中断が求められているか。true なら安全な区切りで処理をやめる。
    pub fn should_abort(&self) -> bool {
        self.abort.load(Ordering::SeqCst)
    }

    fn idle(&self) -> Duration {
        let now = self.started.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_tick_ms.load(Ordering::SeqCst)))
    }

    /// 処理スレッドの終了時に呼ばれる。切り離し済みなら未完了数を戻す。
    fn finish(&self) {
        let mut phase = self.phase.lock().unwrap_or_else(|e| e.into_inner());
        if *phase == Phase::Detached {
            self.hung_counter.fetch_sub(1, Ordering::SeqCst);
        }
        *phase = Phase::Finished;
    }

    /// 切り離す。既に終わっていたら false。
    fn detach(&self) -> bool {
        let mut phase = self.phase.lock().unwrap_or_else(|e| e.into_inner());
        if *phase == Phase::Finished {
            return false;
        }
        *phase = Phase::Detached;
        self.hung_counter.fetch_add(1, Ordering::SeqCst);
        true
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("NAS が {0} 秒間応答しないため、処理を切り離して再試行します")]
    Stalled(u64),
    #[error("NAS 処理が異常終了しました: {0}")]
    Panicked(String),
}

/// 処理を中断させる理由があるか（無応答・再試行要求・ユーザー操作待ち・一時停止・終了）。
fn abort_reason(shared: &Shared, watch: &Watch, epoch_at_start: u64) -> Option<Duration> {
    let idle = watch.idle();
    let mut stall = shared.nas_stall_timeout();
    if watch.slow_step.load(Ordering::SeqCst) {
        stall *= SLOW_STEP_FACTOR;
    }
    let retry_requested = shared.nas_retry_epoch.load(Ordering::SeqCst) != epoch_at_start;
    let wants_lock = shared.nas_op_waiting();
    let stopping = shared.is_paused() || shared.shutdown.is_cancelled();
    if idle >= stall || ((retry_requested || wants_lock || stopping) && idle >= QUICK_ABANDON) {
        Some(idle)
    } else {
        None
    }
}

/// NAS に触れるブロッキング処理を無応答検知付きで実行する。
pub async fn run_watched<T, F>(shared: &Arc<Shared>, f: F) -> Result<T, WatchError>
where
    T: Send + 'static,
    F: FnOnce(Arc<Watch>) -> T + Send + 'static,
{
    let watch = Arc::new(Watch::new(shared.nas_hung_ops.clone()));
    let epoch = shared.nas_retry_epoch.load(Ordering::SeqCst);
    let inner = watch.clone();
    let mut handle = tokio::task::spawn_blocking(move || {
        let out = f(inner.clone());
        inner.finish();
        out
    });
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    let result = loop {
        tokio::select! {
            res = &mut handle => break res.map_err(|e| WatchError::Panicked(e.to_string())),
            _ = ticker.tick() => {
                let idle = watch.idle();
                shared.rt().nas.stalled_secs = (idle >= Duration::from_secs(10)).then_some(idle.as_secs());
                let Some(idle) = abort_reason(shared, &watch, epoch) else { continue };
                // まず協調的に止める（チャンク境界で Aborted を返す）
                watch.abort.store(true, Ordering::SeqCst);
                match tokio::time::timeout(COOPERATIVE_GRACE, &mut handle).await {
                    Ok(res) => break res.map_err(|e| WatchError::Panicked(e.to_string())),
                    Err(_) => {
                        if watch.detach() {
                            tracing::warn!(idle_secs = idle.as_secs(), "detached unresponsive NAS operation");
                            break Err(WatchError::Stalled(idle.as_secs()));
                        }
                        // 切り離す直前に終わっていた
                        break (&mut handle).await.map_err(|e| WatchError::Panicked(e.to_string()));
                    }
                }
            }
        }
    };
    shared.rt().nas.stalled_secs = None;
    result
}

/// 切り離したまま戻ってこない NAS 処理の数。
pub fn hung_operations(shared: &Shared) -> usize {
    shared.nas_hung_ops.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_and_finish_keep_counter_balanced() {
        let counter = Arc::new(AtomicUsize::new(0));
        let w = Watch::new(counter.clone());
        assert!(w.detach());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        w.finish();
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        let w = Watch::new(counter.clone());
        w.finish();
        assert!(!w.detach(), "finished work is never counted as hung");
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn idle_resets_on_tick() {
        let w = Watch::new(Arc::new(AtomicUsize::new(0)));
        std::thread::sleep(Duration::from_millis(30));
        assert!(w.idle() >= Duration::from_millis(30));
        w.tick();
        assert!(w.idle() < Duration::from_millis(30));
    }
}
