//! ソケットのアクセス制御（6.4）: ディレクトリ 0700・ソケット 0600 に加えて、
//! 接続ごとにピアの UID（SO_PEERCRED / getpeereid）を確認する。

use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use tokio::net::{UnixListener, UnixStream};

/// 相手プロセスが自分と同じ UID か確認する。
pub fn verify_peer(stream: &UnixStream) -> io::Result<()> {
    let cred = stream.peer_cred()?;
    let me = nix::unistd::geteuid().as_raw();
    if cred.uid() != me {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("peer uid {} does not match {}", cred.uid(), me),
        ));
    }
    Ok(())
}

/// ソケットを作成する。古いソケットファイルは「ソケットであり、誰も待ち受けていない」場合のみ削除。
pub async fn bind_socket(path: &Path) -> io::Result<UnixListener> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        if UnixStream::connect(path).await.is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another daemon is already running",
            ));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_and_verify_same_uid() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = bind_socket(&sock).await.unwrap();
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let (accepted, client) = tokio::join!(listener.accept(), UnixStream::connect(&sock));
        let server = accepted.unwrap().0;
        let client = client.unwrap();
        verify_peer(&client).unwrap();
        verify_peer(&server).unwrap();
        // 稼働中のソケットは奪わない
        assert!(bind_socket(&sock).await.is_err());
    }

    #[tokio::test]
    async fn refuses_non_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("file");
        std::fs::write(&p, b"x").unwrap();
        assert!(bind_socket(&p).await.is_err());
        assert!(p.exists());
    }
}
