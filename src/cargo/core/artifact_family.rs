//! Declarative policy for shared heavy dependency subgraphs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use serde::Deserialize;
use cargo_util::paths;
use cargo_util_schemas::lockfile::TomlLockfile;

use crate::core::compiler::unit_graph::UnitGraph;
use crate::core::compiler::{BuildConfig, CompileKind, Unit};
use crate::core::resolver::features::CliFeatures;
use crate::core::resolver::encode::into_resolve;
use crate::core::{FeatureValue, Package, PackageId, PackageIdSpec, Resolve, Workspace};
use crate::util::context::CargoArtifactFamilyConfig;
use crate::util::{CargoResult, Filesystem, StableHasher};
use crate::util::data_structures::{HashMap, HashSet};
use crate::util::Graph;
use cargo_util::ProcessBuilder;

#[derive(Clone, Debug)]
pub struct ArtifactFamily {
    pub name: String,
    pub trigger_dependency: String,
    pub scope_package: String,
    pub resolver_baseline: PathBuf,
    pub environment: ArtifactEnvironment,
    pub context_key: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ArtifactEnvironment {
    pub version: u32,
    #[serde(default)]
    pub clear_inherited: bool,
    #[serde(default)]
    pub path: Vec<PathBuf>,
    #[serde(default)]
    pub set: BTreeMap<String, String>,
}

pub fn activate(
    ws: &Workspace<'_>,
    specs: &[PackageIdSpec],
    cli_features: &CliFeatures,
    build_config: &BuildConfig,
    disabled: &BTreeSet<String>,
) -> CargoResult<(CliFeatures, Vec<ArtifactFamily>)> {
    let configured: Option<BTreeMap<String, CargoArtifactFamilyConfig>> =
        ws.gctx().get("artifact-family")?;
    let Some(configured) = configured else {
        return Ok((cli_features.clone(), Vec::new()));
    };

    let selected = ws.members_with_features(specs, cli_features)?;
    let profile = build_config.requested_profile.as_str();
    let mut activated_features = BTreeSet::new();
    let mut families = Vec::new();
    let mut scope_packages = BTreeSet::new();
    let mut host_triple = None;

    for (name, config) in configured {
        validate_config(&name, &config)?;
        let environment_path = config.environment_manifest.resolve_path(ws.gctx());
        if !environment_path.is_file() {
            bail!(
                "artifact family `{name}` environment manifest `{}` does not exist",
                environment_path.display()
            );
        }
        if disabled.contains(&name) {
            continue;
        }
        if !config.profiles.is_empty() && !config.profiles.iter().any(|item| item == profile) {
            continue;
        }
        if config.host_target_only {
            let host = match host_triple {
                Some(host) => host,
                None => {
                    let host = ws.gctx().load_global_rustc(Some(ws))?.host;
                    host_triple = Some(host);
                    host
                }
            };
            if build_config.requested_kinds.iter().any(|kind| match kind {
                CompileKind::Host => false,
                CompileKind::Target(target) => target.rustc_target() != host,
            }) {
                continue;
            }
        }

        let matches = selected.iter().any(|(package, selected_features)| {
            package.dependencies().iter().any(|dependency| {
                let name_matches = dependency.name_in_toml().as_str() == config.trigger_dependency
                    || dependency.package_name().as_str() == config.trigger_dependency;
                name_matches
                    && (!dependency.is_optional()
                        || optional_dependency_enabled(
                            package,
                            selected_features,
                            dependency.name_in_toml().as_str(),
                        ))
            })
        });
        if !matches {
            continue;
        }
        if !scope_packages.insert(config.scope_package.clone()) {
            bail!("artifact families configure duplicate scope package `{}`", config.scope_package);
        }

        activated_features.extend(config.activate_dependency_features.iter().cloned());
        let environment_bytes = fs::read(&environment_path).with_context(|| {
            format!("failed to read artifact family environment `{}`", environment_path.display())
        })?;
        let environment: ArtifactEnvironment = serde_json::from_slice(&environment_bytes)
            .with_context(|| {
                format!("failed to parse artifact family environment `{}`", environment_path.display())
            })?;
        if environment.version != 1 {
            bail!("artifact family `{name}` uses unsupported environment version {}", environment.version);
        }

        let mut hasher = StableHasher::new();
        name.hash(&mut hasher);
        config.trigger_dependency.hash(&mut hasher);
        config.scope_package.hash(&mut hasher);
        config.activate_dependency_features.hash(&mut hasher);
        environment_bytes.hash(&mut hasher);
        let context_key = Hasher::finish(&hasher);
        families.push(ArtifactFamily {
            name,
            trigger_dependency: config.trigger_dependency,
            scope_package: config.scope_package,
            resolver_baseline: config.resolver_baseline.resolve_path(ws.gctx()),
            environment,
            context_key,
        });
    }

    let features = cli_features.with_added_features(activated_features.into_iter())?;
    Ok((features, families))
}

fn optional_dependency_enabled(
    package: &Package,
    selected: &CliFeatures,
    dependency: &str,
) -> bool {
    if selected.all_features {
        return true;
    }
    let feature_map = package.summary().features();
    let mut pending = selected.features.iter().cloned().collect::<Vec<_>>();
    if selected.uses_default_features && feature_map.contains_key("default") {
        pending.push(FeatureValue::Feature("default".into()));
    }
    let mut visited = BTreeSet::new();
    while let Some(feature) = pending.pop() {
        match feature {
            FeatureValue::Dep { dep_name } if dep_name.as_str() == dependency => return true,
            FeatureValue::DepFeature {
                dep_name,
                weak,
                ..
            } if dep_name.as_str() == dependency && !weak => return true,
            FeatureValue::Feature(name) if visited.insert(name) => {
                pending.extend(feature_map.get(&name).into_iter().flatten().cloned());
            }
            _ => {}
        }
    }
    false
}

fn validate_config(name: &str, config: &CargoArtifactFamilyConfig) -> CargoResult<()> {
    if config.trigger_dependency.is_empty() || config.scope_package.is_empty() {
        bail!("artifact family `{name}` requires nonempty trigger-dependency and scope-package");
    }
    if !config.resolver_baseline.raw_value().ends_with("Cargo.lock") {
        bail!("artifact family `{name}` resolver-baseline must end in `Cargo.lock`");
    }
    for feature in &config.activate_dependency_features {
        let Some((dependency, feature_name)) = feature.split_once('/') else {
            bail!("artifact family `{name}` feature `{feature}` must use dependency/feature syntax");
        };
        if dependency.is_empty() || feature_name.is_empty() || feature_name.contains('/') {
            bail!("artifact family `{name}` has invalid dependency feature `{feature}`");
        }
    }
    Ok(())
}

pub fn unit_membership(
    families: &[ArtifactFamily],
    unit_graph: &UnitGraph,
) -> CargoResult<HashMap<Unit, usize>> {
    let mut membership = HashMap::default();
    for (family_index, family) in families.iter().enumerate() {
        let roots = unit_graph
            .keys()
            .filter(|unit| unit.pkg.name().as_str() == family.scope_package)
            .cloned()
            .collect::<Vec<_>>();
        if roots.is_empty() {
            bail!(
                "artifact family `{}` activated but scope package `{}` is absent from the unit graph",
                family.name,
                family.scope_package
            );
        }
        let mut pending = roots;
        while let Some(unit) = pending.pop() {
            if let Some(previous) = membership.insert(unit.clone(), family_index) {
                if previous != family_index {
                    bail!("unit `{}` belongs to multiple artifact families", unit.pkg.name());
                }
                continue;
            }
            pending.extend(
                unit_graph
                    .get(&unit)
                    .into_iter()
                    .flatten()
                    .map(|dependency| dependency.unit.clone()),
            );
        }
    }
    Ok(membership)
}

pub fn apply_environment(family: &ArtifactFamily, command: &mut ProcessBuilder) -> CargoResult<()> {
    if family.environment.clear_inherited {
        command.env_clear();
        for name in ["HOME", "SCCACHE_DIR", "TERM", "TMPDIR"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
    }
    if !family.environment.path.is_empty() {
        command.env("PATH", std::env::join_paths(&family.environment.path)?);
    }
    for (name, value) in &family.environment.set {
        command.env(name, value);
    }
    Ok(())
}

pub fn load_resolver_baselines(
    ws: &Workspace<'_>,
    families: &[ArtifactFamily],
) -> CargoResult<Vec<(String, Resolve)>> {
    families
        .iter()
        .filter(|family| family.resolver_baseline.exists())
        .map(|family| {
            let contents = fs::read_to_string(&family.resolver_baseline).with_context(|| {
                format!(
                    "failed to read artifact family `{}` baseline `{}`",
                    family.name,
                    family.resolver_baseline.display()
                )
            })?;
            let lockfile: TomlLockfile = toml::from_str(&contents).with_context(|| {
                format!(
                    "failed to parse artifact family `{}` baseline `{}`",
                    family.name,
                    family.resolver_baseline.display()
                )
            })?;
            let resolve = into_resolve(lockfile, &contents, ws)?;
            Ok((family.name.clone(), resolve))
        })
        .collect()
}

pub fn ensure_resolver_baselines(
    ws: &Workspace<'_>,
    families: &[ArtifactFamily],
    resolve: &Resolve,
) -> CargoResult<()> {
    for family in families {
        if family.resolver_baseline.exists() {
            continue;
        }
        let parent = family
            .resolver_baseline
            .parent()
            .expect("resolver baseline has a parent");
        paths::create_dir_all(parent)?;
        let lock_root = Filesystem::new(parent.to_path_buf());
        let _lock = lock_root.open_rw_exclusive_create(
            ".artifact-family-baseline.lock",
            ws.gctx(),
            "artifact family resolver baseline",
        )?;
        if family.resolver_baseline.exists() {
            continue;
        }
        let family_resolve = resolver_closure(resolve, family)?;
        let contents = crate::ops::resolve_to_string(ws, &family_resolve)?;
        paths::write_atomic(&family.resolver_baseline, contents)?;
    }
    Ok(())
}

fn resolver_closure(resolve: &Resolve, family: &ArtifactFamily) -> CargoResult<Resolve> {
    let roots = resolve
        .iter()
        .filter(|id| id.name().as_str() == family.scope_package)
        .collect::<Vec<_>>();
    if roots.len() != 1 {
        bail!(
            "artifact family `{}` expected exactly one resolved `{}` package, found {}",
            family.name,
            family.scope_package,
            roots.len()
        );
    }

    let mut members = HashSet::default();
    let mut pending = roots;
    while let Some(id) = pending.pop() {
        if !members.insert(id) {
            continue;
        }
        pending.extend(resolve.deps(id).map(|(dependency, _)| dependency));
    }

    let mut graph = Graph::<PackageId, HashSet<crate::core::Dependency>>::new();
    for id in &members {
        graph.add(*id);
    }
    for id in &members {
        for (dependency, edges) in resolve.deps_not_replaced(*id) {
            if members.contains(&dependency) {
                *graph.link(*id, dependency) = edges.clone();
            }
        }
    }

    let replacements = resolve
        .replacements()
        .iter()
        .filter(|(from, to)| members.contains(from) && members.contains(to))
        .map(|(from, to)| (*from, *to))
        .collect();
    let features = members
        .iter()
        .map(|id| (*id, resolve.features(*id).to_vec()))
        .collect();
    let checksums = resolve
        .checksums()
        .iter()
        .filter(|(id, _)| members.contains(id))
        .map(|(id, checksum)| (*id, checksum.clone()))
        .collect();
    let summaries = members
        .iter()
        .map(|id| (*id, resolve.summary(*id).clone()))
        .collect();

    Ok(Resolve::new(
        graph,
        replacements,
        features,
        checksums,
        resolve.metadata().clone(),
        Vec::new(),
        resolve.version(),
        summaries,
    ))
}
