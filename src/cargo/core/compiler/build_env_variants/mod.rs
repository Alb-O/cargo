//! Retains build-script output branches selected by their observed environment.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use cargo_util::paths;
use serde::{Deserialize, Serialize};

use crate::util::data_structures::HashMap;
use crate::util::{CargoResult, StableHasher};

use super::UnitHash;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug)]
pub struct BuildEnvVariant {
    key: u64,
    registry_dir: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct VariantRecord {
    variables: Vec<(String, Option<String>)>,
}

impl BuildEnvVariant {
    pub fn select(
        build_root: &Path,
        package_name: &str,
        stable_unit_id: UnitHash,
        env_config: &Arc<HashMap<String, OsString>>,
    ) -> CargoResult<Self> {
        let registry_dir = build_root
            .join(".build-env-variants")
            .join(package_name)
            .join(stable_unit_id.to_string());
        let records = load_records(&registry_dir)?;

        if let Some(key) = records.iter().find_map(|(key, record)| {
            (record.variables == variable_values(record.variables.iter().map(|(name, _)| name), env_config))
                .then_some(*key)
        }) {
            return Ok(Self {
                key,
                registry_dir,
            });
        }

        let mut names = records
            .iter()
            .flat_map(|(_, record)| record.variables.iter().map(|(name, _)| name.clone()))
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();

        let key = if names.is_empty() {
            0
        } else {
            variant_key(&variable_values(names.iter(), env_config))
        };

        Ok(Self {
            key,
            registry_dir,
        })
    }

    pub fn is_branched(&self) -> bool {
        self.key != 0
    }

    pub fn hash(&self, hasher: &mut StableHasher) {
        if self.key != 0 {
            self.key.hash(hasher);
        }
    }

    pub fn record(
        &self,
        variable_names: &[String],
        env_config: &Arc<HashMap<String, OsString>>,
    ) -> CargoResult<()> {
        let mut variable_names = variable_names.to_vec();
        variable_names.sort();
        variable_names.dedup();
        if variable_names.is_empty() {
            if self.registry_dir.exists() {
                paths::remove_dir_all(&self.registry_dir)?;
            }
            return Ok(());
        }
        let record = VariantRecord {
            variables: variable_values(variable_names.iter(), env_config),
        };

        paths::create_dir_all(&self.registry_dir)?;
        let path = self.registry_dir.join(format!("{:016x}.json", self.key));
        paths::write_atomic(path, serde_json::to_vec(&record)?)
    }
}

fn load_records(registry_dir: &Path) -> CargoResult<Vec<(u64, VariantRecord)>> {
    let entries = match fs::read_dir(registry_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read `{}`", registry_dir.display()));
        }
    };
    let mut paths = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();

    paths
        .into_iter()
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .map(|path| {
            let key = path
                .file_stem()
                .and_then(OsStr::to_str)
                .and_then(|key| u64::from_str_radix(key, 16).ok())
                .with_context(|| format!("invalid build environment variant `{}`", path.display()))?;
            let record = serde_json::from_slice(&paths::read_bytes(&path)?)
                .with_context(|| format!("failed to load `{}`", path.display()))?;
            Ok((key, record))
        })
        .collect()
}

fn variable_values<'a>(
    names: impl IntoIterator<Item = &'a String>,
    env_config: &Arc<HashMap<String, OsString>>,
) -> Vec<(String, Option<String>)> {
    names
        .into_iter()
        .map(|name| (name.clone(), variable_value(name, env_config)))
        .collect()
}

#[allow(clippy::disallowed_methods)]
fn variable_value(name: &str, env_config: &Arc<HashMap<String, OsString>>) -> Option<String> {
    if let Some(value) = env_config.get(name) {
        value.to_str().map(ToOwned::to_owned)
    } else {
        env::var(name).ok()
    }
}

fn variant_key(variables: &[(String, Option<String>)]) -> u64 {
    let mut hasher = StableHasher::new();
    variables.hash(&mut hasher);
    Hasher::finish(&hasher)
}
