//! Build-unit locking for concurrent Cargo processes.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::instrument;

use crate::compiler::build_runner::{DependencyArtifact, JobDependencies};
use crate::compiler::{BuildRunner, Unit};
use crate::util::data_structures::HashMap;
use crate::util::flock;
use crate::util::CargoResult;

/// Creates job-scoped lock sets for build units.
pub(crate) struct LockManager {
    shared_locks: Arc<SharedLockPool>,
}

impl LockManager {
    pub(crate) fn new() -> Self {
        Self {
            shared_locks: Arc::new(SharedLockPool::new()),
        }
    }

    /// Prepares a path-only lock set for one job.
    ///
    /// Handles are opened when the job starts. Dependencies needed only for
    /// pipelining use their metadata lock; dependencies whose object code is
    /// needed use their full-unit lock.
    pub(crate) fn prepare(
        &self,
        build_runner: &BuildRunner<'_, '_>,
        unit: &Unit,
        dependencies: &JobDependencies,
    ) -> UnitLockSet {
        let own_full = LockKey::full(build_runner, unit);
        let own_metadata = LockKey::metadata(build_runner, unit);
        let mut requests = BTreeMap::new();
        for (dependency, artifact) in dependencies {
            let key = match artifact {
                DependencyArtifact::All => LockKey::full(build_runner, dependency),
                DependencyArtifact::Metadata => LockKey::metadata(build_runner, dependency),
            };
            requests.entry(key).or_insert(LockMode::Shared);
        }
        requests.insert(own_full.clone(), LockMode::Exclusive);
        requests.insert(own_metadata.clone(), LockMode::Exclusive);
        let requests = requests.into_iter().collect::<Vec<_>>();
        let own_full = requests
            .binary_search_by(|(key, _)| key.cmp(&own_full))
            .expect("unit lock set contains its full lock");
        let own_metadata = requests
            .binary_search_by(|(key, _)| key.cmp(&own_metadata))
            .expect("unit lock set contains its metadata lock");

        UnitLockSet {
            own_full,
            own_metadata,
            requests,
            shared_locks: Arc::clone(&self.shared_locks),
        }
    }

    /// Prepares path-only exclusive locks for user-facing artifact destinations.
    pub(crate) fn prepare_artifact_publication(
        &self,
        build_runner: &BuildRunner<'_, '_>,
        destinations: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> ArtifactLockSet {
        let target_root = build_runner.bcx.ws.target_dir().into_path_unlocked();
        let lock_root = target_root.join(".cargo-artifact-locks/v1");
        let mut paths = destinations
            .into_iter()
            .map(|destination| {
                let destination = destination.as_ref();
                let relative = destination.strip_prefix(&target_root);
                let key = match relative {
                    Ok(relative) => crate::util::short_hash(&(true, relative)),
                    Err(_) => crate::util::short_hash(&(false, destination)),
                };
                lock_root.join(format!("{key}.lock"))
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        ArtifactLockSet(paths)
    }

    /// Prepares a shared lease for reading one completed unit.
    pub(crate) fn prepare_unit_read(
        &self,
        build_runner: &BuildRunner<'_, '_>,
        unit: &Unit,
    ) -> UnitReadLock {
        UnitReadLock {
            path: build_runner.files().build_unit_lock(unit),
            shared_locks: Arc::clone(&self.shared_locks),
        }
    }
}

/// Shares one open shared-lock descriptor between local jobs using the same path.
struct SharedLockPool {
    entries: Mutex<HashMap<PathBuf, Arc<SharedLockEntry>>>,
}

impl SharedLockPool {
    fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::default()),
        }
    }

    fn acquire(&self, path: &Path) -> CargoResult<SharedLockLease> {
        let entry = {
            let mut entries = self.entries.lock().unwrap();
            Arc::clone(
                entries
                    .entry(path.to_path_buf())
                    .or_insert_with(|| Arc::new(SharedLockEntry::new(path.to_path_buf()))),
            )
        };
        entry.acquire()
    }
}

struct SharedLockEntry {
    path: PathBuf,
    state: Mutex<SharedLockState>,
    acquired: Condvar,
}

impl SharedLockEntry {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(SharedLockState::Vacant),
            acquired: Condvar::new(),
        }
    }

    fn acquire(self: Arc<Self>) -> CargoResult<SharedLockLease> {
        let mut state = self.state.lock().unwrap();
        loop {
            match &mut *state {
                SharedLockState::Vacant => {
                    *state = SharedLockState::Acquiring;
                    break;
                }
                SharedLockState::Acquiring => {
                    state = self.acquired.wait(state).unwrap();
                }
                SharedLockState::Held { leases, .. } => {
                    *leases += 1;
                    drop(state);
                    return Ok(SharedLockLease { entry: self });
                }
            }
        }
        drop(state);

        let acquired = (|| {
            let file = flock::open_lock_file(&self.path)?;
            flock::lock_shared(&file)?;
            Ok(file)
        })();
        let mut state = self.state.lock().unwrap();
        match acquired {
            Ok(file) => {
                *state = SharedLockState::Held {
                    _file: file,
                    leases: 1,
                };
                self.acquired.notify_all();
                drop(state);
                Ok(SharedLockLease { entry: self })
            }
            Err(error) => {
                *state = SharedLockState::Vacant;
                self.acquired.notify_all();
                Err(error)
            }
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        match &mut *state {
            SharedLockState::Held { leases, .. } if *leases > 1 => *leases -= 1,
            SharedLockState::Held { .. } => *state = SharedLockState::Vacant,
            SharedLockState::Vacant | SharedLockState::Acquiring => {
                unreachable!("shared lock lease outlived its acquired state")
            }
        }
    }
}

enum SharedLockState {
    Vacant,
    Acquiring,
    Held { _file: File, leases: usize },
}

struct SharedLockLease {
    entry: Arc<SharedLockEntry>,
}

impl Drop for SharedLockLease {
    fn drop(&mut self) {
        self.entry.release();
    }
}

/// Exclusive publication locks acquired lazily in a stable order.
pub(crate) struct ArtifactLockSet(Vec<PathBuf>);

impl ArtifactLockSet {
    pub(crate) fn acquire(&self) -> CargoResult<ArtifactLockLease> {
        let mut locks = Vec::with_capacity(self.0.len());
        for path in &self.0 {
            let file = flock::open_lock_file(path)?;
            flock::lock_exclusive(&file)?;
            locks.push(file);
        }
        Ok(ArtifactLockLease { _locks: locks })
    }
}

/// Keeps artifact publication locks held until it is dropped.
pub(crate) struct ArtifactLockLease {
    _locks: Vec<File>,
}

/// A lazily opened shared lock for consuming one completed unit.
pub(crate) struct UnitReadLock {
    path: PathBuf,
    shared_locks: Arc<SharedLockPool>,
}

impl UnitReadLock {
    pub(crate) fn acquire(&self) -> CargoResult<UnitReadLease> {
        Ok(UnitReadLease {
            _lease: self.shared_locks.acquire(&self.path)?,
        })
    }
}

/// Keeps a completed unit stable while it is being consumed.
pub(crate) struct UnitReadLease {
    _lease: SharedLockLease,
}

/// A sorted set of lock paths owned by one queued job.
pub(crate) struct UnitLockSet {
    own_full: usize,
    own_metadata: usize,
    requests: Vec<(LockKey, LockMode)>,
    shared_locks: Arc<SharedLockPool>,
}

impl UnitLockSet {
    pub(crate) fn acquire_shared(&self) -> CargoResult<UnitLockLease> {
        self.acquire_with(false)
    }

    #[instrument(skip_all, fields(unit = %self.requests[self.own_full].0))]
    pub(crate) fn acquire(&self) -> CargoResult<UnitLockLease> {
        self.acquire_with(true)
    }

    fn acquire_with(&self, requested_modes: bool) -> CargoResult<UnitLockLease> {
        let mut locks = Vec::with_capacity(self.requests.len());
        for (key, mode) in &self.requests {
            let lock = match (requested_modes, mode) {
                (true, LockMode::Exclusive) => {
                    let file = flock::open_lock_file(&key.0)?;
                    flock::lock_exclusive(&file)?;
                    UnitLockHandle::Exclusive(file)
                }
                _ => UnitLockHandle::Shared {
                    _lease: self.shared_locks.acquire(&key.0)?,
                },
            };
            locks.push(lock);
        }
        Ok(UnitLockLease {
            locks,
            own_full: self.own_full,
            own_metadata: self.own_metadata,
            full_downgraded: AtomicBool::new(!requested_modes),
            metadata_downgraded: AtomicBool::new(!requested_modes),
        })
    }
}

/// Open unit locks held by a running job.
pub(crate) struct UnitLockLease {
    locks: Vec<UnitLockHandle>,
    own_full: usize,
    own_metadata: usize,
    full_downgraded: AtomicBool,
    metadata_downgraded: AtomicBool,
}

impl UnitLockLease {
    /// Allows pipelined dependents to consume metadata while the producer
    /// continues generating object code.
    pub(crate) fn metadata_produced(&self) -> CargoResult<()> {
        if !self.metadata_downgraded.swap(true, Ordering::Relaxed) {
            flock::lock_shared(self.locks[self.own_metadata].exclusive_file())?;
        }
        Ok(())
    }

    /// Converts the job's own locks to shared access before consuming a fresh
    /// artifact. Dependency locks are already shared.
    pub(crate) fn downgrade_own(&self) -> CargoResult<()> {
        self.metadata_produced()?;
        if !self.full_downgraded.swap(true, Ordering::Relaxed) {
            flock::lock_shared(self.locks[self.own_full].exclusive_file())?;
        }
        Ok(())
    }
}

enum UnitLockHandle {
    Exclusive(File),
    Shared { _lease: SharedLockLease },
}

impl UnitLockHandle {
    fn exclusive_file(&self) -> &File {
        let Self::Exclusive(file) = self else {
            unreachable!("only an exclusively acquired unit lock is downgraded")
        };
        file
    }
}

#[derive(Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, Hash, Eq, Ord, PartialEq, PartialOrd)]
struct LockKey(PathBuf);

impl LockKey {
    fn full(build_runner: &BuildRunner<'_, '_>, unit: &Unit) -> Self {
        Self(build_runner.files().build_unit_lock(unit))
    }

    fn metadata(build_runner: &BuildRunner<'_, '_>, unit: &Unit) -> Self {
        Self(build_runner.files().build_unit_metadata_lock(unit))
    }
}

impl Display for LockKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}
