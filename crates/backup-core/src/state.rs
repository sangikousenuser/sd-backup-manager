//! セッション・ファイルの状態と許可された状態遷移（設計書 9 章）。

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = UnknownState;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok($name::$variant),)+
                    other => Err(UnknownState(other.to_string())),
                }
            }
        }
    };
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown state: {0}")]
pub struct UnknownState(pub String);

string_enum!(
    /// セッション全体の状態（9.1 の状態機械）。
    SessionState {
        Detected => "DETECTED",
        Scanning => "SCANNING",
        Blocked => "BLOCKED",
        Copying => "COPYING",
        SourceInterrupted => "SOURCE_INTERRUPTED",
        SourceFinalizing => "SOURCE_FINALIZING",
        Unmounting => "UNMOUNTING",
        MediaGreen => "MEDIA_GREEN",
        MediaYellow => "MEDIA_YELLOW",
        UnmountFailed => "UNMOUNT_FAILED",
        NasPending => "NAS_PENDING",
        NeedsAttention => "NEEDS_ATTENTION",
        NasVerified => "NAS_VERIFIED",
        StagingCleanup => "STAGING_CLEANUP",
        Complete => "COMPLETE",
    }
);

impl SessionState {
    /// 設計書 9.1 の遷移に、実装上必要な安全側の遷移を加えたもの。
    ///
    /// 追加した遷移:
    /// - 走査前・走査中・待機中の抜去 / デーモン停止 → `SOURCE_INTERRUPTED`
    /// - 再挿入時は不足分確認のため `SOURCE_INTERRUPTED`/`NEEDS_ATTENTION` → `SCANNING`
    /// - `UNMOUNT_FAILED` からの再アンマウント、または物理抜去後の NAS 処理継続
    pub fn can_transition_to(self, next: SessionState) -> bool {
        use SessionState::*;
        matches!(
            (self, next),
            (Detected, Scanning)
                | (Detected, Blocked)
                | (Detected, SourceInterrupted)
                | (Scanning, Copying)
                | (Scanning, Blocked)
                | (Scanning, SourceInterrupted)
                | (Blocked, Scanning)
                | (Blocked, Copying)
                | (Blocked, SourceInterrupted)
                | (Copying, Blocked)
                | (Copying, SourceInterrupted)
                | (Copying, SourceFinalizing)
                | (SourceInterrupted, Copying)
                | (SourceInterrupted, Scanning)
                | (SourceFinalizing, Unmounting)
                | (SourceFinalizing, SourceInterrupted)
                | (Unmounting, MediaGreen)
                | (Unmounting, MediaYellow)
                | (Unmounting, UnmountFailed)
                | (Unmounting, SourceInterrupted)
                | (UnmountFailed, Unmounting)
                | (UnmountFailed, NasPending)
                | (UnmountFailed, NeedsAttention)
                | (MediaGreen, NasPending)
                | (MediaYellow, NeedsAttention)
                | (NasPending, NasVerified)
                | (NasPending, NeedsAttention)
                | (NasVerified, StagingCleanup)
                | (NasVerified, NasPending)
                | (StagingCleanup, Complete)
                | (StagingCleanup, NasPending)
                | (NeedsAttention, Copying)
                | (NeedsAttention, Scanning)
        )
    }

    /// メディアを読み込み中（物理メディアが必要）の状態か。
    pub fn is_media_active(self) -> bool {
        use SessionState::*;
        matches!(
            self,
            Detected | Scanning | Blocked | Copying | SourceFinalizing | Unmounting
        )
    }

    /// 再挿入時に同じセッション（同じ日時フォルダ）を再利用すべき状態か。
    pub fn is_resumable(self) -> bool {
        use SessionState::*;
        matches!(self, SourceInterrupted | NeedsAttention | UnmountFailed)
    }

    /// 元メディアからの読み込みが完了している状態か。
    pub fn is_media_done(self) -> bool {
        use SessionState::*;
        matches!(
            self,
            MediaGreen
                | MediaYellow
                | UnmountFailed
                | NasPending
                | NeedsAttention
                | NasVerified
                | StagingCleanup
                | Complete
        )
    }
}

string_enum!(
    /// SSD 側のファイル状態（9.2）。
    StageState {
        Discovered => "DISCOVERED",
        SkippedVerified => "SKIPPED_VERIFIED",
        Copying => "COPYING",
        Verifying => "VERIFYING",
        SsdVerified => "SSD_VERIFIED",
        FailedRetryable => "FAILED_RETRYABLE",
        FailedTerminal => "FAILED_TERMINAL",
        SourceRemoved => "SOURCE_REMOVED",
    }
);

impl StageState {
    /// SSD への取り込みとして完了扱いできる状態。
    pub fn is_done(self) -> bool {
        matches!(self, StageState::SsdVerified | StageState::SkippedVerified)
    }

    /// まだコピーを試みるべき状態。
    pub fn needs_copy(self) -> bool {
        matches!(
            self,
            StageState::Discovered
                | StageState::Copying
                | StageState::Verifying
                | StageState::FailedRetryable
                | StageState::SourceRemoved
        )
    }
}

string_enum!(
    /// NAS 側のファイル状態（9.2）。
    NasFileState {
        NotReady => "NOT_READY",
        Queued => "QUEUED",
        Uploading => "UPLOADING",
        Verifying => "VERIFYING",
        NasVerified => "NAS_VERIFIED",
        RetryWait => "RETRY_WAIT",
        NasFailed => "NAS_FAILED",
    }
);

string_enum!(
    /// セッション単位の NAS 進捗（表示用の 2 軸目）。
    SessionNasState {
        Waiting => "WAITING",
        InProgress => "IN_PROGRESS",
        RetryWait => "RETRY_WAIT",
        Verified => "VERIFIED",
        Failed => "FAILED",
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_strings() {
        for s in SessionState::ALL {
            assert_eq!(SessionState::from_str(s.as_str()).unwrap(), *s);
        }
        for s in StageState::ALL {
            assert_eq!(StageState::from_str(s.as_str()).unwrap(), *s);
        }
        for s in NasFileState::ALL {
            assert_eq!(NasFileState::from_str(s.as_str()).unwrap(), *s);
        }
        assert!(SessionState::from_str("BOGUS").is_err());
    }

    #[test]
    fn happy_path_transitions_are_allowed() {
        use SessionState::*;
        let path = [
            Detected,
            Scanning,
            Copying,
            SourceFinalizing,
            Unmounting,
            MediaGreen,
            NasPending,
            NasVerified,
            StagingCleanup,
            Complete,
        ];
        for pair in path.windows(2) {
            assert!(
                pair[0].can_transition_to(pair[1]),
                "{:?} -> {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn dangerous_transitions_are_rejected() {
        use SessionState::*;
        // 失敗が残るセッションは NAS 完了・削除へ進めない
        assert!(!MediaYellow.can_transition_to(NasPending));
        assert!(!NeedsAttention.can_transition_to(NasVerified));
        assert!(!NeedsAttention.can_transition_to(StagingCleanup));
        // アンマウント失敗から直接緑にはならない
        assert!(!UnmountFailed.can_transition_to(MediaGreen));
        // コピー中から完了へ飛ばない
        assert!(!Copying.can_transition_to(Complete));
        assert!(!Complete.can_transition_to(Copying));
        assert!(!NasPending.can_transition_to(StagingCleanup));
    }
}
