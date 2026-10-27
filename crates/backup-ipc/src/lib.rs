//! デーモンと GUI/CLI 間の IPC。
//! Linux/macOS: Unix Domain Socket（同一 UID のみ）、Windows: 名前付きパイプ（ローカルのみ）。

pub mod protocol;
pub mod transport;

pub use protocol::*;

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
pub use transport::{Reader, Writer};

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("デーモンに接続できません: {0}")]
    Connect(std::io::Error),
    #[error("通信エラー: {0}")]
    Io(#[from] std::io::Error),
    #[error("メッセージが不正です: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("メッセージが大きすぎます")]
    TooLarge,
    #[error("接続が閉じられました")]
    Closed,
    #[error("プロトコルのバージョンが一致しません (daemon {daemon}, client {client})")]
    Version { daemon: u32, client: u32 },
    #[error("{0}")]
    Remote(String),
    #[error("予期しない応答です")]
    Unexpected,
}

/// 長さ制限付きで 1 行読む。EOF なら None。
pub async fn read_line_limited<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, IpcError> {
    let mut line = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(IpcError::Closed)
            };
        }
        if let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            line.extend_from_slice(&buf[..pos]);
            reader.consume(pos + 1);
            if line.len() > MAX_LINE_BYTES {
                return Err(IpcError::TooLarge);
            }
            return Ok(Some(line));
        }
        let n = buf.len();
        line.extend_from_slice(buf);
        reader.consume(n);
        if line.len() > MAX_LINE_BYTES {
            return Err(IpcError::TooLarge);
        }
    }
}

pub async fn read_message<T: DeserializeOwned, R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<T>, IpcError> {
    match read_line_limited(reader).await? {
        Some(line) => Ok(Some(serde_json::from_slice(&line)?)),
        None => Ok(None),
    }
}

pub async fn write_message<T: Serialize, W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &T,
) -> Result<(), IpcError> {
    let mut bytes = serde_json::to_vec(msg)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// クライアント側の接続。
pub struct Client {
    reader: Reader,
    writer: Writer,
    next_id: u64,
    pub daemon_version: String,
}

impl Client {
    pub async fn connect(socket: &Path, client_name: &str) -> Result<Self, IpcError> {
        let (reader, writer) = transport::connect(socket)
            .await
            .map_err(IpcError::Connect)?;
        let mut c = Client {
            reader,
            writer,
            next_id: 1,
            daemon_version: String::new(),
        };
        write_message(
            &mut c.writer,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                client: client_name.to_string(),
            },
        )
        .await?;
        match read_message::<ServerMessage, _>(&mut c.reader).await? {
            Some(ServerMessage::Hello {
                protocol_version,
                daemon_version,
            }) => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(IpcError::Version {
                        daemon: protocol_version,
                        client: PROTOCOL_VERSION,
                    });
                }
                c.daemon_version = daemon_version;
                Ok(c)
            }
            Some(_) => Err(IpcError::Unexpected),
            None => Err(IpcError::Closed),
        }
    }

    /// コマンドを送り、対応する応答を待つ（途中のスナップショットは読み捨てる）。
    pub async fn request(&mut self, command: Command) -> Result<Response, IpcError> {
        let id = self.next_id;
        self.next_id += 1;
        write_message(&mut self.writer, &ClientMessage::Request { id, command }).await?;
        loop {
            match read_message::<ServerMessage, _>(&mut self.reader).await? {
                Some(ServerMessage::Response { id: rid, result }) if rid == id => {
                    return result.map_err(IpcError::Remote);
                }
                Some(_) => continue,
                None => return Err(IpcError::Closed),
            }
        }
    }

    /// `Subscribe` 後に次のメッセージを待つ。
    pub async fn next_message(&mut self) -> Result<ServerMessage, IpcError> {
        read_message::<ServerMessage, _>(&mut self.reader)
            .await?
            .ok_or(IpcError::Closed)
    }
}

/// 1 回だけ接続してコマンドを実行する補助。
pub async fn one_shot(
    socket: &Path,
    client_name: &str,
    command: Command,
) -> Result<Response, IpcError> {
    let mut c = Client::connect(socket, client_name).await?;
    c.request(command).await
}
