//! BLAKE3 による全読み込みハッシュ。

use crate::{Control, Progress};
use backup_core::IO_BUFFER_BYTES;

use std::io::{self, Read};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashOutcome {
    pub hash: String,
    pub size: u64,
}

pub fn hash_reader(reader: &mut dyn Read, progress: Progress<'_>) -> io::Result<HashOutcome> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; IO_BUFFER_BYTES];
    let mut size = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        size += n as u64;
        if progress(n as u64) == Control::Abort {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "aborted"));
        }
    }
    Ok(HashOutcome {
        hash: hasher.finalize().to_hex().to_string(),
        size,
    })
}

pub fn hash_file(path: &Path, progress: Progress<'_>) -> io::Result<HashOutcome> {
    let mut f = open_read_only(path)?;
    crate::drop_page_cache(&f);
    hash_reader(&mut f, progress)
}

pub use crate::platform::open_read_only;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_blake3() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.bin");
        let data: Vec<u8> = (0..10_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let mut seen = 0;
        let out = hash_file(&p, &mut |n| {
            seen += n;
            Control::Continue
        })
        .unwrap();
        assert_eq!(out.size, data.len() as u64);
        assert_eq!(seen, data.len() as u64);
        assert_eq!(out.hash, blake3::hash(&data).to_hex().to_string());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("t");
        std::fs::write(&target, b"x").unwrap();
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(hash_file(&link, &mut crate::no_progress()).is_err());
    }
}
