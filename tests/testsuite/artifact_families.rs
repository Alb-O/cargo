//! Tests for declarative shared artifact-family policy.

use crate::prelude::*;
use cargo_test_support::project;

#[cargo_test]
fn activates_dependency_feature_and_creates_a_separate_baseline() {
    let p = project()
        .file(
            ".cargo/config.toml",
            r#"
                [artifact-family.demo]
                trigger-dependency = "trigger"
                scope-package = "anchor"
                activate-dependency-features = ["trigger/family"]
                profiles = ["dev"]
                host-target-only = true
                resolver-baseline = ".cargo/baseline/Cargo.lock"
                environment-manifest = ".cargo/family-environment.json"
            "#,
        )
        .file(
            ".cargo/family-environment.json",
            r#"{
                "version": 1,
                "clear_inherited": false,
                "set": { "FAMILY_CANONICAL": "canonical" }
            }"#,
        )
        .file(
            "Cargo.toml",
            r#"
                [workspace]
                resolver = "2"
                members = ["app", "trigger", "anchor"]
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
                trigger = { path = "../trigger" }
            "#,
        )
        .file(
            "app/src/main.rs",
            r#"
                fn main() {
                    println!("{}:{}", trigger::family_value(), env!("FAMILY_CANONICAL"));
                }
            "#,
        )
        .file(
            "trigger/Cargo.toml",
            r#"
                [package]
                name = "trigger"
                version = "0.1.0"
                edition = "2024"

                [features]
                family = ["dep:anchor"]

                [dependencies]
                anchor = { path = "../anchor", optional = true }
            "#,
        )
        .file(
            "trigger/src/lib.rs",
            r#"
                #[cfg(feature = "family")]
                pub fn family_value() -> &'static str { anchor::value() }
            "#,
        )
        .file(
            "anchor/Cargo.toml",
            r#"
                [package]
                name = "anchor"
                version = "0.1.0"
                edition = "2024"
                build = "build.rs"
            "#,
        )
        .file(
            "anchor/build.rs",
            r#"
                fn main() {
                    println!(
                        "cargo::rustc-env=ANCHOR_FAMILY_ENV={}",
                        std::env::var("FAMILY_CANONICAL").unwrap()
                    );
                }
            "#,
        )
        .file(
            "anchor/src/lib.rs",
            "pub fn value() -> &'static str { env!(\"ANCHOR_FAMILY_ENV\") }",
        )
        .build();

    p.cargo("run -p app")
        .env("FAMILY_CANONICAL", "ambient-one")
        .with_stdout_contains("canonical:ambient-one")
        .run();
    assert!(p.root().join(".cargo/baseline/Cargo.lock").is_file());

    p.cargo("run -vv -p app")
        .env("FAMILY_CANONICAL", "ambient-two")
        .with_stdout_contains("canonical:ambient-two")
        .with_stderr_contains("[FRESH] anchor v0.1.0 ([ROOT]/foo/anchor)")
        .run();

    p.cargo("check -p app --without-artifact-family demo")
        .with_status(101)
        .with_stderr_contains(
            "error[E0425]: cannot find function `family_value` in crate `trigger`",
        )
        .run();
}

#[cargo_test]
fn inactive_optional_trigger_does_not_apply_the_family_patch() {
    let p = project()
        .file(
            ".cargo/config.toml",
            r#"
                [artifact-family.demo]
                trigger-dependency = "trigger"
                scope-package = "anchor"
                activate-dependency-features = ["trigger/family"]
                profiles = ["dev"]
                host-target-only = true
                resolver-baseline = ".cargo/baseline/Cargo.lock"
                environment-manifest = ".cargo/family-environment.json"

                [artifact-family.demo.patch.crates-io.anchor]
                path = "missing-anchor"
            "#,
        )
        .file(
            ".cargo/family-environment.json",
            r#"{ "version": 1, "clear_inherited": false }"#,
        )
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2024"

                [features]
                use-trigger = ["dep:trigger"]

                [dependencies]
                trigger = { path = "trigger", optional = true }
            "#,
        )
        .file("src/lib.rs", "pub fn value() {}")
        .file(
            "trigger/Cargo.toml",
            r#"
                [package]
                name = "trigger"
                version = "0.1.0"
                edition = "2024"

                [features]
                family = ["dep:anchor"]

                [dependencies]
                anchor = { version = "0.1", optional = true }
            "#,
        )
        .file("trigger/src/lib.rs", "pub fn value() {}")
        .build();

    p.cargo("check").run();
    assert!(!p.root().join(".cargo/baseline/Cargo.lock").exists());
}
