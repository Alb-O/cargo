//! Retains compilation branches selected by observed process inputs.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use cargo_util::paths;
use serde::{Deserialize, Serialize};

use crate::util::data_structures::HashMap;
use crate::util::{CargoResult, StableHasher};

use super::UnitHash;

pub mod outputs;

#[cfg(test)]
mod tests;

const FORMAT_VERSION: u32 = 1;

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
    source: InputSource,
    stable_unit_id: UnitHash,
    schema_path: PathBuf,
    records_dir: PathBuf,
    env_config: Arc<HashMap<String, OsString>>,
    inherit_process_env: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct InputSchema {
    version: u32,
    source: InputSource,
    stable_unit_id: String,
    generation: u64,
    names: Vec<String>,
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
        stable_unit_id: UnitHash,
        source: InputSource,
        env_config: &Arc<HashMap<String, OsString>>,
        inherit_process_env: bool,
    ) -> CargoResult<Self> {
        let registry_root = build_root.join(".input-variants").join("v1");
        let relative = Path::new(source.directory())
            .join(package_name)
            .join(stable_unit_id.to_string());
        let schema_path = registry_root
            .join("schemas")
            .join(&relative)
            .with_extension("json");
        let records_dir = registry_root.join("records").join(relative);
        let schema = load_schema(&schema_path, source, stable_unit_id)?;
        let records = load_records(&records_dir, schema.as_ref().map(|schema| schema.generation))?;

        if let Some(mut record) = records.into_iter().find(|record| {
            record.values
                == variable_values(
                    record.values.iter().map(|(name, _)| name),
                    env_config,
                    inherit_process_env,
                )
        }) {
            record.last_used = now();
            write_record(&records_dir, &record)?;
            return Ok(Self {
                key: record.key,
                source,
                stable_unit_id,
                schema_path,
                records_dir,
                env_config: Arc::clone(env_config),
                inherit_process_env,
            });
        }

        let names = schema.map(|schema| schema.names).unwrap_or_default();
        let key = if names.is_empty() {
            0
        } else {
            variant_key(&variable_values(names.iter(), env_config, inherit_process_env))
        };

        Ok(Self {
            key,
            source,
            stable_unit_id,
            schema_path,
            records_dir,
            env_config: Arc::clone(env_config),
            inherit_process_env,
        })
    }

    pub fn is_branched(&self) -> bool {
        self.key != 0
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
    ) -> CargoResult<()> {
        let mut names = names.to_vec();
        names.sort();
        names.dedup();

        if names.is_empty() {
            remove_if_exists(&self.schema_path)?;
            remove_dir_if_exists(&self.records_dir)?;
            return Ok(());
        }

        let previous = load_schema(&self.schema_path, self.source, self.stable_unit_id)?;
        let mut merged_names = previous
            .as_ref()
            .map(|schema| schema.names.clone())
            .unwrap_or_default();
        merged_names.extend(names);
        merged_names.sort();
        merged_names.dedup();

        let expanded = previous
            .as_ref()
            .is_some_and(|schema| schema.names != merged_names);
        let generation = previous
            .as_ref()
            .map(|schema| schema.generation + u64::from(expanded))
            .unwrap_or(0);
        let schema = InputSchema {
            version: FORMAT_VERSION,
            source: self.source,
            stable_unit_id: self.stable_unit_id.to_string(),
            generation,
            names: merged_names,
        };

        if expanded {
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
    Hasher::finish(&hasher)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
