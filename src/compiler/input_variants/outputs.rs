//! Ownership records for retained variant outputs.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use cargo_util::paths;
use serde::{Deserialize, Serialize};

use crate::util::CargoResult;

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutputRecord {
    version: u32,
    package: String,
    unit: String,
    last_used: u64,
    paths: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct LastUsedRecord {
    version: u32,
    last_used: u64,
}

pub fn record(
    build_root: &Path,
    package: &str,
    unit: impl ToString,
    output_paths: Vec<PathBuf>,
    roots: &[&Path],
) -> CargoResult<()> {
    validate_paths(&output_paths, roots)?;
    let unit = unit.to_string();
    let record = OutputRecord {
        version: FORMAT_VERSION,
        package: package.to_owned(),
        unit: unit.clone(),
        last_used: now(),
        paths: output_paths,
    };
    let path = record_path(build_root, package, &unit);
    paths::create_dir_all(path.parent().expect("output record has a parent"))?;
    paths::write_atomic(path, serde_json::to_vec(&record)?)
}

pub fn touch(build_root: &Path, package: &str, unit: impl ToString) -> CargoResult<()> {
    let path = record_path(build_root, package, &unit.to_string());
    let bytes = match paths::read_bytes(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
            error.kind() == std::io::ErrorKind::NotFound
        }) => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut record: OutputRecord = match serde_json::from_slice::<OutputRecord>(&bytes) {
        Ok(record) if record.version == FORMAT_VERSION => record,
        _ => {
            remove_file_if_exists(&path)?;
            return Ok(());
        }
    };
    record.last_used = now();
    paths::write_atomic(path, serde_json::to_vec(&record)?)
}

pub fn clean_expired(
    build_root: &Path,
    target_root: &Path,
    max_age: Duration,
    dry_run: bool,
) -> CargoResult<Vec<PathBuf>> {
    let output_root = build_root.join(".input-variants/v1/outputs");
    let cutoff = now().saturating_sub(max_age.as_secs());
    let mut removed = Vec::new();
    for path in json_files(&output_root)? {
        let record = paths::read_bytes(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<OutputRecord>(&bytes).ok());
        let Some(record) = record.filter(|record| record.version == FORMAT_VERSION) else {
            if !dry_run {
                remove_file_if_exists(&path)?;
            }
            continue;
        };
        if record.last_used > cutoff {
            continue;
        }
        validate_paths(&record.paths, &[build_root, target_root])?;
        for owned in &record.paths {
            removed.push(owned.clone());
            if !dry_run {
                remove_path_if_exists(owned)?;
            }
        }
        removed.push(path.clone());
        if !dry_run {
            remove_file_if_exists(&path)?;
        }
    }
    let records_root = build_root.join(".input-variants/v1/records");
    for path in json_files(&records_root)? {
        let record = paths::read_bytes(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LastUsedRecord>(&bytes).ok());
        let expired = match record {
            Some(record) => record.version != FORMAT_VERSION || record.last_used <= cutoff,
            None => true,
        };
        if expired {
            removed.push(path.clone());
            if !dry_run {
                remove_file_if_exists(&path)?;
            }
        }
    }
    Ok(removed)
}

fn json_files(root: &Path) -> CargoResult<Vec<PathBuf>> {
    let mut pending = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read `{}`", directory.display()));
            }
        };
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "json") {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

fn record_path(build_root: &Path, package: &str, unit: &str) -> PathBuf {
    build_root
        .join(".input-variants/v1/outputs")
        .join(package)
        .join(format!("{unit}.json"))
}

pub fn validate_paths(output_paths: &[PathBuf], roots: &[&Path]) -> CargoResult<()> {
    for path in output_paths {
        if !path.is_absolute() || !roots.iter().any(|root| path.starts_with(root)) {
            bail!("retained output `{}` is outside configured Cargo roots", path.display());
        }
    }
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> CargoResult<()> {
    if path.is_dir() {
        remove_dir_if_exists(path)
    } else {
        remove_file_if_exists(path)
    }
}

fn remove_dir_if_exists(path: &Path) -> CargoResult<()> {
    if path.exists() {
        paths::remove_dir_all(path)?;
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> CargoResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{OutputRecord, clean_expired, record, record_path, validate_paths};

    #[test]
    fn output_paths_must_stay_below_a_configured_root() {
        let root = Path::new("/cache/build");
        assert!(validate_paths(&[PathBuf::from("/cache/build/debug/unit")], &[root]).is_ok());
        assert!(validate_paths(&[PathBuf::from("/tmp/unit")], &[root]).is_err());
        assert!(validate_paths(&[PathBuf::from("relative/unit")], &[root]).is_err());
    }

    #[test]
    fn expiration_dry_run_reports_without_removing() {
        let root = tempfile::tempdir().unwrap();
        let build = root.path().join("build");
        let target = root.path().join("target");
        let expired_output = target.join("debug/deps/expired");
        fs::create_dir_all(expired_output.parent().unwrap()).unwrap();
        fs::write(&expired_output, b"artifact").unwrap();
        record(
            &build,
            "package",
            "expired",
            vec![expired_output.clone()],
            &[&build, &target],
        )
        .unwrap();
        let record_path = record_path(&build, "package", "expired");
        let mut output_record: OutputRecord =
            serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        output_record.last_used = 0;
        fs::write(&record_path, serde_json::to_vec(&output_record).unwrap()).unwrap();

        let reported = clean_expired(&build, &target, Duration::ZERO, true).unwrap();
        assert!(reported.contains(&expired_output));
        assert!(expired_output.exists());
        assert!(record_path.exists());

        clean_expired(&build, &target, Duration::ZERO, false).unwrap();
        assert!(!expired_output.exists());
        assert!(!record_path.exists());
    }
}
