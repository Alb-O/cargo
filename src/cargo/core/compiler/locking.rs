//! Build-unit locking for concurrent Cargo processes.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::instrument;

use crate::core::compiler::build_runner::{DependencyArtifact, JobDependencies};
use crate::core::compiler::{BuildRunner, Unit};
use crate::util::flock;
use crate::util::CargoResult;

/// Creates job-scoped lock sets for build units.
pub(crate) struct LockManager;

impl LockManager {
    pub(crate) fn new() -> Self {
        Self
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
        UnitReadLock(build_runner.files().build_unit_lock(unit))
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
pub(crate) struct UnitReadLock(PathBuf);

impl UnitReadLock {
    pub(crate) fn acquire(&self) -> CargoResult<UnitReadLease> {
        let file = flock::open_lock_file(&self.0)?;
        flock::lock_shared(&file)?;
        Ok(UnitReadLease { _file: file })
    }
}

/// Keeps a completed unit stable while it is being consumed.
pub(crate) struct UnitReadLease {
    _file: File,
}

/// A sorted set of lock paths owned by one queued job.
pub(crate) struct UnitLockSet {
    own_full: usize,
    own_metadata: usize,
    requests: Vec<(LockKey, LockMode)>,
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
            let file = flock::open_lock_file(&key.0)?;
            match (requested_modes, mode) {
                (true, LockMode::Exclusive) => flock::lock_exclusive(&file)?,
                _ => flock::lock_shared(&file)?,
            }
            locks.push(file);
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
    locks: Vec<File>,
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
            flock::lock_shared(&self.locks[self.own_metadata])?;
        }
        Ok(())
    }

    /// Converts the job's own locks to shared access before consuming a fresh
    /// artifact. Dependency locks are already shared.
    pub(crate) fn downgrade_own(&self) -> CargoResult<()> {
        self.metadata_produced()?;
        if !self.full_downgraded.swap(true, Ordering::Relaxed) {
            flock::lock_shared(&self.locks[self.own_full])?;
        }
        Ok(())
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
