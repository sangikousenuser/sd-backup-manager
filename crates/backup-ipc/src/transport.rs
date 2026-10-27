//! IPC の接続方式（6.4）。
//!
//! - Linux/macOS: Unix Domain Socket。ディレクトリ 0700・ソケット 0600 に加え、接続ごとに
//!   ピアの UID（SO_PEERCRED / getpeereid）を確認する。
//! - Windows: 名前付きパイプ `\\.\pipe\sd-backup-manager-<id>`。リモート接続を拒否し、
//!   `first_pipe_instance` で同名パイプの先取り（なりすまし）を検出する。パイプの既定 DACL は
//!   作成者・SYSTEM・Administrators にのみ書き込みを許すため、他のユーザーはコマンドを送れない。

use std::io;
use std::path::Path;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};

pub type Reader = BufReader<Pin<Box<dyn AsyncRead + Send>>>;
pub type Writer = Pin<Box<dyn AsyncWrite + Send>>;

fn split<S: AsyncRead + AsyncWrite + Send + 'static>(stream: S) -> (Reader, Writer) {
    let (r, w) = tokio::io::split(stream);
    (BufReader::new(Box::pin(r)), Box::pin(w))
}

// ---------------------------------------------------------------------------
// Unix
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
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

    pub async fn connect(path: &Path) -> io::Result<(Reader, Writer)> {
        let stream = UnixStream::connect(path).await?;
        verify_peer(&stream)?;
        Ok(split(stream))
    }

    pub struct Listener {
        inner: UnixListener,
    }

    impl Listener {
        /// 古いソケットファイルは「ソケットであり、誰も待ち受けていない」場合のみ削除。
        pub async fn bind(path: &Path) -> io::Result<Self> {
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
            let inner = UnixListener::bind(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self { inner })
        }

        pub async fn accept(&mut self) -> io::Result<(Reader, Writer)> {
            let (stream, _) = self.inner.accept().await?;
            verify_peer(&stream)?;
            Ok(split(stream))
        }
    }

    pub fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use std::time::Duration;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

    const ERROR_PIPE_BUSY: i32 = 231;

    pub async fn connect(path: &Path) -> io::Result<(Reader, Writer)> {
        let name = path.as_os_str();
        for _ in 0..50 {
            match ClientOptions::new().open(name) {
                Ok(client) => return Ok(split(client)),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "named pipe is busy",
        ))
    }

    pub struct Listener {
        name: std::ffi::OsString,
        next: NamedPipeServer,
    }

    impl Listener {
        pub async fn bind(path: &Path) -> io::Result<Self> {
            let name = path.as_os_str().to_owned();
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)
                .map_err(|e| {
                    if e.kind() == io::ErrorKind::PermissionDenied {
                        io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "another daemon is already running (pipe exists)",
                        )
                    } else {
                        e
                    }
                })?;
            Ok(Self { name, next })
        }

        pub async fn accept(&mut self) -> io::Result<(Reader, Writer)> {
            self.next.connect().await?;
            let fresh = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)?;
            let connected = std::mem::replace(&mut self.next, fresh);
            Ok(split(connected))
        }
    }

    pub fn cleanup(_path: &Path) {}
}

pub use imp::{Listener, cleanup, connect};

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    fn endpoint(dir: &Path) -> std::path::PathBuf {
        #[cfg(windows)]
        {
            let _ = dir;
            std::path::PathBuf::from(format!(
                r"\\.\pipe\sdbm-test-{}",
                std::process::id() as u64 * 1000 + rand_suffix()
            ))
        }
        #[cfg(not(windows))]
        {
            dir.join("d.sock")
        }
    }

    #[cfg(windows)]
    fn rand_suffix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64
            % 1000
    }

    #[tokio::test]
    async fn roundtrip_and_single_instance() {
        let dir = tempfile::tempdir().unwrap();
        let ep = endpoint(dir.path());
        let mut listener = Listener::bind(&ep).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&ep).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let (accepted, client) = tokio::join!(listener.accept(), connect(&ep));
        let (mut sr, mut sw) = accepted.unwrap();
        let (mut cr, mut cw) = client.unwrap();
        cw.write_all(b"ping\n").await.unwrap();
        cw.flush().await.unwrap();
        let mut line = String::new();
        sr.read_line(&mut line).await.unwrap();
        assert_eq!(line, "ping\n");
        sw.write_all(b"pong\n").await.unwrap();
        sw.flush().await.unwrap();
        line.clear();
        cr.read_line(&mut line).await.unwrap();
        assert_eq!(line, "pong\n");
        // 稼働中のエンドポイントは奪わない
        assert!(Listener::bind(&ep).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_non_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("file");
        std::fs::write(&p, b"x").unwrap();
        assert!(Listener::bind(&p).await.is_err());
        assert!(p.exists());
    }
}
