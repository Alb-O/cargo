use crate::command_prelude::*;
use crate::util::cache_lock::CacheLockMode;
use crate::util::time_span::parse_time_span;

use crate::util::data_structures::IndexSet;
use cargo::ops::CleanContext;
use cargo::ops::{self, CleanOptions};
use cargo::util::print_available_packages;
use cargo::workspace::gc::Gc;
use cargo::workspace::gc::GcOpts;
use cargo::workspace::gc::parse_human_size;
use cargo::workspace::global_cache_tracker::GlobalCacheTracker;
use clap_complete::ArgValueCandidates;
use std::time::Duration;

pub fn cli() -> Command {
    subcommand("clean")
        .about("Remove artifacts that cargo has generated in the past")
        .arg_doc("Whether or not to clean just the documentation directory")
        .arg_silent_suggestion()
        .arg_package_spec_simple(
            "Package to clean artifacts for",
            ArgValueCandidates::new(get_pkg_name_candidates),
        )
        .arg(
            flag("workspace", "Clean artifacts of the workspace members")
                .help_heading(heading::PACKAGE_SELECTION),
        )
        .arg_release("Whether or not to clean release artifacts")
        .arg_profile("Clean artifacts of the specified profile")
        .arg_target_triple("Target triple to clean output for")
        .arg_target_dir()
        .arg_manifest_path()
        .arg_dry_run("Display what would be deleted without deleting anything")
        .args_conflicts_with_subcommands(true)
        .subcommand(
            subcommand("variants")
                .about("Clean retained compilation variants")
                .arg_silent_suggestion()
                .arg_dry_run("Display what would be deleted without deleting anything")
                .group(
                    clap::ArgGroup::new("retention")
                        .args(["max-age", "max-size"])
                        .multiple(true)
                        .required(true),
                )
                .arg(
                    opt("build-dir", "Cargo build directory containing variant records")
                        .value_name("PATH")
                        .required(true),
                )
                .arg(
                    opt("target-dir", "Cargo target directory containing retained outputs")
                        .value_name("PATH")
                        .required(true),
                )
                .arg(
                    opt("max-age", "Delete variants unused for the given age")
                        .value_name("DURATION")
                        .value_parser(parse_time_span),
                )
                .arg(
                    opt(
                        "max-size",
                        "Delete least recently used variants until retained outputs are under the given size",
                    )
                    .value_name("SIZE")
                    .value_parser(parse_human_size),
                ),
        )
        .subcommand(
            subcommand("gc")
                .about("Clean global caches")
                .hide(true)
                .arg_silent_suggestion()
                .arg_dry_run("Display what would be deleted without deleting anything")
                // NOTE: Not all of these options may get stabilized. Some of them are
                // very low-level details, and may not be something typical users need.
                .arg(
                    opt(
                        "max-src-age",
                        "Deletes source cache files that have not been used \
                        since the given age (unstable)",
                    )
                    .value_name("DURATION")
                    .value_parser(parse_time_span),
                )
                .arg(
                    opt(
                        "max-crate-age",
                        "Deletes crate cache files that have not been used \
                        since the given age (unstable)",
                    )
                    .value_name("DURATION")
                    .value_parser(parse_time_span),
                )
                .arg(
                    opt(
                        "max-index-age",
                        "Deletes registry indexes that have not been used \
                        since the given age (unstable)",
                    )
                    .value_name("DURATION")
                    .value_parser(parse_time_span),
                )
                .arg(
                    opt(
                        "max-git-co-age",
                        "Deletes git dependency checkouts that have not been used \
                        since the given age (unstable)",
                    )
                    .value_name("DURATION")
                    .value_parser(parse_time_span),
                )
                .arg(
                    opt(
                        "max-git-db-age",
                        "Deletes git dependency clones that have not been used \
                        since the given age (unstable)",
                    )
                    .value_name("DURATION")
                    .value_parser(parse_time_span),
                )
                .arg(
                    opt(
                        "max-download-age",
                        "Deletes any downloaded cache data that has not been used \
                        since the given age (unstable)",
                    )
                    .value_name("DURATION")
                    .value_parser(parse_time_span),
                )
                .arg(
                    opt(
                        "max-src-size",
                        "Deletes source cache files until the cache is under the \
                        given size (unstable)",
                    )
                    .value_name("SIZE")
                    .value_parser(parse_human_size),
                )
                .arg(
                    opt(
                        "max-crate-size",
                        "Deletes crate cache files until the cache is under the \
                        given size (unstable)",
                    )
                    .value_name("SIZE")
                    .value_parser(parse_human_size),
                )
                .arg(
                    opt(
                        "max-git-size",
                        "Deletes git dependency caches until the cache is under \
                        the given size (unstable)",
                    )
                    .value_name("SIZE")
                    .value_parser(parse_human_size),
                )
                .arg(
                    opt(
                        "max-download-size",
                        "Deletes downloaded cache data until the cache is under \
                        the given size (unstable)",
                    )
                    .value_name("SIZE")
                    .value_parser(parse_human_size),
                ),
        )
        .after_help(color_print::cstr!(
            "Run `<bright-cyan,bold>cargo help clean</>` for more detailed information.\n"
        ))
}

pub fn exec(gctx: &mut GlobalContext, args: &ArgMatches) -> CliResult {
    match args.subcommand() {
        Some(("variants", args)) => {
            return clean_variants(gctx, args);
        }
        Some(("gc", args)) => {
            return gc(gctx, args);
        }
        Some((cmd, _)) => {
            unreachable!("unexpected command {}", cmd)
        }
        None => {}
    }

    let ws = args.workspace(gctx)?;

    if args.is_present_with_zero_values("package") {
        print_available_packages(&ws)?;
    }
    let mut spec = IndexSet::from_iter(values(args, "package"));

    if args.flag("workspace") {
        spec.extend(ws.members().map(|package| package.name().to_string()))
    };
    let opts = CleanOptions {
        gctx,
        spec,
        targets: args.targets()?,
        requested_profile: args.get_profile_name("dev", ProfileChecking::Custom)?,
        profile_specified: args.contains_id("profile") || args.flag("release"),
        doc: args.flag("doc"),
        dry_run: args.dry_run(),
        explicit_target_dir_arg: args.contains_id("target-dir"),
    };
    ops::clean(&ws, &opts)?;
    Ok(())
}

fn clean_variants(gctx: &GlobalContext, args: &ArgMatches) -> CliResult {
    let build_dir = args
        .value_of_path("build-dir", gctx)
        .expect("required --build-dir");
    let target_dir = args
        .value_of_path("target-dir", gctx)
        .expect("required --target-dir");
    let max_age = args.get_one::<Duration>("max-age").copied();
    let max_size = args.get_one::<u64>("max-size").copied();
    let build_root = cargo::util::Filesystem::new(build_dir.clone());
    let target_root = cargo::util::Filesystem::new(target_dir.clone());
    let _build_lock = build_root.open_rw_exclusive_create(
        ".cargo-input-variants-lock",
        gctx,
        "retained input variants",
    )?;
    let _target_lock = if target_root == build_root {
        None
    } else {
        Some(target_root.open_rw_exclusive_create(
            ".cargo-input-variants-lock",
            gctx,
            "retained input variants",
        )?)
    };
    let report = cargo::compiler::input_variants::outputs::clean(
        &build_dir,
        &target_dir,
        max_age,
        max_size,
        args.dry_run(),
    )?;
    let action = if args.dry_run() { "Would remove" } else { "Removed" };
    gctx.shell().status(
        action,
        format!(
            "{} retained variant paths ({:.1})",
            report.removed.len(),
            cargo::util::HumanBytes(report.removed_bytes)
        ),
    )?;
    Ok(())
}

fn gc(gctx: &GlobalContext, args: &ArgMatches) -> CliResult {
    gctx.cli_unstable().fail_if_stable_command(
        gctx,
        "clean gc",
        12633,
        "gc",
        gctx.cli_unstable().gc,
    )?;

    let size_opt = |opt| -> Option<u64> { args.get_one::<u64>(opt).copied() };
    let duration_opt = |opt| -> Option<Duration> { args.get_one::<Duration>(opt).copied() };
    let mut gc_opts = GcOpts {
        max_src_age: duration_opt("max-src-age"),
        max_crate_age: duration_opt("max-crate-age"),
        max_index_age: duration_opt("max-index-age"),
        max_git_co_age: duration_opt("max-git-co-age"),
        max_git_db_age: duration_opt("max-git-db-age"),
        max_src_size: size_opt("max-src-size"),
        max_crate_size: size_opt("max-crate-size"),
        max_git_size: size_opt("max-git-size"),
        max_download_size: size_opt("max-download-size"),
    };
    if let Some(age) = duration_opt("max-download-age") {
        gc_opts.set_max_download_age(age);
    }
    // If the user sets any options, then only perform the options requested.
    // If no options are set, do the default behavior.
    if !gc_opts.is_download_cache_opt_set() {
        gc_opts.update_for_auto_gc(gctx)?;
    }

    let _lock = gctx.acquire_package_cache_lock(CacheLockMode::MutateExclusive)?;
    let mut cache_track = GlobalCacheTracker::new(&gctx)?;
    let mut gc = Gc::new(gctx, &mut cache_track)?;
    let mut clean_ctx = CleanContext::new(gctx);
    clean_ctx.dry_run = args.dry_run();
    gc.gc(&mut clean_ctx, &gc_opts)?;
    clean_ctx.display_summary()?;
    Ok(())
}
