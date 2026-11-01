use std::io;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::shared_index::{IndexOptions, Readiness, Watcher};

// Bump whenever a message below changes shape.
pub(crate) const PROTOCOL: u32 = 2;
pub(crate) const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
pub(crate) struct Endpoint {
    pub name: String,
    pub token: String,
    pub protocol: u32,
    pub build: String,
    pub pid: u32,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Hello {
    pub token: String,
    pub protocol: u32,
    pub request: Request,
}

// Authentication fields are checked before the version-specific request is decoded.
#[derive(Deserialize)]
pub(crate) struct HelloHeader {
    pub token: String,
    pub protocol: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub(crate) enum Request {
    Session {
        scope: PathBuf,
        options: IndexOptions,
    },
    Status,
    Stop,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub(crate) enum Reply {
    Ready { warnings: Vec<String> },
    Status(ServiceStatus),
    Stopped,
    Error { error: ServiceError },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ServiceError {
    #[error("the namespace token does not match the running service")]
    Unauthorized,
    #[error("service protocol {service} is incompatible with client protocol {client}")]
    Protocol { service: u32, client: u32 },
    #[error("invalid service request: {reason}")]
    InvalidRequest { reason: String },
    #[error("the service is shutting down")]
    ShuttingDown,
    #[error("the service has {sessions} connected clients; disconnect them before stopping it")]
    Busy { sessions: usize },
    #[error("scope {} is unavailable: {reason}", path.display())]
    Scope { path: PathBuf, reason: String },
    #[error("scope {} is the home directory or filesystem root, which requires explicit opt-in", path.display())]
    ScopeRefused { path: PathBuf },
    #[error("cannot index {}: {reason}", root.display())]
    Index { root: PathBuf, reason: String },
    #[error("the service registry is unavailable")]
    RegistryUnavailable,
}

impl ServiceError {
    // Stale discovery state or a service that is going away; a fresh attempt can succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Unauthorized | Self::ShuttingDown | Self::RegistryUnavailable
        )
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ServiceStatus {
    pub pid: u32,
    pub build: String,
    pub protocol: u32,
    pub index_count: usize,
    pub indexes: Vec<IndexStatus>,
    pub sessions: Vec<SessionStatus>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct IndexStatus {
    pub id: String,
    pub root: PathBuf,
    pub repository: Option<PathBuf>,
    pub state: Readiness,
    pub watcher: Watcher,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionStatus {
    pub scope: PathBuf,
    pub index: String,
    pub root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum MessageError {
    #[error("connection closed")]
    Closed,
    #[error("message exceeds {MAX_MESSAGE_BYTES} bytes")]
    TooLarge,
    #[error("malformed message: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub(crate) async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Vec<u8>, MessageError> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_MESSAGE_BYTES as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    match bytes.last() {
        None => Err(MessageError::Closed),
        Some(b'\n') => Ok(bytes),
        Some(_) if bytes.len() > MAX_MESSAGE_BYTES => Err(MessageError::TooLarge),
        Some(_) => Err(MessageError::Closed),
    }
}

pub(crate) async fn read_message<T: DeserializeOwned, R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<T, MessageError> {
    Ok(serde_json::from_slice(&read_line(reader).await?)?)
}

pub(crate) async fn send_message<T: Serialize, W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &T,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversized_and_truncated_messages_are_rejected() {
        let oversized = vec![b'x'; MAX_MESSAGE_BYTES + 10];
        let result = read_line(&mut &oversized[..]).await;
        assert!(matches!(result, Err(MessageError::TooLarge)));
        let result = read_line(&mut &b"{\"partial\""[..]).await;
        assert!(matches!(result, Err(MessageError::Closed)));
        let line = read_line(&mut &b"{}\nrest"[..]).await.unwrap();
        assert_eq!(line, b"{}\n");
    }

    #[test]
    fn replies_round_trip_with_typed_errors() {
        let reply = Reply::Error {
            error: ServiceError::Busy { sessions: 2 },
        };
        let text = serde_json::to_string(&reply).unwrap();
        let Reply::Error { error } = serde_json::from_str(&text).unwrap() else {
            panic!("{text}");
        };
        assert_eq!(error, ServiceError::Busy { sessions: 2 });
        assert!(!error.is_retryable());
        assert!(ServiceError::ShuttingDown.is_retryable());
    }
}
