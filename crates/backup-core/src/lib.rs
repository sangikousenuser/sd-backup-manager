//! SD Backup Manager のドメインモデル。
//!
//! I/O を伴わない純粋なロジック（状態遷移・分類・識別・パス安全化・マニフェスト・
//! 容量/削除ポリシー）をここに集め、単体テストで安全性を担保する。

pub mod classifier;
pub mod identity;
pub mod manifest;
pub mod paths;
pub mod policy;
pub mod session;
pub mod state;

pub use classifier::{Classification, FormatDefinitions, MediaKind};
pub use identity::{Confidence, MediaDescriptor, PartitionDescriptor};
pub use state::{NasFileState, SessionState, StageState};

/// 1 ファイルあたりの合計最大試行回数（初回を含む）。FR-023。
pub const MAX_COPY_ATTEMPTS: u32 = 3;

/// SSD に常に残す最低空き容量（10進 20GB）。
pub const DEFAULT_RESERVE_FREE_BYTES: u64 = 20_000_000_000;

/// コピー/ハッシュ用固定長バッファ（4 MiB）。
pub const IO_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// アプリのバージョン（マニフェスト等へ記録）。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// ファイル名に使える 2 桁パーティション表記（P01, P02 ...）。
pub fn partition_label(index: u32) -> String {
    format!("P{index:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_labels() {
        assert_eq!(partition_label(1), "P01");
        assert_eq!(partition_label(12), "P12");
        assert_eq!(partition_label(123), "P123");
    }
}
