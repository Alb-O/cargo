//! Tests for retaining build artifacts across build-script environments.

use crate::prelude::*;
use cargo_test_support::project;
use std::fs;

#[cargo_test]
fn reuses_observed_environment_branches_and_common_dependencies() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [workspace]
                resolver = "2"
                members = ["app", "fingerprinted", "shared"]
            "#,
        )
        .file(
            "shared/Cargo.toml",
            r#"
                [package]
                name = "shared"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file("shared/src/lib.rs", "pub fn marker() {}")
        .file(
            "fingerprinted/Cargo.toml",
            r#"
                [package]
                name = "fingerprinted"
                version = "0.1.0"
                edition = "2024"

                [build-dependencies]
                shared = { path = "../shared" }
            "#,
        )
        .file(
            "fingerprinted/build.rs",
            r#"
                fn main() {
                    shared::marker();
                    println!("cargo::rerun-if-env-changed=NATIVE_PATH");
                    let value = std::env::var("NATIVE_PATH").unwrap();
                    std::fs::write(
                        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
                            .join("native-path"),
                        value,
                    )
                    .unwrap();
                }
            "#,
        )
        .file(
            "fingerprinted/src/lib.rs",
            r#"
                pub fn native_path() -> &'static str {
                    include_str!(concat!(env!("OUT_DIR"), "/native-path"))
                }
            "#,
        )
        .file(
            "app/Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                fingerprinted = { path = "../fingerprinted" }
                shared = { path = "../shared" }
            "#,
        )
        .file(
            "app/src/main.rs",
            r#"
                fn main() {
                    shared::marker();
                    println!("{}", fingerprinted::native_path());
                }
            "#,
        )
        .build();

    p.cargo("run -vv -p app")
        .env("CARGO_INCREMENTAL", "1")
        .env("NATIVE_PATH", "lean")
        .with_stdout_contains("lean")
        .run();

    p.cargo("run -vv -p app")
        .env("CARGO_INCREMENTAL", "1")
        .env("NATIVE_PATH", "graphics")
        .with_stdout_contains("graphics")
        .with_stderr_contains("[FRESH] shared v0.1.0 ([ROOT]/foo/shared)")
        .with_stderr_contains("[COMPILING] fingerprinted v0.1.0 ([ROOT]/foo/fingerprinted)")
        .with_stderr_contains("[COMPILING] app v0.1.0 ([ROOT]/foo/app)")
        .run();

    p.cargo("run -vv -p app")
        .env("CARGO_INCREMENTAL", "1")
        .env("NATIVE_PATH", "lean")
        .with_stdout_contains("lean")
        .with_stderr_contains("[FRESH] shared v0.1.0 ([ROOT]/foo/shared)")
        .with_stderr_contains("[FRESH] fingerprinted v0.1.0 ([ROOT]/foo/fingerprinted)")
        .with_stderr_contains("[FRESH] app v0.1.0 ([ROOT]/foo/app)")
        .with_stderr_does_not_contain("[COMPILING]")
        .run();

    let incremental = p.root().join("target/debug/incremental");
    let variant_incremental_dirs = fs::read_dir(incremental)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("build-env-"))
        .count();
    assert_eq!(variant_incremental_dirs, 2);

    p.cargo("clean -p fingerprinted").run();
    assert!(!p
        .root()
        .join("target/.build-env-variants/fingerprinted")
        .exists());
    let fingerprinted_incremental = fs::read_dir(p.root().join("target/debug/incremental"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("build-env-fingerprinted-")
        })
        .count();
    assert_eq!(fingerprinted_incremental, 0);
}
