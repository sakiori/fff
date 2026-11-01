use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::client::ClientError;
use crate::program::read_frame;
use crate::protocol::{Request, ServiceError};
use crate::session::{ProgramConfig, Target};

/// Largest event frame a client reads.
pub const MAX_EVENT_FRAME_BYTES: usize = 64 * 1024 * 1024;
const RECONNECT_MIN: Duration = Duration::from_millis(100);
const RECONNECT_MAX: Duration = Duration::from_secs(5);

/// What a subscription asks for beside its directory and index options.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchRequest {
    /// A glob, an exact path or a directory, relative to the subscription's directory;
    /// empty watches all of it.
    pub pattern: String,
    /// Exclusions relative to the subscription's directory: globs, or path prefixes
    /// without wildcards.
    pub ignore: Vec<String>,
    /// Whether to deliver [`WatchEventKind::Repository`] events.
    pub repository: bool,
    /// Whether to deliver [`WatchEventKind::Refs`] events.
    pub refs: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchEventKind {
    Created,
    Modified,
    Removed,
    /// `path` is the destination, `from` the source.
    Renamed,
    /// Changes were lost; whatever the program derives from the directory is stale.
    Rescan,
    /// The git state of the work tree at `path` changed: its index, HEAD or an operation
    /// in progress such as a merge.
    Repository,
    /// A branch, tag or remote-tracking ref of the work tree at `path` was created, moved or
    /// deleted, loose or in `packed-refs`. A linked worktree shares its main repository's.
    Refs,
}

/// One change. Paths are relative to the subscription's directory, and a rescan names the
/// directory itself with an empty path; a repository or refs event names its work tree,
/// absolute when the work tree lies above the directory. The service sends batches of these as
/// [`WatchFrame::Events`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchEvent {
    pub path: String,
    pub kind: WatchEventKind,
    pub from: Option<String>,
}

impl WatchEvent {
    pub fn rescan() -> Self {
        Self {
            path: String::new(),
            kind: WatchEventKind::Rescan,
            from: None,
        }
    }
}

/// What the service sends a subscription after `ready`.
#[derive(Debug, Serialize, Deserialize)]
pub enum WatchFrame {
    Events(Vec<WatchEvent>),
    /// The subscription cannot go on, and the service ends it.
    Failed(ServiceError),
}

/// What a subscription hands its program, in order.
#[derive(Debug)]
pub enum Delivery {
    /// Every change from now on is delivered.
    Ready,
    Events(Vec<WatchEvent>),
    /// The subscription ended for good: it could not be opened, or the service, or a
    /// reconnect, refused it. Nothing follows.
    Failed(ClientError),
}

/// A watch subscription on the shared service, delivering on a thread of its own.
///
/// Opening returns at once; [`Delivery::Ready`] follows once the service subscribed, or
/// [`Delivery::Failed`]. When the connection to the service is lost, the subscription
/// opens a new one with backoff, starting the service when none runs, and delivers a
/// rescan event once it is subscribed again; a reconnect the service refuses, rather than
/// one that cannot reach it, ends it with `Failed`. Dropping it ends the subscription;
/// once the drop returns, `deliver` runs no more, unless the drop happens inside `deliver`,
/// which then is not called again.
pub struct Subscription {
    stop: tokio::sync::watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
}

impl Subscription {
    /// Starts subscribing to the changes under `config.scope`. `config.connect_timeout`
    /// bounds opening it, the index's initial scan included, and each reconnect attempt.
    pub fn start(
        config: &ProgramConfig,
        request: WatchRequest,
        deliver: impl FnMut(Delivery) + Send + 'static,
    ) -> Result<Self, ClientError> {
        let target = Target::resolve(config, |scope, options| Request::Watch {
            scope,
            options,
            subscription: request,
        })?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let thread = std::thread::Builder::new()
            .name("fff-watch".into())
            .spawn(move || runtime.block_on(deliver_until(target, stopped, deliver)))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
        {
            let _ = thread.join();
        }
    }
}

async fn deliver_until(
    target: Target,
    mut stop: tokio::sync::watch::Receiver<bool>,
    mut deliver: impl FnMut(Delivery),
) {
    let opened = tokio::select! {
        _ = stop.changed() => return,
        opened = target.open(target.connect_timeout) => opened,
    };
    let mut connection = match opened {
        Ok((connection, _)) => Some(connection),
        Err(error) => return deliver(Delivery::Failed(error)),
    };
    deliver(Delivery::Ready);
    let mut backoff = RECONNECT_MIN;
    loop {
        // The send half stays open with the subscription: closing it ends the subscription.
        if let Some((mut recv, _send)) = connection.take() {
            loop {
                let frame = tokio::select! {
                    _ = stop.changed() => return,
                    frame = read_frame::<WatchFrame, _>(&mut recv, MAX_EVENT_FRAME_BYTES) => frame,
                };
                if *stop.borrow() {
                    return;
                }
                match frame {
                    Ok(WatchFrame::Events(events)) => deliver(Delivery::Events(events)),
                    Ok(WatchFrame::Failed(error)) => {
                        return deliver(Delivery::Failed(ClientError::Service(error)));
                    }
                    Err(error) => {
                        tracing::debug!(%error, "Watch subscription lost its service");
                        break;
                    }
                }
            }
            backoff = RECONNECT_MIN;
        }
        tokio::select! {
            _ = stop.changed() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(RECONNECT_MAX);
        let opened = tokio::select! {
            _ = stop.changed() => return,
            opened = target.open(target.connect_timeout) => opened,
        };
        match opened {
            Ok((reopened, _)) => {
                connection = Some(reopened);
                deliver(Delivery::Events(vec![WatchEvent::rescan()]));
            }
            // A service that cannot be reached may come back; one that refuses does not.
            Err(error) if error.lost_service() => {
                tracing::debug!(%error, "Cannot resubscribe yet");
            }
            Err(error) => return deliver(Delivery::Failed(error)),
        }
    }
}
