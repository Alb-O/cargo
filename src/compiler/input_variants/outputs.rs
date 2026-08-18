//! Ownership records for retained variant outputs.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use cargo_util::paths;
use serde::{Deserialize, Serialize};

use crate::util::CargoResult;

const OUTPUT_FORMAT_VERSION: u32 = 2;
const LEGACY_OUTPUT_FORMAT_VERSION: u32 = 1;

/// Complete filesystem ownership for one retained unit identity.
#[derive(Clone, Debug)]
pub struct OutputOwnership {
    build_root: PathBuf,
    package: String,
    unit: String,
    unit_dir: Option<PathBuf>,
    paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutputRecord {
    version: u32,
    package: String,
    unit: String,
    last_used: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unit_dir: Option<PathBuf>,
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
    owned: Vec<PathBuf>,
    changed: bool,
}

#[derive(Deserialize)]
struct LastUsedRecord {
    version: u32,
    last_used: u64,
}

impl OutputOwnership {
    pub fn new(
        build_root: &Path,
        target_root: &Path,
        package: &str,
        unit: impl ToString,
        unit_dir: Option<PathBuf>,
        paths: Vec<PathBuf>,
    ) -> CargoResult<Self> {
        let unit = unit.to_string();
        validate_ownership(
            build_root,
            target_root,
            package,
            &unit,
            unit_dir.as_deref(),
            &paths,
        )?;
        let paths = minimal_paths(paths.into_iter().filter(|path| {
            !unit_dir
                .as_ref()
                .is_some_and(|unit_dir| path.starts_with(unit_dir))
        }));
        Ok(Self {
            build_root: build_root.to_path_buf(),
            package: package.to_owned(),
            unit,
            unit_dir,
            paths,
        })
    }

    /// Records a cache hit and upgrades its ownership description when needed.
    pub fn mark_used(&self) -> CargoResult<()> {
        self.write(true)
    }

    /// Records outputs from a completed build and invalidates any measured size.
    pub fn record_build(&self) -> CargoResult<()> {
        self.write(false)
    }

    fn write(&self, preserve_size: bool) -> CargoResult<()> {
        let path = record_path(&self.build_root, &self.package, &self.unit);
        let size = if preserve_size {
            read_output_record(&path)?
                .filter(OutputRecord::is_supported)
                .filter(|record| record.has_ownership(self))
                .and_then(|record| record.size)
        } else {
            None
        };
        let record = OutputRecord {
            version: OUTPUT_FORMAT_VERSION,
            package: self.package.clone(),
            unit: self.unit.clone(),
            last_used: now(),
            size,
            unit_dir: self.unit_dir.clone(),
            paths: self.paths.clone(),
        };
        paths::create_dir_all(path.parent().expect("output record has a parent"))?;
        paths::write_atomic(path, serde_json::to_vec(&record)?)
    }
}

impl OutputRecord {
    fn is_supported(&self) -> bool {
        matches!(
            self.version,
            LEGACY_OUTPUT_FORMAT_VERSION | OUTPUT_FORMAT_VERSION
        )
    }

    fn has_ownership(&self, ownership: &OutputOwnership) -> bool {
        self.package == ownership.package
            && self.unit == ownership.unit
            && self.unit_dir == ownership.unit_dir
            && self.paths == ownership.paths
    }

    fn upgrade_ownership(&mut self, build_root: &Path) -> bool {
        if self.version != LEGACY_OUTPUT_FORMAT_VERSION || self.unit_dir.is_some() {
            return false;
        }
        let Some(unit_dir) = infer_unit_dir(build_root, &self.package, &self.unit, &self.paths)
        else {
            return false;
        };
        self.version = OUTPUT_FORMAT_VERSION;
        self.size = None;
        self.paths.retain(|path| !path.starts_with(&unit_dir));
        self.paths = minimal_paths(std::mem::take(&mut self.paths));
        self.unit_dir = Some(unit_dir);
        true
    }

    fn owned_paths(&self) -> Vec<PathBuf> {
        minimal_paths(self.unit_dir.iter().cloned().chain(self.paths.iter().cloned()))
    }
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
        let record = read_output_record(&path)?;
        let Some(mut record) = record.filter(OutputRecord::is_supported) else {
            if !dry_run {
                remove_file_if_exists(&path)?;
            }
            continue;
        };
        let upgraded = record.upgrade_ownership(build_root);
        validate_ownership(
            build_root,
            target_root,
            &record.package,
            &record.unit,
            record.unit_dir.as_deref(),
            &record.paths,
        )?;
        let owned = record.owned_paths();
        outputs.push(StoredOutput {
            path,
            record,
            owned,
            changed: upgraded,
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
                output.record.size = Some(output_size(&output.owned)?);
                output.changed = true;
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
                .map_or_else(|| output_size(&output.owned), Ok)?;
            retained_bytes = retained_bytes.saturating_sub(size);
            removed_bytes = removed_bytes.saturating_add(size);
            for owned in &output.owned {
                removed.push(owned.clone());
                if !dry_run {
                    remove_path_if_exists(owned)?;
                }
            }
            removed.push(output.path.clone());
            if !dry_run {
                remove_file_if_exists(&output.path)?;
            }
        } else if output.changed && !dry_run {
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
                record.version != super::FORMAT_VERSION
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

fn read_output_record(path: &Path) -> CargoResult<Option<OutputRecord>> {
    let bytes = match paths::read_bytes(path) {
        Ok(bytes) => bytes,
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
            error.kind() == std::io::ErrorKind::NotFound
        }) => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

fn infer_unit_dir(
    build_root: &Path,
    package: &str,
    unit: &str,
    paths: &[PathBuf],
) -> Option<PathBuf> {
    let mut found = None;
    for candidate in paths
        .iter()
        .flat_map(|path| path.ancestors())
        .filter(|path| path.starts_with(build_root))
        .filter(|path| unit_dir_matches(path, package, unit))
    {
        if found.as_deref().is_some_and(|found| found != candidate) {
            return None;
        }
        found = Some(candidate.to_path_buf());
    }
    found
}

fn unit_dir_matches(unit_dir: &Path, package: &str, unit: &str) -> bool {
    unit_dir.file_name() == Some(std::ffi::OsStr::new(unit))
        && unit_dir.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new(package))
        && unit_dir
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            == Some(std::ffi::OsStr::new("build"))
}

fn validate_ownership(
    build_root: &Path,
    target_root: &Path,
    package: &str,
    unit: &str,
    unit_dir: Option<&Path>,
    paths: &[PathBuf],
) -> CargoResult<()> {
    if let Some(unit_dir) = unit_dir {
        validate_paths(std::slice::from_ref(&unit_dir), &[build_root])?;
        if !unit_dir_matches(unit_dir, package, unit) {
            bail!(
                "retained unit directory `{}` does not match {package}/{unit}",
                unit_dir.display()
            );
        }
        if let Some(path) = paths
            .iter()
            .find(|path| unit_dir.starts_with(path.as_path()))
        {
            bail!(
                "retained sidecar `{}` contains unit directory `{}`",
                path.display(),
                unit_dir.display()
            );
        }
    }
    validate_paths(paths, &[build_root, target_root])
}

fn validate_paths(output_paths: &[impl AsRef<Path>], roots: &[&Path]) -> CargoResult<()> {
    for path in output_paths {
        let path = path.as_ref();
        if !path.is_absolute()
            || !roots
                .iter()
                .any(|root| path != *root && path.starts_with(root))
        {
            bail!("retained output `{}` is outside configured Cargo roots", path.display());
        }
    }
    Ok(())
}

fn minimal_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut paths = paths.into_iter().collect::<Vec<_>>();
    paths.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    paths.dedup();
    let mut roots = Vec::new();
    for path in paths {
        if !roots.iter().any(|root: &PathBuf| path.starts_with(root)) {
            roots.push(path);
        }
    }
    roots
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

    use super::{
        LEGACY_OUTPUT_FORMAT_VERSION, OUTPUT_FORMAT_VERSION, OutputOwnership, OutputRecord, clean,
        record_path, validate_paths,
    };

    fn ownership(
        build: &Path,
        target: &Path,
        unit: &str,
        unit_dir: Option<PathBuf>,
        paths: Vec<PathBuf>,
    ) -> OutputOwnership {
        OutputOwnership::new(build, target, "package", unit, unit_dir, paths).unwrap()
    }

    fn set_last_used(path: &Path, last_used: u64) {
        let mut record: OutputRecord = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        record.last_used = last_used;
        fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    }

    #[test]
    fn output_paths_must_stay_below_a_configured_root() {
        let root = Path::new("/cache/build");
        assert!(validate_paths(&[PathBuf::from("/cache/build/debug/unit")], &[root]).is_ok());
        assert!(validate_paths(&[PathBuf::from("/cache/build")], &[root]).is_err());
        assert!(validate_paths(&[PathBuf::from("/tmp/unit")], &[root]).is_err());
        assert!(validate_paths(&[PathBuf::from("relative/unit")], &[root]).is_err());

        let target = Path::new("/cache/target");
        let unit_dir = PathBuf::from("/cache/build/debug/build/package/unit");
        assert!(
            OutputOwnership::new(
                root,
                target,
                "package",
                "other-unit",
                Some(unit_dir.clone()),
                Vec::new(),
            )
            .is_err()
        );
        assert!(
            OutputOwnership::new(
                root,
                target,
                "package",
                "unit",
                Some(unit_dir),
                vec![PathBuf::from("/cache/build/debug/build/package")],
            )
            .is_err()
        );
    }

    #[test]
    fn expiration_dry_run_reports_without_removing() {
        let root = tempfile::tempdir().unwrap();
        let build = root.path().join("build");
        let target = root.path().join("target");
        let expired_output = target.join("debug/deps/expired");
        fs::create_dir_all(expired_output.parent().unwrap()).unwrap();
        fs::write(&expired_output, b"artifact").unwrap();
        ownership(
            &build,
            &target,
            "expired",
            None,
            vec![expired_output.clone()],
        )
        .record_build()
        .unwrap();
        let record_path = record_path(&build, "package", "expired");
        set_last_used(&record_path, 0);

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
        ownership(&build, &target, "old", None, vec![old_output.clone()])
            .record_build()
            .unwrap();
        ownership(&build, &target, "new", None, vec![new_output.clone()])
            .record_build()
            .unwrap();
        for (unit, last_used) in [("old", 1), ("new", 2)] {
            let path = record_path(&build, "package", unit);
            set_last_used(&path, last_used);
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
        ownership(
            &build,
            &target,
            "unit",
            None,
            vec![output_dir, output_file],
        )
        .record_build()
        .unwrap();
        let path = record_path(&build, "package", "unit");

        clean(&build, &target, None, Some(5), false).unwrap();
        let output_record: OutputRecord =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(output_record.size, Some(5));
    }

    #[test]
    fn complete_unit_directory_owns_unenumerated_outputs() {
        let root = tempfile::tempdir().unwrap();
        let build = root.path().join("build");
        let target = root.path().join("target");
        let unit_dir = build.join("debug/build/package/unit");
        let stdout = unit_dir.join("run/stdout");
        let unenumerated = unit_dir.join("out/auxiliary.o");
        let incremental = build.join("debug/incremental/input-variant-package-unit");
        fs::create_dir_all(stdout.parent().unwrap()).unwrap();
        fs::create_dir_all(unenumerated.parent().unwrap()).unwrap();
        fs::create_dir_all(&incremental).unwrap();
        fs::write(&stdout, b"stdout").unwrap();
        fs::write(&unenumerated, b"auxiliary").unwrap();
        fs::write(incremental.join("cache"), b"incremental").unwrap();

        ownership(
            &build,
            &target,
            "unit",
            Some(unit_dir.clone()),
            vec![stdout, unenumerated, incremental.clone()],
        )
        .record_build()
        .unwrap();
        let path = record_path(&build, "package", "unit");
        let record: OutputRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.version, OUTPUT_FORMAT_VERSION);
        assert_eq!(record.unit_dir.as_deref(), Some(unit_dir.as_path()));
        assert_eq!(record.paths, vec![incremental.clone()]);

        clean(&build, &target, Some(Duration::ZERO), None, false).unwrap();
        assert!(!unit_dir.exists());
        assert!(!incremental.exists());
    }

    #[test]
    fn legacy_record_infers_its_v2_unit_directory() {
        let root = tempfile::tempdir().unwrap();
        let build = root.path().join("build");
        let target = root.path().join("target");
        let unit_dir = build.join("debug/build/package/unit");
        let recorded = unit_dir.join("run/out/generated");
        let unrecorded = unit_dir.join("run/stdout");
        fs::create_dir_all(recorded.parent().unwrap()).unwrap();
        fs::write(&recorded, b"generated").unwrap();
        fs::write(&unrecorded, b"stdout").unwrap();
        let path = record_path(&build, "package", "unit");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = OutputRecord {
            version: LEGACY_OUTPUT_FORMAT_VERSION,
            package: "package".into(),
            unit: "unit".into(),
            last_used: 0,
            size: Some(1),
            unit_dir: None,
            paths: vec![recorded],
        };
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let report = clean(&build, &target, Some(Duration::ZERO), None, false).unwrap();
        assert!(report.removed.contains(&unit_dir));
        assert!(!unit_dir.exists());
        assert!(!path.exists());
    }

    #[test]
    fn fresh_use_upgrades_legacy_ownership_and_preserves_current_sizes() {
        let root = tempfile::tempdir().unwrap();
        let build = root.path().join("build");
        let target = root.path().join("target");
        let unit_dir = build.join("debug/build/package/unit");
        let incremental = build.join("debug/incremental/input-variant-package-unit");
        let path = record_path(&build, "package", "unit");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = OutputRecord {
            version: LEGACY_OUTPUT_FORMAT_VERSION,
            package: "package".into(),
            unit: "unit".into(),
            last_used: 1,
            size: Some(5),
            unit_dir: None,
            paths: vec![incremental.clone()],
        };
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let ownership = ownership(
            &build,
            &target,
            "unit",
            Some(unit_dir.clone()),
            vec![incremental],
        );

        ownership.mark_used().unwrap();
        let upgraded: OutputRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(upgraded.version, OUTPUT_FORMAT_VERSION);
        assert_eq!(upgraded.unit_dir, Some(unit_dir));
        assert_eq!(upgraded.size, None);

        let mut measured = upgraded;
        measured.size = Some(9);
        fs::write(&path, serde_json::to_vec(&measured).unwrap()).unwrap();
        ownership.mark_used().unwrap();
        let refreshed: OutputRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(refreshed.size, Some(9));

        ownership.record_build().unwrap();
        let rebuilt: OutputRecord = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(rebuilt.size, None);
    }

    #[test]
    fn current_input_variant_records_survive_size_collection() {
        let root = tempfile::tempdir().unwrap();
        let build = root.path().join("build");
        let target = root.path().join("target");
        let path = build.join(".input-variants/v1/records/rustc-env/package/record.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": super::super::FORMAT_VERSION,
                "last_used": 1,
            }))
            .unwrap(),
        )
        .unwrap();

        clean(&build, &target, None, Some(u64::MAX), false).unwrap();
        assert!(path.exists());
    }
}
