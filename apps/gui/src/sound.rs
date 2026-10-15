//! 通知音（FR-033、FR-036）。色だけに頼らず、成功・注意・警告を区別できる音を鳴らす。
//! 音声ファイルは同梱せず、起動時に簡単な WAV を生成して PulseAudio/PipeWire で再生する。

use backup_ipc::NotificationKind;
use std::path::PathBuf;

const RATE: u32 = 44_100;

/// (周波数Hz, 長さms) の並び。0Hz は無音。
fn pattern(kind: NotificationKind) -> &'static [(f32, u32)] {
    match kind {
        // 上昇 2 音
        NotificationKind::Success => &[(880.0, 140), (0.0, 40), (1318.5, 220)],
        // 同じ高さの 3 回
        NotificationKind::Warning => &[
            (660.0, 160),
            (0.0, 90),
            (660.0, 160),
            (0.0, 90),
            (660.0, 160),
        ],
        // 低い長音 2 回
        NotificationKind::Error => &[(330.0, 450), (0.0, 120), (247.0, 600)],
        // 柔らかい 1 音
        NotificationKind::NasComplete => &[(1046.5, 180)],
    }
}

fn wav_bytes(kind: NotificationKind) -> Vec<u8> {
    let mut samples: Vec<i16> = Vec::new();
    for (freq, ms) in pattern(kind) {
        let n = (RATE as u64 * *ms as u64 / 1000) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            // 短いフェードでクリックノイズを防ぐ
            let env = ((i.min(n - i) as f32) / (RATE as f32 * 0.01)).min(1.0);
            let v = if *freq > 0.0 {
                (t * freq * std::f32::consts::TAU).sin() * 0.35 * env
            } else {
                0.0
            };
            samples.push((v * i16::MAX as f32) as i16);
        }
    }
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn sound_file(kind: NotificationKind) -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("sd-backup-manager-gui");
    std::fs::create_dir_all(&dir).ok()?;
    let name = match kind {
        NotificationKind::Success => "success.wav",
        NotificationKind::Warning => "warning.wav",
        NotificationKind::Error => "error.wav",
        NotificationKind::NasComplete => "nas-complete.wav",
    };
    let path = dir.join(name);
    if !path.exists() {
        std::fs::write(&path, wav_bytes(kind)).ok()?;
    }
    Some(path)
}

pub fn play(kind: NotificationKind) {
    let Some(path) = sound_file(kind) else { return };
    std::thread::spawn(move || {
        for player in ["paplay", "pw-play", "aplay", "afplay"] {
            let mut cmd = std::process::Command::new(player);
            if player == "aplay" {
                cmd.arg("-q");
            }
            if let Ok(status) = cmd
                .arg(&path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                && status.success()
            {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_valid_and_sounds_differ() {
        let a = wav_bytes(NotificationKind::Success);
        let b = wav_bytes(NotificationKind::Warning);
        assert_eq!(&a[0..4], b"RIFF");
        assert_eq!(&a[8..12], b"WAVE");
        assert_ne!(a.len(), b.len());
    }
}
