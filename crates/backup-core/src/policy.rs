//! 安全ポリシー: 容量アドミッション（5.2）・SSD 自動削除ガード（9.4）・
//! 再試行バックオフ（FR-028）・速度/残り時間推定（FR-024）。

use crate::state::{NasFileState, SessionState, StageState};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 容量アドミッション
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityInput {
    /// ステージング FS の空き（statvfs の f_bavail × f_frsize）。取得不可なら None。
    pub free_bytes: Option<u64>,
    /// 最低確保空き容量（既定 20GB）。
    pub reserve_bytes: u64,
    /// このセッションでこれから書き込むバイト数（スキップ分を除く）。
    pub required_bytes: u64,
    /// 追加安全マージン（最大ファイルサイズ + FS 予約見込み等）。
    pub margin_bytes: u64,
    /// 既に開始済みの他セッションがこれから書き込む残りバイト数。
    pub reserved_by_others: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Admit,
    /// 足りないバイト数。
    Wait {
        shortfall: u64,
    },
    /// 空き容量を取得できないため開始しない（安全側）。
    Unknown,
}

pub fn admit(input: CapacityInput) -> Admission {
    let Some(free) = input.free_bytes else {
        return Admission::Unknown;
    };
    let needed = input
        .reserve_bytes
        .saturating_add(input.required_bytes)
        .saturating_add(input.margin_bytes)
        .saturating_add(input.reserved_by_others);
    if free >= needed {
        Admission::Admit
    } else {
        Admission::Wait {
            shortfall: needed - free,
        }
    }
}

/// コピー途中のファイル単位チェック: 書き込み後も最低空き容量を割らないか。
pub fn can_write_next(
    free_bytes: Option<u64>,
    reserve_bytes: u64,
    next_file_bytes: u64,
    margin: u64,
) -> bool {
    match free_bytes {
        Some(free) => {
            free >= reserve_bytes
                .saturating_add(next_file_bytes)
                .saturating_add(margin)
        }
        None => false,
    }
}

/// 追加安全マージン: 最大ファイルサイズ + 容量の 1%（FS メタデータ・予約分の見込み）、最低 256MB。
pub fn safety_margin(max_file_bytes: u64, required_bytes: u64) -> u64 {
    max_file_bytes
        .saturating_add(required_bytes / 100)
        .max(256 * 1024 * 1024)
}

// ---------------------------------------------------------------------------
// SSD 自動削除ガード（9.4）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FileGuardInfo {
    pub stage_state: StageState,
    pub nas_state: NasFileState,
    /// SKIPPED_VERIFIED の場合、過去実体の参照が検証済みか。
    pub reference_verified: bool,
}

#[derive(Debug, Clone)]
pub struct DeletionGuardInput {
    pub session_state: SessionState,
    pub scan_completed: bool,
    pub files: Vec<FileGuardInfo>,
    pub manifest_verified_on_nas: bool,
    pub nas_target_verified: bool,
    pub pending_transfers: usize,
    pub open_handles: usize,
    pub path_inside_staging_root: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardViolation {
    WrongSessionState(SessionState),
    ScanNotCompleted,
    UnresolvedFile { index: usize, stage: StageState },
    NasNotVerified { index: usize, nas: NasFileState },
    ReferenceNotVerified { index: usize },
    ManifestNotVerified,
    NasTargetNotVerified,
    PendingTransfers(usize),
    OpenHandles(usize),
    OutsideStagingRoot,
}

/// すべての条件が真のときだけ `Ok(())`。1 つでも欠ければ削除禁止。
pub fn check_staging_deletion(input: &DeletionGuardInput) -> Result<(), Vec<GuardViolation>> {
    let mut v = Vec::new();
    if !matches!(
        input.session_state,
        SessionState::NasVerified | SessionState::StagingCleanup
    ) {
        v.push(GuardViolation::WrongSessionState(input.session_state));
    }
    if !input.scan_completed {
        v.push(GuardViolation::ScanNotCompleted);
    }
    for (index, f) in input.files.iter().enumerate() {
        match f.stage_state {
            StageState::SsdVerified => {
                if f.nas_state != NasFileState::NasVerified {
                    v.push(GuardViolation::NasNotVerified {
                        index,
                        nas: f.nas_state,
                    });
                }
            }
            StageState::SkippedVerified => {
                if !f.reference_verified {
                    v.push(GuardViolation::ReferenceNotVerified { index });
                }
            }
            other => v.push(GuardViolation::UnresolvedFile {
                index,
                stage: other,
            }),
        }
    }
    if !input.manifest_verified_on_nas {
        v.push(GuardViolation::ManifestNotVerified);
    }
    if !input.nas_target_verified {
        v.push(GuardViolation::NasTargetNotVerified);
    }
    if input.pending_transfers > 0 {
        v.push(GuardViolation::PendingTransfers(input.pending_transfers));
    }
    if input.open_handles > 0 {
        v.push(GuardViolation::OpenHandles(input.open_handles));
    }
    if !input.path_inside_staging_root {
        v.push(GuardViolation::OutsideStagingRoot);
    }
    if v.is_empty() { Ok(()) } else { Err(v) }
}

// ---------------------------------------------------------------------------
// バックオフ
// ---------------------------------------------------------------------------

/// 指数バックオフ + ジッタ。`attempt` は 1 始まり、`jitter` は [0, 1) の乱数。
/// 上限 `max` まで増加し、無期限に再試行する（FR-028）。
pub fn backoff_delay(attempt: u32, base: Duration, max: Duration, jitter: f64) -> Duration {
    let exp = attempt.saturating_sub(1).min(20);
    let raw = base.saturating_mul(1u32 << exp.min(16));
    let capped = raw.min(max);
    let jitter = jitter.clamp(0.0, 1.0);
    // ±20% のジッタ
    let factor = 0.8 + 0.4 * jitter;
    Duration::from_secs_f64((capped.as_secs_f64() * factor).max(0.5))
}

// ---------------------------------------------------------------------------
// 速度・残り時間
// ---------------------------------------------------------------------------

/// 直近の移動平均で転送速度を推定する。計測が不十分なら None（「計算中」）。
#[derive(Debug, Clone)]
pub struct RateEstimator {
    window: Duration,
    samples: VecDeque<(Instant, u64)>,
}

impl RateEstimator {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            samples: VecDeque::new(),
        }
    }

    /// 累積転送バイト数を記録する。
    pub fn record(&mut self, now: Instant, total_bytes: u64) {
        self.samples.push_back((now, total_bytes));
        while let Some((t, _)) = self.samples.front() {
            if now.duration_since(*t) > self.window && self.samples.len() > 2 {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn reset(&mut self) {
        self.samples.clear();
    }

    /// bytes/sec。
    pub fn rate(&self) -> Option<f64> {
        let (t0, b0) = *self.samples.front()?;
        let (t1, b1) = *self.samples.back()?;
        let dt = t1.duration_since(t0).as_secs_f64();
        if dt < 2.0 || b1 < b0 {
            return None;
        }
        let r = (b1 - b0) as f64 / dt;
        if r <= 0.0 { None } else { Some(r) }
    }

    pub fn eta(&self, remaining_bytes: u64) -> Option<Duration> {
        let r = self.rate()?;
        let secs = remaining_bytes as f64 / r;
        if !secs.is_finite() || secs > 7.0 * 24.0 * 3600.0 {
            return None;
        }
        Some(Duration::from_secs_f64(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    #[test]
    fn admission_respects_reserve() {
        let base = CapacityInput {
            free_bytes: Some(200 * GB),
            reserve_bytes: 20 * GB,
            required_bytes: 128 * GB,
            margin_bytes: 4 * GB,
            reserved_by_others: 0,
        };
        assert_eq!(admit(base), Admission::Admit);
        // 2 枚目（1 枚目が 128GB 予約済み）は待機
        let second = CapacityInput {
            reserved_by_others: 128 * GB,
            ..base
        };
        assert_eq!(admit(second), Admission::Wait { shortfall: 80 * GB });
        // ちょうど境界
        let edge = CapacityInput {
            free_bytes: Some(152 * GB),
            ..base
        };
        assert_eq!(admit(edge), Admission::Admit);
        let below = CapacityInput {
            free_bytes: Some(152 * GB - 1),
            ..base
        };
        assert_eq!(admit(below), Admission::Wait { shortfall: 1 });
        // 取得不可は開始しない
        assert_eq!(
            admit(CapacityInput {
                free_bytes: None,
                ..base
            }),
            Admission::Unknown
        );
    }

    #[test]
    fn per_file_check() {
        assert!(can_write_next(Some(25 * GB), 20 * GB, 4 * GB, GB / 2));
        assert!(!can_write_next(Some(24 * GB), 20 * GB, 4 * GB, GB / 2));
        assert!(!can_write_next(None, 20 * GB, 1, 0));
    }

    fn ok_input() -> DeletionGuardInput {
        DeletionGuardInput {
            session_state: SessionState::StagingCleanup,
            scan_completed: true,
            files: vec![
                FileGuardInfo {
                    stage_state: StageState::SsdVerified,
                    nas_state: NasFileState::NasVerified,
                    reference_verified: false,
                },
                FileGuardInfo {
                    stage_state: StageState::SkippedVerified,
                    nas_state: NasFileState::NotReady,
                    reference_verified: true,
                },
            ],
            manifest_verified_on_nas: true,
            nas_target_verified: true,
            pending_transfers: 0,
            open_handles: 0,
            path_inside_staging_root: true,
        }
    }

    #[test]
    fn deletion_allowed_only_when_everything_verified() {
        assert!(check_staging_deletion(&ok_input()).is_ok());

        let mut i = ok_input();
        i.files[0].nas_state = NasFileState::RetryWait;
        assert!(check_staging_deletion(&i).is_err());

        let mut i = ok_input();
        i.files[0].stage_state = StageState::FailedTerminal;
        assert!(check_staging_deletion(&i).is_err());

        let mut i = ok_input();
        i.files[1].reference_verified = false;
        assert!(check_staging_deletion(&i).is_err());

        let mut i = ok_input();
        i.manifest_verified_on_nas = false;
        assert_eq!(
            check_staging_deletion(&i),
            Err(vec![GuardViolation::ManifestNotVerified])
        );

        let mut i = ok_input();
        i.session_state = SessionState::NeedsAttention;
        assert!(check_staging_deletion(&i).is_err());

        let mut i = ok_input();
        i.pending_transfers = 1;
        i.open_handles = 2;
        i.path_inside_staging_root = false;
        i.nas_target_verified = false;
        i.scan_completed = false;
        assert_eq!(check_staging_deletion(&i).unwrap_err().len(), 5);
    }

    #[test]
    fn backoff_grows_and_caps() {
        let base = Duration::from_secs(5);
        let max = Duration::from_secs(300);
        let d1 = backoff_delay(1, base, max, 0.5);
        let d3 = backoff_delay(3, base, max, 0.5);
        let d50 = backoff_delay(50, base, max, 0.5);
        assert_eq!(d1, Duration::from_secs(5));
        assert_eq!(d3, Duration::from_secs(20));
        assert_eq!(d50, max);
        assert!(backoff_delay(50, base, max, 0.99) <= max.mul_f64(1.2));
        assert!(backoff_delay(1, base, max, 0.0) >= Duration::from_secs(4));
    }

    #[test]
    fn rate_needs_enough_samples() {
        let t0 = Instant::now();
        let mut r = RateEstimator::new(Duration::from_secs(10));
        assert!(r.rate().is_none());
        r.record(t0, 0);
        r.record(t0 + Duration::from_secs(1), 100);
        assert!(r.rate().is_none(), "2 秒未満は計算中");
        r.record(t0 + Duration::from_secs(4), 400);
        assert_eq!(r.rate(), Some(100.0));
        assert_eq!(r.eta(1000), Some(Duration::from_secs(10)));
    }
}
