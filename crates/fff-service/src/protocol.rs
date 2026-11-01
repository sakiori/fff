use std::io;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Bump whenever a message below or a program frame changes shape.
pub const PROTOCOL: u32 = 6;
/// Largest hello the service reads.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Largest reply a client reads; a status lists every index and session.
pub const MAX_REPLY_LINE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub struct Endpoint {
    pub name: String,
    pub token: String,
    pub protocol: u32,
    pub build: String,
    pub pid: u32,
}

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub token: String,
    pub protocol: u32,
    pub request: Request,
}

// Authentication fields are checked before the version-specific request is decoded.
#[derive(Deserialize)]
pub struct HelloHeader {
    pub token: String,
    pub protocol: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// An MCP session: after `ready` the connection carries MCP messages.
    Session {
        scope: PathBuf,
        options: IndexOptions,
    },
    /// A program session: after `ready` the connection carries length-prefixed
    /// MessagePack frames, see `program`.
    Program {
        scope: PathBuf,
        options: IndexOptions,
    },
    /// A watch subscription for the changes under `scope`: `ready` follows once it is
    /// subscribed, then the service sends only event frames, see `watch`, until the program
    /// closes the connection.
    Watch {
        scope: PathBuf,
        options: IndexOptions,
        subscription: crate::watch::WatchRequest,
    },
    Status,
    Stop,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Ready { warnings: Vec<String> },
    Status(ServiceStatus),
    Stopped,
    Error { error: ServiceError },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServiceError {
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
    #[error("the service reported an error this client does not know")]
    #[serde(other)]
    Unknown,
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

/// What a session asks of the index that serves it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IndexOptions {
    pub warmup: bool,
    pub content_indexing: bool,
    pub watch: bool,
    pub follow_symlinks: bool,
    pub max_cached_files: Option<usize>,
    pub frecency_db: Option<String>,
    pub enable_home_scan: bool,
    pub enable_root_scan: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    Scanning,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Watcher {
    Disabled,
    Starting,
    Ready,
    Failed,
    Stopped,
}

#[derive(Serialize, Deserialize)]
pub struct ServiceStatus {
    pub pid: u32,
    pub build: String,
    pub protocol: u32,
    pub index_count: usize,
    pub indexes: Vec<IndexStatus>,
    pub sessions: Vec<SessionStatus>,
}

#[derive(Serialize, Deserialize)]
pub struct IndexStatus {
    pub id: String,
    pub root: PathBuf,
    pub repository: Option<PathBuf>,
    pub options: IndexOptions,
    pub state: Readiness,
    pub watcher: Watcher,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Mcp,
    Program,
    Watch,
}

#[derive(Serialize, Deserialize)]
pub struct SessionStatus {
    pub kind: SessionKind,
    pub scope: PathBuf,
    pub index: String,
    pub root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error("connection closed")]
    Closed,
    #[error("message exceeds {0} bytes")]
    TooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Reads one line of at most `limit` bytes.
pub async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Vec<u8>, MessageError> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    match bytes.last() {
        None => Err(MessageError::Closed),
        Some(b'\n') => Ok(bytes),
        Some(_) if bytes.len() > limit => Err(MessageError::TooLarge(limit)),
        Some(_) => Err(MessageError::Closed),
    }
}

pub async fn read_message<T: DeserializeOwned, R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<T, MessageError> {
    Ok(serde_json::from_slice(&read_line(reader, limit).await?)?)
}

pub async fn send_message<T: Serialize, W: AsyncWrite + Unpin>(
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
        let result = read_line(&mut &oversized[..], MAX_MESSAGE_BYTES).await;
        assert!(matches!(result, Err(MessageError::TooLarge(_))));
        let mut status = vec![b'x'; MAX_MESSAGE_BYTES + 10];
        status.push(b'\n');
        let line = read_line(&mut &status[..], MAX_REPLY_LINE_BYTES)
            .await
            .unwrap();
        assert_eq!(line.len(), status.len());
        let result = read_line(&mut &b"{\"partial\""[..], MAX_MESSAGE_BYTES).await;
        assert!(matches!(result, Err(MessageError::Closed)));
        let line = read_line(&mut &b"{}\nrest"[..], MAX_MESSAGE_BYTES)
            .await
            .unwrap();
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
