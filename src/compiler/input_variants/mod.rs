//! Retains compilation branches selected by observed process inputs.

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use cargo_util::paths;
use serde::{Deserialize, Serialize};

use crate::util::data_structures::HashMap;
use crate::util::flock;
use crate::util::{CargoResult, FileLock, Filesystem, GlobalContext, StableHasher};

use super::UnitHash;

pub mod outputs;

#[cfg(test)]
mod tests;

const FORMAT_VERSION: u32 = 4;
const CARGO_MANIFEST_DIR_ENV: &str = "CARGO_MANIFEST_DIR";
const CARGO_MANIFEST_PATH_ENV: &str = "CARGO_MANIFEST_PATH";
const CACHE_LOCK_FILE: &str = ".cargo-input-variants-lock";
const CACHE_LOCK_DESCRIPTION: &str = "retained input variants";

/// Holds the build and target roots stable while retained variant state is in use.
///
/// Acquire this guard before profile or build-unit locks so compilation and cleanup use one lock
/// order.
pub struct CacheLockGuard {
    _build: FileLock,
    _target: Option<FileLock>,
}

impl CacheLockGuard {
    pub fn shared(
        build_root: &Filesystem,
        target_root: &Filesystem,
        gctx: &GlobalContext,
    ) -> CargoResult<Self> {
        Self::acquire(build_root, target_root, gctx, CacheLockMode::Shared)
    }

    pub fn exclusive(
        build_root: &Filesystem,
        target_root: &Filesystem,
        gctx: &GlobalContext,
    ) -> CargoResult<Self> {
        Self::acquire(build_root, target_root, gctx, CacheLockMode::Exclusive)
    }

    fn acquire(
        build_root: &Filesystem,
        target_root: &Filesystem,
        gctx: &GlobalContext,
        mode: CacheLockMode,
    ) -> CargoResult<Self> {
        let build = mode.acquire(build_root, gctx)?;
        let target = if target_root == build_root {
            None
        } else {
            Some(mode.acquire(target_root, gctx)?)
        };
        Ok(Self {
            _build: build,
            _target: target,
        })
    }
}

#[derive(Clone, Copy)]
enum CacheLockMode {
    Shared,
    Exclusive,
}

impl CacheLockMode {
    fn acquire(self, root: &Filesystem, gctx: &GlobalContext) -> CargoResult<FileLock> {
        match self {
            Self::Shared => root.open_ro_shared_create(
                CACHE_LOCK_FILE,
                gctx,
                CACHE_LOCK_DESCRIPTION,
            ),
            Self::Exclusive => root.open_rw_exclusive_create(
                CACHE_LOCK_FILE,
                gctx,
                CACHE_LOCK_DESCRIPTION,
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputSource {
    BuildScriptEnv,
    RustcEnv,
}

impl InputSource {
    fn directory(self) -> &'static str {
        match self {
            Self::BuildScriptEnv => "build-script-env",
            Self::RustcEnv => "rustc-env",
        }
    }

    fn normalize_names(self, names: &mut Vec<String>) {
        if self == Self::BuildScriptEnv {
            // Build scripts may read Cargo's injected paths without declaring them through
            // `rerun-if-env-changed`.
            names.extend(
                [CARGO_MANIFEST_DIR_ENV, CARGO_MANIFEST_PATH_ENV].map(str::to_owned),
            );
        }
        names.sort_unstable();
        names.dedup();
    }
}

pub(super) fn is_manifest_path_env(name: &str) -> bool {
    matches!(name, CARGO_MANIFEST_DIR_ENV | CARGO_MANIFEST_PATH_ENV)
}

#[derive(Clone, Debug)]
struct InputEnvironment {
    manifest_dir: PathBuf,
    manifest_path: PathBuf,
    env_config: Arc<HashMap<String, OsString>>,
    inherit_process_env: bool,
}

impl InputEnvironment {
    fn new(
        manifest_path: &Path,
        env_config: &Arc<HashMap<String, OsString>>,
        inherit_process_env: bool,
    ) -> Self {
        Self {
            manifest_dir: manifest_path
                .parent()
                .expect("package manifest has a parent")
                .to_path_buf(),
            manifest_path: manifest_path.to_path_buf(),
            env_config: Arc::clone(env_config),
            inherit_process_env,
        }
    }

    fn values(&self, names: &[String]) -> Vec<(String, EncodedValue)> {
        names
            .iter()
            .map(|name| (name.clone(), encode_value(self.value(name))))
            .collect()
    }

    #[allow(clippy::disallowed_methods)]
    fn value(&self, name: &str) -> Option<OsString> {
        match name {
            CARGO_MANIFEST_DIR_ENV => Some(self.manifest_dir.as_os_str().to_owned()),
            CARGO_MANIFEST_PATH_ENV => Some(self.manifest_path.as_os_str().to_owned()),
            _ => self
                .env_config
                .get(name)
                .cloned()
                .or_else(|| self.inherit_process_env.then(|| env::var_os(name)).flatten()),
        }
    }

    #[allow(clippy::disallowed_methods)]
    fn provisional_key(&self) -> u64 {
        let mut environment = BTreeMap::new();
        if self.inherit_process_env {
            environment.extend(env::vars_os());
        }
        environment.extend(
            self.env_config
                .iter()
                .map(|(name, value)| (OsString::from(name), value.clone())),
        );
        environment.insert(
            OsString::from(CARGO_MANIFEST_DIR_ENV),
            self.manifest_dir.as_os_str().to_owned(),
        );
        environment.insert(
            OsString::from(CARGO_MANIFEST_PATH_ENV),
            self.manifest_path.as_os_str().to_owned(),
        );

        let mut hasher = StableHasher::new();
        environment.hash(&mut hasher);
        Hasher::finish(&hasher).max(1)
    }
}

#[derive(Clone, Debug)]
pub struct InputVariant {
    key: u64,
    needs_refresh: bool,
    source: InputSource,
    schema_id: UnitHash,
    context_id: UnitHash,
    build_root: PathBuf,
    schema_path: PathBuf,
    records_dir: PathBuf,
    schema_lock: Option<PathBuf>,
    environment: InputEnvironment,
}

#[derive(Debug)]
struct InputLock(File);

impl InputLock {
    fn acquire(path: &PathBuf) -> CargoResult<Self> {
        let file = flock::open_lock_file(path)?;
        flock::lock_exclusive(&file)?;
        Ok(Self(file))
    }
}

impl Drop for InputLock {
    fn drop(&mut self) {
        if let Err(error) = flock::unlock(&self.0) {
            tracing::warn!("failed to release input variant lock: {error}");
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct InputSchema {
    version: u32,
    source: InputSource,
    schema_id: String,
    generation: u64,
    names: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct TrackedPath {
    root: TrackedRoot,
    path: PathBuf,
    stamp: Option<FileStamp>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
enum TrackedRoot {
    Package,
    Build,
    Absolute,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct FileStamp {
    len: u64,
    modified_secs: u64,
    modified_nanos: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct VariantRecord {
    version: u32,
    generation: u64,
    context_id: String,
    key: u64,
    values: Vec<(String, EncodedValue)>,
    tracked_paths: Vec<TrackedPath>,
    created: u64,
    last_used: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "state", content = "value")]
enum EncodedValue {
    Unset,
    UnixBytes(String),
    WindowsWide(Vec<u16>),
}

impl InputVariant {
    pub fn select(
        build_root: &Path,
        package_name: &str,
        manifest_path: &Path,
        schema_id: UnitHash,
        context_id: UnitHash,
        source: InputSource,
        env_config: &Arc<HashMap<String, OsString>>,
        inherit_process_env: bool,
        gctx: &GlobalContext,
    ) -> CargoResult<Self> {
        let environment = InputEnvironment::new(
            manifest_path,
            env_config,
            inherit_process_env,
        );
        let registry_root = build_root.join(".input-variants").join("v1");
        let relative = Path::new(source.directory())
            .join(package_name)
            .join(schema_id.to_string());
        let schema_path = registry_root
            .join("schemas")
            .join(&relative)
            .with_extension("json");
        let records_dir = registry_root.join("records").join(&relative);
        let schema_lock_path = relative.with_extension("lock");
        let schema_lock = if flock::is_on_nfs_mount(build_root) {
            if gctx.cli_unstable().fine_grain_locking {
                anyhow::bail!(
                    "fine-grained build locking is not supported on NFS build directories"
                );
            }
            None
        } else {
            Some(registry_root.join("locks").join(schema_lock_path))
        };
        let _schema_lock_guard = schema_lock
            .as_ref()
            .map(InputLock::acquire)
            .transpose()?;
        let schema = load_schema(&schema_path, source, schema_id)?;
        let records = load_records(
            &records_dir,
            schema.as_ref().map(|schema| schema.generation),
        )?;
        let schema_missing = schema.is_none();
        let mut names = schema
            .as_ref()
            .map(|schema| schema.names.clone())
            .unwrap_or_default();
        source.normalize_names(&mut names);
        let values = environment.values(&names);
        let context_key = context_id.to_string();
        let mut found_matching_record = false;
        let mut matching_records = Vec::new();
        for record in records {
            if record.context_id != context_key || record.values != values {
                continue;
            }
            found_matching_record = true;
            if !record.is_stale(&environment.manifest_dir, build_root) {
                matching_records.push(record);
            }
        }
        let records_stale = found_matching_record && matching_records.is_empty();
        let matching_record = if matching_records.len() > 1 {
            let preferred = environment.provisional_key();
            if let Some(index) = matching_records
                .iter()
                .position(|record| record.key == preferred)
            {
                Some(matching_records.swap_remove(index))
            } else {
                matching_records.into_iter().next()
            }
        } else {
            matching_records.pop()
        };
        // Until the observed names are known, the full visible environment is
        // the only safe discriminator between concurrent cold planners. The
        // published record narrows future selection to the observed values.
        let provisional = records_stale
            || (schema_missing && gctx.cli_unstable().fine_grain_locking);
        let key = if provisional {
            environment.provisional_key()
        } else if let Some(mut record) = matching_record {
            record.last_used = now();
            write_record(&records_dir, &record)?;
            record.key
        } else if schema_missing || values.is_empty() {
            0
        } else {
            variant_key(&values)
        };

        Ok(Self {
            key,
            needs_refresh: provisional,
            source,
            schema_id,
            context_id,
            build_root: build_root.to_path_buf(),
            schema_path,
            records_dir,
            schema_lock,
            environment,
        })
    }

    pub fn is_branched(&self) -> bool {
        self.key != 0
    }

    pub fn needs_refresh(&self) -> bool {
        self.needs_refresh
    }

    pub fn hash(&self, hasher: &mut StableHasher) {
        FORMAT_VERSION.hash(hasher);
        if self.key != 0 {
            self.source.hash(hasher);
            self.key.hash(hasher);
        }
    }

    pub fn record_names(
        &self,
        names: &[String],
        paths: &[PathBuf],
    ) -> CargoResult<()> {
        let _lock = self
            .schema_lock
            .as_ref()
            .map(InputLock::acquire)
            .transpose()?;
        let previous = load_schema(&self.schema_path, self.source, self.schema_id)?;
        let mut merged_names = previous
            .as_ref()
            .map(|schema| schema.names.clone())
            .unwrap_or_default();
        merged_names.extend(names.iter().cloned());
        self.source.normalize_names(&mut merged_names);

        let mut tracked_paths = paths
            .iter()
            .map(|path| self.track_path(path))
            .collect::<Vec<_>>();
        tracked_paths.sort();
        tracked_paths.dedup();

        let names_expanded = previous
            .as_ref()
            .is_some_and(|schema| schema.names != merged_names);
        let generation = previous
            .as_ref()
            .map(|schema| schema.generation + u64::from(names_expanded))
            .unwrap_or(0);
        let schema = InputSchema {
            version: FORMAT_VERSION,
            source: self.source,
            schema_id: self.schema_id.to_string(),
            generation,
            names: merged_names,
        };

        if names_expanded {
            remove_dir_if_exists(&self.records_dir)?;
        }

        let parent = self
            .schema_path
            .parent()
            .expect("input variant schema has a parent");
        paths::create_dir_all(parent)?;
        paths::write_atomic(&self.schema_path, serde_json::to_vec(&schema)?)?;

        let timestamp = now();
        let record = VariantRecord {
            version: FORMAT_VERSION,
            generation,
            context_id: self.context_id.to_string(),
            key: self.key,
            values: self.environment.values(&schema.names),
            tracked_paths,
            created: timestamp,
            last_used: timestamp,
        };
        write_record(&self.records_dir, &record)?;
        Ok(())
    }

    fn track_path(&self, path: &Path) -> TrackedPath {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.environment.manifest_dir.join(path)
        };
        let (root, path) = if let Ok(path) = absolute.strip_prefix(&self.environment.manifest_dir) {
            (TrackedRoot::Package, path.to_path_buf())
        } else if let Ok(path) = absolute.strip_prefix(&self.build_root) {
            (TrackedRoot::Build, path.to_path_buf())
        } else {
            (TrackedRoot::Absolute, absolute.clone())
        };
        TrackedPath {
            root,
            path,
            stamp: file_stamp(&absolute),
        }
    }
}

impl VariantRecord {
    fn is_stale(&self, package_root: &Path, build_root: &Path) -> bool {
        self.tracked_paths
            .iter()
            .any(|path| path.is_stale(package_root, build_root))
    }
}

impl TrackedPath {
    fn resolve(&self, package_root: &Path, build_root: &Path) -> PathBuf {
        match self.root {
            TrackedRoot::Package => package_root.join(&self.path),
            TrackedRoot::Build => build_root.join(&self.path),
            TrackedRoot::Absolute => self.path.clone(),
        }
    }

    fn is_stale(&self, package_root: &Path, build_root: &Path) -> bool {
        self.stamp != file_stamp(&self.resolve(package_root, build_root))
    }
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(FileStamp {
        len: metadata.len(),
        modified_secs: modified.as_secs(),
        modified_nanos: modified.subsec_nanos(),
    })
}

fn load_schema(
    path: &Path,
    source: InputSource,
    schema_id: UnitHash,
) -> CargoResult<Option<InputSchema>> {
    let bytes = match paths::read_bytes(path) {
        Ok(bytes) => bytes,
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
            error.kind() == std::io::ErrorKind::NotFound
        }) => return Ok(None),
        Err(error) => return Err(error),
    };

    let schema = match serde_json::from_slice::<InputSchema>(&bytes) {
        Ok(schema)
            if schema.version == FORMAT_VERSION
                && schema.source == source
                && schema.schema_id == schema_id.to_string() => schema,
        Ok(_) | Err(_) => {
            remove_if_exists(path)?;
            return Ok(None);
        }
    };
    Ok(Some(schema))
}

fn load_records(records_dir: &Path, generation: Option<u64>) -> CargoResult<Vec<VariantRecord>> {
    let Some(generation) = generation else {
        return Ok(Vec::new());
    };
    let entries = match fs::read_dir(records_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read `{}`", records_dir.display()));
        }
    };
    let mut entry_paths = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    entry_paths.sort();

    let mut records = Vec::new();
    for path in entry_paths {
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        let record = paths::read_bytes(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<VariantRecord>(&bytes).ok());
        match record {
            Some(record)
                if record.version == FORMAT_VERSION && record.generation == generation =>
            {
                records.push(record);
            }
            _ => remove_if_exists(&path)?,
        }
    }
    Ok(records)
}

fn write_record(records_dir: &Path, record: &VariantRecord) -> CargoResult<()> {
    paths::create_dir_all(records_dir)?;
    paths::write_atomic(
        records_dir.join(format!("{}-{:016x}.json", record.context_id, record.key)),
        serde_json::to_vec(record)?,
    )
}

fn remove_if_exists(path: &Path) -> CargoResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_dir_if_exists(path: &Path) -> CargoResult<()> {
    if path.exists() {
        paths::remove_dir_all(path)?;
    }
    Ok(())
}

fn encode_value(value: Option<OsString>) -> EncodedValue {
    let Some(value) = value else {
        return EncodedValue::Unset;
    };

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        EncodedValue::UnixBytes(hex::encode(value.as_os_str().as_bytes()))
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        EncodedValue::WindowsWide(value.encode_wide().collect())
    }

    #[cfg(not(any(unix, windows)))]
    {
        EncodedValue::UnixBytes(hex::encode(value.to_string_lossy().as_bytes()))
    }
}

fn variant_key(values: &[(String, EncodedValue)]) -> u64 {
    let mut hasher = StableHasher::new();
    values.hash(&mut hasher);
    Hasher::finish(&hasher).max(1)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
