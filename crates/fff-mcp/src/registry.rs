use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use fff::scope::SearchScope;
use parking_lot::{Mutex, RwLock};
use tokio::sync::Notify;

use crate::protocol::{IndexStatus, ServiceError, SessionStatus};
use crate::service::Lease;
use crate::shared_index::{Index, IndexKey, IndexOptions, RepositoryProbe};

// Upper bound for a tool call waiting on an index that is still being built.
const INDEX_READY_TIMEOUT: Duration = Duration::from_secs(30);
// A binding can go stale again while it is replaced; give up after this many rebinds.
const MAX_REBINDS: usize = 3;

type PendingIndex = tokio::sync::OnceCell<Result<Arc<Index>, ServiceError>>;

#[derive(Default)]
pub(crate) struct Registry {
    sessions: Mutex<Vec<Weak<Session>>>,
    indexes: Mutex<Vec<Weak<Index>>>,
    pending: Mutex<HashMap<IndexKey, Weak<PendingIndex>>>,
    // Signalled by indexing threads when an index's directory coverage changes.
    coverage_changed: Arc<Notify>,
}

pub(crate) struct Session {
    pub scope: PathBuf,
    options: IndexOptions,
    binding: RwLock<Arc<Binding>>,
    repository: Mutex<RepositoryProbe>,
    registry: Weak<Registry>,
    _lease: Lease,
}

// An index and the session scope expressed relative to it.
pub(crate) struct Binding {
    pub index: Arc<Index>,
    pub scope: SearchScope,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SessionError {
    #[error("Index is still building; retry shortly")]
    Building,
    #[error("Index coverage is changing; retry shortly")]
    Unstable,
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error("Background task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl Registry {
    // Consolidates bindings on the runtime whenever an index's coverage changes.
    pub fn start() -> Arc<Self> {
        let registry = Arc::new(Self::default());
        let weak = Arc::downgrade(&registry);
        let changed = registry.coverage_changed.clone();
        tokio::spawn(async move {
            loop {
                changed.notified().await;
                let Some(registry) = weak.upgrade() else {
                    return;
                };
                registry.consolidate().await;
            }
        });
        registry
    }

    pub async fn register(
        self: &Arc<Self>,
        scope: PathBuf,
        options: IndexOptions,
        lease: Lease,
    ) -> Result<Arc<Session>, ServiceError> {
        let policy = options.clone();
        let (scope, probe) = tokio::task::spawn_blocking(move || {
            let scope_error = |reason: String| ServiceError::Scope {
                path: scope.clone(),
                reason,
            };
            let canonical =
                fff::path_utils::canonicalize(&scope).map_err(|e| scope_error(e.to_string()))?;
            if !canonical.is_dir() {
                return Err(scope_error("not a directory".into()));
            }
            if !policy.allows_root(&canonical) {
                return Err(ServiceError::ScopeRefused { path: canonical });
            }
            let probe = RepositoryProbe::discover(&canonical);
            Ok((canonical, probe))
        })
        .await
        .map_err(|_| ServiceError::RegistryUnavailable)??;
        let repository = probe.root().cloned();
        let index = self.resolve(&scope, repository, &options).await?;
        let binding = Binding::new(index, &scope)?;
        let session = Arc::new(Session {
            scope,
            options,
            binding: RwLock::new(Arc::new(binding)),
            repository: Mutex::new(probe),
            registry: Arc::downgrade(self),
            _lease: lease,
        });
        {
            let mut sessions = self.sessions.lock();
            sessions.retain(|s| s.strong_count() > 0);
            sessions.push(Arc::downgrade(&session));
        }
        self.consolidate().await;
        Ok(session)
    }

    pub fn status(&self) -> (Vec<IndexStatus>, Vec<SessionStatus>) {
        let indexes = live(&self.indexes)
            .iter()
            .map(|index| IndexStatus {
                id: index.id.clone(),
                root: index.root.clone(),
                repository: index.repository.clone(),
                state: index.readiness(),
                watcher: index.watcher(),
            })
            .collect();
        let sessions = live(&self.sessions)
            .iter()
            .map(|session| {
                let binding = session.snapshot();
                SessionStatus {
                    scope: session.scope.clone(),
                    index: binding.index.id.clone(),
                    root: binding.index.root.clone(),
                }
            })
            .collect();
        (indexes, sessions)
    }

    // Finds a compatible index for `scope` or starts building one.
    async fn resolve(
        self: &Arc<Self>,
        scope: &Path,
        repository: Option<PathBuf>,
        options: &IndexOptions,
    ) -> Result<Arc<Index>, ServiceError> {
        let candidates: Vec<_> = live(&self.indexes)
            .into_iter()
            .filter(|index| &index.options == options && index.repository == repository)
            .collect();
        let selection = {
            let scope = scope.to_path_buf();
            let root = repository.clone().unwrap_or_else(|| scope.clone());
            let options = options.clone();
            tokio::task::spawn_blocking(move || {
                let root = if options.allows_root(&root) {
                    root
                } else {
                    scope.clone()
                };
                let selection = select(candidates, &scope, root);
                // A scope removed after registration fails visibly until it is restored.
                if matches!(selection, Selection::Create(_)) && !scope.is_dir() {
                    return Err(ServiceError::Scope {
                        path: scope,
                        reason: "not a directory".into(),
                    });
                }
                Ok(selection)
            })
            .await
            .map_err(|_| ServiceError::RegistryUnavailable)??
        };
        let root = match selection {
            Selection::Existing(index) => return Ok(index),
            Selection::Create(root) => root,
        };
        let key = IndexKey {
            root,
            repository,
            options: options.clone(),
        };
        let cell = {
            let mut pending = self.pending.lock();
            pending.retain(|_, cell| cell.strong_count() > 0);
            match pending.get(&key).and_then(Weak::upgrade) {
                Some(cell) => cell,
                None => {
                    let cell = Arc::new(PendingIndex::new());
                    pending.insert(key.clone(), Arc::downgrade(&cell));
                    cell
                }
            }
        };
        cell.get_or_init(|| self.create(key)).await.clone()
    }

    async fn create(self: &Arc<Self>, key: IndexKey) -> Result<Arc<Index>, ServiceError> {
        let changed = self.coverage_changed.clone();
        let index =
            tokio::task::spawn_blocking(move || Index::create(key, move || changed.notify_one()))
                .await
                .map_err(|_| ServiceError::RegistryUnavailable)??;
        {
            let mut indexes = self.indexes.lock();
            indexes.retain(|i| i.strong_count() > 0);
            indexes.push(Arc::downgrade(&index));
        }
        let weak_index = Arc::downgrade(&index);
        let registry = Arc::downgrade(self);
        tokio::spawn(async move {
            if Index::monitor(weak_index).await.is_some()
                && let Some(registry) = registry.upgrade()
            {
                registry.consolidate().await;
            }
        });
        Ok(index)
    }

    // Moves watched sessions to the broadest compatible ready index that covers them;
    // an index no session uses any more is released with its last binding.
    async fn consolidate(self: &Arc<Self>) {
        let registry = self.clone();
        if let Err(error) =
            tokio::task::spawn_blocking(move || registry.consolidate_blocking()).await
        {
            tracing::error!(%error, "Index consolidation failed");
        }
    }

    fn consolidate_blocking(&self) {
        let indexes: Vec<_> = live(&self.indexes)
            .into_iter()
            .filter(|index| index.is_ready())
            .collect();
        for session in live(&self.sessions) {
            if !session.options.watch {
                continue;
            }
            let current = session.snapshot();
            let repository = session.repository.lock().root().cloned();
            let best = indexes
                .iter()
                .filter(|index| {
                    index.options == session.options
                        && index.repository == repository
                        && index.depth() < current.index.depth()
                        && index.covers(&session.scope)
                })
                .min_by_key(|index| index.depth());
            if let Some(best) = best
                && let Err(error) = session.rebind(&current, best.clone())
            {
                tracing::warn!(%error, scope = %session.scope.display(), "Cannot share broader index");
            }
        }
    }

    async fn rebind(
        self: &Arc<Self>,
        session: &Session,
        stale: &Arc<Binding>,
    ) -> Result<(), ServiceError> {
        let repository = session.repository.lock().root().cloned();
        let index = self
            .resolve(&session.scope, repository, &session.options)
            .await?;
        session.rebind(stale, index)
    }
}

impl Drop for Registry {
    // Ends the consolidation task.
    fn drop(&mut self) {
        self.coverage_changed.notify_one();
    }
}

enum Selection {
    Existing(Arc<Index>),
    Create(PathBuf),
}

// Prefers the broadest ready index covering `scope`, then a pending one that may cover it.
// Narrows to `scope` only when a compatible index at `root` is known not to cover it.
fn select(candidates: Vec<Arc<Index>>, scope: &Path, root: PathBuf) -> Selection {
    let covering = candidates
        .iter()
        .filter(|index| index.is_ready() && index.covers(scope))
        .min_by_key(|index| index.depth());
    let provisional = || {
        candidates
            .iter()
            .filter(|index| !index.is_ready() && index.may_cover(scope))
            .min_by_key(|index| index.depth())
    };
    if let Some(index) = covering.or_else(provisional) {
        return Selection::Existing(index.clone());
    }
    if candidates
        .iter()
        .any(|index| index.root == root && index.is_ready())
    {
        return Selection::Create(scope.to_path_buf());
    }
    Selection::Create(root)
}

impl Binding {
    fn new(index: Arc<Index>, scope: &Path) -> Result<Self, ServiceError> {
        Ok(Self {
            scope: index.search_scope(scope)?,
            index,
        })
    }
}

impl Session {
    pub fn snapshot(&self) -> Arc<Binding> {
        self.binding.read().clone()
    }

    // Waits for a ready index that still covers the scope, rebinding when coverage or
    // repository identity changed.
    pub async fn prepare(self: &Arc<Self>) -> Result<Arc<Binding>, SessionError> {
        let deadline = tokio::time::Instant::now() + INDEX_READY_TIMEOUT;
        for _ in 0..MAX_REBINDS {
            let binding = self.snapshot();
            if !binding.index.wait_ready(deadline).await {
                return Err(SessionError::Building);
            }
            let session = self.clone();
            let checked = binding.clone();
            if tokio::task::spawn_blocking(move || session.is_current(&checked)).await? {
                return Ok(binding);
            }
            let registry = self
                .registry
                .upgrade()
                .ok_or(ServiceError::RegistryUnavailable)?;
            registry.rebind(self, &binding).await?;
        }
        Err(SessionError::Unstable)
    }

    fn is_current(&self, binding: &Binding) -> bool {
        let mut repository = self.repository.lock();
        !repository.refresh(&self.scope)
            && binding.index.repository.as_ref() == repository.root()
            && binding.index.covers(&self.scope)
    }

    // Swaps the binding unless another task replaced `stale` first.
    fn rebind(&self, stale: &Arc<Binding>, index: Arc<Index>) -> Result<(), ServiceError> {
        let replacement = Arc::new(Binding::new(index, &self.scope)?);
        let retired = {
            let mut binding = self.binding.write();
            if !Arc::ptr_eq(&binding, stale) {
                return Ok(());
            }
            std::mem::replace(&mut *binding, replacement)
        };
        drop(retired);
        Ok(())
    }
}

fn live<T>(items: &Mutex<Vec<Weak<T>>>) -> Vec<Arc<T>> {
    items.lock().iter().filter_map(Weak::upgrade).collect()
}
