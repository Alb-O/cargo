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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    paths: Vec<PathBuf>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct CleanReport {
    pub removed: Vec<PathBuf>,
    pub removed_bytes: u64,
}

struct StoredOutput {
    path: PathBuf,
    record: OutputRecord,
    measured: bool,
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
        size: None,
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

pub fn clean(
    build_root: &Path,
    target_root: &Path,
    max_age: Option<Duration>,
    max_size: Option<u64>,
    dry_run: bool,
) -> CargoResult<CleanReport> {
    let output_root = build_root.join(".input-variants/v1/outputs");
    let cutoff = max_age.map(|max_age| now().saturating_sub(max_age.as_secs()));
    let mut removed = Vec::new();
    let mut outputs = Vec::new();
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
        validate_paths(&record.paths, &[build_root, target_root])?;
        outputs.push(StoredOutput {
            path,
            record,
            measured: false,
        });
    }
    outputs.sort_by(|a, b| {
        a.record
            .last_used
            .cmp(&b.record.last_used)
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut retained_bytes = 0u64;
    if max_size.is_some() {
        for output in &mut outputs {
            if output.record.size.is_none() {
                output.record.size = Some(output_size(&output.record.paths)?);
                output.measured = true;
            }
            retained_bytes = retained_bytes.saturating_add(output.record.size.unwrap());
        }
    }
    let mut removed_bytes = 0u64;
    for output in outputs {
        let expired = cutoff.is_some_and(|cutoff| output.record.last_used <= cutoff);
        let oversized = max_size.is_some_and(|max_size| retained_bytes > max_size);
        if expired || oversized {
            let size = output
                .record
                .size
                .map_or_else(|| output_size(&output.record.paths), Ok)?;
            retained_bytes = retained_bytes.saturating_sub(size);
            removed_bytes = removed_bytes.saturating_add(size);
            for owned in &output.record.paths {
                removed.push(owned.clone());
                if !dry_run {
                    remove_path_if_exists(owned)?;
                }
            }
            removed.push(output.path.clone());
            if !dry_run {
                remove_file_if_exists(&output.path)?;
            }
        } else if output.measured && !dry_run {
            paths::write_atomic(output.path, serde_json::to_vec(&output.record)?)?;
        }
    }
    let records_root = build_root.join(".input-variants/v1/records");
    for path in json_files(&records_root)? {
        let record = paths::read_bytes(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LastUsedRecord>(&bytes).ok());
        let expired = match record {
            Some(record) => {
                record.version != FORMAT_VERSION
                    || cutoff.is_some_and(|cutoff| record.last_used <= cutoff)
            }
            None => true,
        };
        if expired {
            removed.push(path.clone());
            if !dry_run {
                remove_file_if_exists(&path)?;
            }
        }
    }
    Ok(CleanReport {
        removed,
        removed_bytes,
    })
}

fn output_size(output_paths: &[PathBuf]) -> CargoResult<u64> {
    let mut paths = output_paths.iter().map(PathBuf::as_path).collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    let mut roots = Vec::new();
    for path in paths {
        if !roots.iter().any(|root: &&Path| path.starts_with(root)) {
            roots.push(path);
        }
    }
    roots.into_iter().try_fold(0u64, |total, path| {
        let size = path_size(path)?;
        Ok(total.saturating_add(size))
    })
}

fn path_size(path: &Path) -> CargoResult<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read `{}`", path.display()));
        }
    };
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    walkdir::WalkDir::new(path)
        .into_iter()
        .try_fold(0u64, |total, entry| -> CargoResult<u64> {
            let metadata = entry?.metadata()?;
            Ok(if metadata.is_file() {
                total.saturating_add(metadata.len())
            } else {
                total
            })
        })
        .with_context(|| format!("failed to walk `{}`", path.display()))
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

    use super::{OutputRecord, clean, record, record_path, validate_paths};

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

        let reported = clean(
            &build,
            &target,
            Some(Duration::ZERO),
            None,
            true,
        )
        .unwrap();
        assert!(reported.removed.contains(&expired_output));
        assert!(expired_output.exists());
        assert!(record_path.exists());

        clean(
            &build,
            &target,
            Some(Duration::ZERO),
            None,
            false,
        )
        .unwrap();
        assert!(!expired_output.exists());
        assert!(!record_path.exists());
    }

    #[test]
    fn size_limit_removes_least_recently_used_outputs() {
        let root = tempfile::tempdir().unwrap();
        let build = root.path().join("build");
        let target = root.path().join("target");
        let old_output = target.join("debug/deps/old");
        let new_output = target.join("debug/deps/new");
        fs::create_dir_all(old_output.parent().unwrap()).unwrap();
        fs::write(&old_output, b"old!").unwrap();
        fs::write(&new_output, b"newest").unwrap();
        record(
            &build,
            "package",
            "old",
            vec![old_output.clone()],
            &[&build, &target],
        )
        .unwrap();
        record(
            &build,
            "package",
            "new",
            vec![new_output.clone()],
            &[&build, &target],
        )
        .unwrap();
        for (unit, last_used) in [("old", 1), ("new", 2)] {
            let path = record_path(&build, "package", unit);
            let mut output_record: OutputRecord =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            output_record.last_used = last_used;
            fs::write(path, serde_json::to_vec(&output_record).unwrap()).unwrap();
        }

        let reported = clean(&build, &target, None, Some(6), true).unwrap();
        assert_eq!(reported.removed_bytes, 4);
        assert!(reported.removed.contains(&old_output));
        assert!(old_output.exists());
        assert!(new_output.exists());

        clean(&build, &target, None, Some(6), false).unwrap();
        assert!(!old_output.exists());
        assert!(new_output.exists());
    }

    #[test]
    fn record_size_is_measured_without_counting_nested_paths_twice() {
        let root = tempfile::tempdir().unwrap();
        let build = root.path().join("build");
        let target = root.path().join("target");
        let output_dir = target.join("debug/incremental/unit");
        let output_file = output_dir.join("cache.bin");
        fs::create_dir_all(&output_dir).unwrap();
        fs::write(&output_file, b"cache").unwrap();
        record(
            &build,
            "package",
            "unit",
            vec![output_dir, output_file],
            &[&build, &target],
        )
        .unwrap();
        let path = record_path(&build, "package", "unit");

        clean(&build, &target, None, Some(5), false).unwrap();
        let output_record: OutputRecord =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(output_record.size, Some(5));
    }
}
