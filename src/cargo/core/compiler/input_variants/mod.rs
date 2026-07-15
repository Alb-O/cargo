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
use crate::util::{CargoResult, GlobalContext, StableHasher};

use super::UnitHash;

pub mod outputs;

#[cfg(test)]
mod tests;

const FORMAT_VERSION: u32 = 2;

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
}

#[derive(Clone, Debug)]
pub struct InputVariant {
    key: u64,
    requires_initialization_lock: bool,
    needs_refresh: bool,
    provisional: bool,
    source: InputSource,
    stable_unit_id: UnitHash,
    build_root: PathBuf,
    package_root: PathBuf,
    schema_path: PathBuf,
    records_dir: PathBuf,
    schema_lock: Option<PathBuf>,
    env_config: Arc<HashMap<String, OsString>>,
    inherit_process_env: bool,
}

/// Serializes discovery of previously unknown input schemas.
pub struct RegistryLock(File);

impl RegistryLock {
    pub fn exclusive(build_root: &Path) -> CargoResult<Self> {
        let path = build_root.join(".input-variants/v1/registry.lock");
        let file = flock::open_lock_file(&path)?;
        flock::lock_exclusive(&file)?;
        Ok(Self(file))
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        if let Err(error) = flock::unlock(&self.0) {
            tracing::warn!("failed to release input variant registry lock: {error}");
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct InputSchema {
    version: u32,
    source: InputSource,
    stable_unit_id: String,
    generation: u64,
    names: Vec<String>,
    tracked_paths: Vec<TrackedPath>,
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
    key: u64,
    values: Vec<(String, EncodedValue)>,
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
        package_root: &Path,
        stable_unit_id: UnitHash,
        source: InputSource,
        isolated_by_dependency: bool,
        env_config: &Arc<HashMap<String, OsString>>,
        inherit_process_env: bool,
        gctx: &GlobalContext,
    ) -> CargoResult<Self> {
        let registry_root = build_root.join(".input-variants").join("v1");
        let relative = Path::new(source.directory())
            .join(package_name)
            .join(stable_unit_id.to_string());
        let schema_path = registry_root
            .join("schemas")
            .join(&relative)
            .with_extension("json");
        let records_dir = registry_root.join("records").join(&relative);
        let lock_path = relative.with_extension("lock");
        let schema_lock = if flock::is_on_nfs_mount(build_root) {
            if gctx.cli_unstable().fine_grain_locking {
                anyhow::bail!(
                    "fine-grained build locking is not supported on NFS build directories"
                );
            }
            None
        } else {
            Some(registry_root.join("locks").join(lock_path))
        };
        let _schema_lock_guard = schema_lock
            .as_ref()
            .map(SchemaLockGuard::new)
            .transpose()?;
        let schema = load_schema(&schema_path, source, stable_unit_id)?;
        let records = load_records(
            &records_dir,
            schema.as_ref().map(|schema| schema.generation),
        )?;
        let schema_missing = schema.is_none();
        let requires_initialization_lock = schema_missing && !isolated_by_dependency;
        let schema_stale = schema
            .as_ref()
            .is_some_and(|schema| {
                schema
                    .tracked_paths
                    .iter()
                    .any(|path| path.is_stale(package_root, build_root))
            });
        let values = schema
            .as_ref()
            .map(|schema| {
                variable_values(
                    schema.names.iter(),
                    env_config,
                    inherit_process_env,
                )
            })
            .unwrap_or_default();
        let matching_record = if schema_stale {
            None
        } else {
            let mut records = records
                .into_iter()
                .filter(|record| record.values == values)
                .collect::<Vec<_>>();
            if records.len() > 1 {
                let preferred = provisional_key(env_config, inherit_process_env);
                if let Some(index) = records.iter().position(|record| record.key == preferred) {
                    Some(records.swap_remove(index))
                } else {
                    records.into_iter().next()
                }
            } else {
                records.pop()
            }
        };
        let key = if schema_stale {
            provisional_key(env_config, inherit_process_env)
        } else if let Some(mut record) = matching_record {
            record.last_used = now();
            write_record(&records_dir, &record)?;
            record.key
        } else if values.is_empty() {
            0
        } else {
            variant_key(&values)
        };

        Ok(Self {
            key,
            requires_initialization_lock,
            needs_refresh: schema_missing || schema_stale,
            provisional: schema_stale,
            source,
            stable_unit_id,
            build_root: build_root.to_path_buf(),
            package_root: package_root.to_path_buf(),
            schema_path,
            records_dir,
            schema_lock,
            env_config: Arc::clone(env_config),
            inherit_process_env,
        })
    }

    pub fn is_branched(&self) -> bool {
        self.key != 0
    }

    pub fn requires_initialization_lock(&self) -> bool {
        self.requires_initialization_lock
    }

    pub fn needs_refresh(&self) -> bool {
        self.needs_refresh
    }

    pub fn is_provisional(&self) -> bool {
        self.provisional
    }

    pub fn hash(&self, hasher: &mut StableHasher) {
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
            .map(SchemaLockGuard::new)
            .transpose()?;
        let mut names = names.to_vec();
        names.sort();
        names.dedup();
        let observed_paths = paths
            .iter()
            .map(|path| self.track_path(path))
            .collect::<Vec<_>>();

        let previous = load_schema(&self.schema_path, self.source, self.stable_unit_id)?;
        let mut merged_names = previous
            .as_ref()
            .map(|schema| schema.names.clone())
            .unwrap_or_default();
        merged_names.extend(names);
        merged_names.sort();
        merged_names.dedup();

        let paths_invalidated = previous
            .as_ref()
            .is_some_and(|schema| {
                schema
                    .tracked_paths
                    .iter()
                    .any(|path| path.is_stale(&self.package_root, &self.build_root))
            });
        let mut tracked_paths = previous
            .as_ref()
            .map(|schema| schema.tracked_paths.clone())
            .unwrap_or_default();
        for path in &mut tracked_paths {
            path.stamp = file_stamp(&path.resolve(&self.package_root, &self.build_root));
        }
        for path in observed_paths {
            if !tracked_paths.iter().any(|tracked| tracked.same_path(&path)) {
                tracked_paths.push(path);
            }
        }
        tracked_paths.sort();
        tracked_paths.dedup();

        let names_expanded = previous
            .as_ref()
            .is_some_and(|schema| schema.names != merged_names);
        let changed = names_expanded || paths_invalidated;
        let generation = previous
            .as_ref()
            .map(|schema| schema.generation + u64::from(changed))
            .unwrap_or(0);
        let schema = InputSchema {
            version: FORMAT_VERSION,
            source: self.source,
            stable_unit_id: self.stable_unit_id.to_string(),
            generation,
            names: merged_names,
            tracked_paths,
        };

        if changed {
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
            key: self.key,
            values: variable_values(
                schema.names.iter(),
                &self.env_config,
                self.inherit_process_env,
            ),
            created: timestamp,
            last_used: timestamp,
        };
        write_record(&self.records_dir, &record)
    }

    fn track_path(&self, path: &Path) -> TrackedPath {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.package_root.join(path)
        };
        let (root, path) = if let Ok(path) = absolute.strip_prefix(&self.package_root) {
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

impl TrackedPath {
    fn resolve(&self, package_root: &Path, build_root: &Path) -> PathBuf {
        match self.root {
            TrackedRoot::Package => package_root.join(&self.path),
            TrackedRoot::Build => build_root.join(&self.path),
            TrackedRoot::Absolute => self.path.clone(),
        }
    }

    fn same_path(&self, other: &Self) -> bool {
        self.root == other.root && self.path == other.path
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

#[allow(clippy::disallowed_methods)]
fn provisional_key(
    env_config: &Arc<HashMap<String, OsString>>,
    inherit_process_env: bool,
) -> u64 {
    let mut environment = BTreeMap::new();
    if inherit_process_env {
        environment.extend(env::vars_os());
    }
    environment.extend(
        env_config
            .iter()
            .map(|(name, value)| (OsString::from(name), value.clone())),
    );
    let mut hasher = StableHasher::new();
    environment.hash(&mut hasher);
    Hasher::finish(&hasher).max(1)
}

struct SchemaLockGuard(File);

impl SchemaLockGuard {
    fn new(path: &PathBuf) -> CargoResult<Self> {
        let file = flock::open_lock_file(path)?;
        flock::lock_exclusive(&file)?;
        Ok(Self(file))
    }
}

impl Drop for SchemaLockGuard {
    fn drop(&mut self) {
        if let Err(error) = flock::unlock(&self.0) {
            tracing::warn!("failed to release input variant schema lock: {error}");
        }
    }
}

fn load_schema(
    path: &Path,
    source: InputSource,
    stable_unit_id: UnitHash,
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
                && schema.stable_unit_id == stable_unit_id.to_string() => schema,
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
        records_dir.join(format!("{:016x}.json", record.key)),
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

fn variable_values<'a>(
    names: impl IntoIterator<Item = &'a String>,
    env_config: &Arc<HashMap<String, OsString>>,
    inherit_process_env: bool,
) -> Vec<(String, EncodedValue)> {
    names
        .into_iter()
        .map(|name| {
            (
                name.clone(),
                encode_value(variable_value(name, env_config, inherit_process_env)),
            )
        })
        .collect()
}

#[allow(clippy::disallowed_methods)]
fn variable_value(
    name: &str,
    env_config: &Arc<HashMap<String, OsString>>,
    inherit_process_env: bool,
) -> Option<OsString> {
    env_config
        .get(name)
        .cloned()
        .or_else(|| inherit_process_env.then(|| env::var_os(name)).flatten())
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
