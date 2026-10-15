//! デーモンとの通信。スナップショットの購読は自動再接続する。

use backup_ipc::{Client, Command, Response, ServerMessage, Snapshot};
use iced::futures::{SinkExt, Stream};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum DaemonEvent {
    Snapshot(Box<Snapshot>),
    Disconnected(String),
}

pub fn socket_path() -> PathBuf {
    backup_config::AppPaths::from_env().socket_path()
}

/// スナップショットの継続購読（切断時は 1 秒後に再接続）。
pub fn subscribe() -> impl Stream<Item = DaemonEvent> {
    iced::stream::channel(32, async |mut output| {
        loop {
            match Client::connect(&socket_path(), "gui").await {
                Ok(mut client) => {
                    if let Err(e) = client.request(Command::Subscribe).await {
                        let _ = output.send(DaemonEvent::Disconnected(e.to_string())).await;
                    } else {
                        loop {
                            match client.next_message().await {
                                Ok(ServerMessage::Snapshot { snapshot }) => {
                                    if output.send(DaemonEvent::Snapshot(snapshot)).await.is_err() {
                                        return;
                                    }
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    let _ =
                                        output.send(DaemonEvent::Disconnected(e.to_string())).await;
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = output.send(DaemonEvent::Disconnected(e.to_string())).await;
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
}

/// 1 回のコマンド実行。
pub async fn send(command: Command) -> Result<Response, String> {
    backup_ipc::one_shot(&socket_path(), "gui", command)
        .await
        .map_err(|e| e.to_string())
}
