use clap_complete::engine::ArgValueCompleter;
use clap_complete::engine::CompletionCandidate;
use std::ffi::OsStr;
use std::path::Path;

use crate::command_prelude::*;

use cargo::{CargoResult, ops};

pub fn cli() -> Command {
    subcommand("generate-lockfile")
        .about("Generate the lockfile for a package")
        .arg_silent_suggestion()
        .arg_manifest_path()
        .arg(
            flag(
                "narrow",
                "Generate an exact lockfile for only the selected package roots",
            )
            .help_heading(heading::MANIFEST_OPTIONS),
        )
        .arg(
            multi_opt(
                "include-manifest",
                "PATH",
                "Add a package manifest as a narrowed lockfile root",
            )
            .requires("narrow")
            .help_heading(heading::MANIFEST_OPTIONS)
            .add(ArgValueCompleter::new(
                clap_complete::engine::PathCompleter::any().filter(|path: &Path| {
                    path.file_name() == Some(OsStr::new("Cargo.toml"))
                }),
            )),
        )
        .arg_ignore_rust_version_with_help("Ignore `rust-version` specification in packages")
        .arg(
            clap::Arg::new("publish-time")
                .long("publish-time")
                .value_name("yyyy-mm-ddThh:mm:ssZ")
                .add(ArgValueCompleter::new(datetime_completer))
                .help("Latest publish time allowed for registry packages (unstable)")
                .help_heading(heading::MANIFEST_OPTIONS)
        )
        .after_help(color_print::cstr!(
            "Run `<bright-cyan,bold>cargo help generate-lockfile</>` for more detailed information.\n"
        ))
}

fn datetime_completer(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let mut completions = vec![];
    let Some(current) = current.to_str() else {
        return completions;
    };

    if current.is_empty() {
        // While not likely what people want, it can at least give them a starting point to edit
        let timestamp = jiff::Timestamp::now();
        completions.push(CompletionCandidate::new(timestamp.to_string()));
    } else if let Ok(date) = current.parse::<jiff::civil::Date>() {
        if let Ok(zoned) = jiff::Zoned::default().with().date(date).build() {
            let timestamp = zoned.timestamp();
            completions.push(CompletionCandidate::new(timestamp.to_string()));
        }
    }
    completions
}

pub fn exec(gctx: &mut GlobalContext, args: &ArgMatches) -> CliResult {
    let publish_time = args.get_one::<String>("publish-time");
    let additional_manifests = values(args, "include-manifest")
        .iter()
        .map(|path| root_manifest(Some(Path::new(path)), gctx))
        .collect::<CargoResult<Vec<_>>>()?;
    let mut ws = args.workspace_with_additional_members(gctx, &additional_manifests)?;
    if let Some(publish_time) = publish_time {
        gctx.cli_unstable()
            .fail_if_stable_opt("--publish-time", 5221)?;
        let publish_time =
            cargo_util_schemas::index::parse_pubtime(publish_time).map_err(anyhow::Error::from)?;
        ws.set_resolve_publish_time(publish_time);
    }
    if args.flag("narrow") {
        let mut root_ids = Vec::new();
        if let Some(package) = ws.current_opt() {
            root_ids.push(package.package_id());
        }
        for manifest_path in &additional_manifests {
            let package_id = ws.load(manifest_path)?.package_id();
            if !root_ids.contains(&package_id) {
                root_ids.push(package_id);
            }
        }
        let specs = root_ids
            .into_iter()
            .map(|package_id| package_id.to_spec())
            .collect::<Vec<_>>();
        ops::generate_narrowed_lockfile(&ws, &specs)?;
    } else {
        ops::generate_lockfile(&ws)?;
    }
    Ok(())
}
