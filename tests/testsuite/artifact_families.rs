//! Tests for declarative shared artifact-family policy.

use crate::prelude::*;
use cargo_test_support::project;

fn family_dylib_count(root: &std::path::Path) -> usize {
    std::fs::read_dir(root.join("target/debug/deps"))
        .unwrap()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.contains("anchor-")
                && [".dll", ".dylib", ".so"]
                    .iter()
                    .any(|extension| name.ends_with(extension))
        })
        .count()
}

#[cargo_test]
fn activates_dependency_feature_in_a_canonical_environment() {
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

                [lib]
                crate-type = ["dylib"]
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
    assert_eq!(family_dylib_count(&p.root()), 1);
    p.cargo("run -vv -p app")
        .env("FAMILY_CANONICAL", "ambient-two")
        .with_stdout_contains("canonical:ambient-two")
        .with_stderr_contains("[FRESH] anchor v0.1.0 ([ROOT]/foo/anchor)")
        .run();

    std::fs::write(
        p.root().join(".cargo/family-environment.json"),
        r#"{
            "version": 1,
            "clear_inherited": false,
            "set": { "FAMILY_CANONICAL": "canonical-two" }
        }"#,
    )
    .unwrap();
    p.cargo("run -p app")
        .env("FAMILY_CANONICAL", "ambient-three")
        .with_stdout_contains("canonical-two:ambient-three")
        .run();
    assert_eq!(family_dylib_count(&p.root()), 2);

    std::fs::write(
        p.root().join(".cargo/family-environment.json"),
        r#"{
            "version": 1,
            "clear_inherited": false,
            "set": { "FAMILY_CANONICAL": "canonical" }
        }"#,
    )
    .unwrap();
    p.cargo("run -vv -p app")
        .env("FAMILY_CANONICAL", "ambient-four")
        .with_stdout_contains("canonical:ambient-four")
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
fn inactive_optional_trigger_does_not_activate_the_family() {
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
                environment-manifest = ".cargo/family-environment.json"
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
