//! Tests for the `cargo generate-lockfile` command.

use std::fs;

use crate::prelude::*;
use cargo_test_support::compare::assert_e2e;
use cargo_test_support::registry::{Package, RegistryBuilder};
use cargo_test_support::{ProjectBuilder, basic_manifest, paths, project, str};

#[cargo_test]
fn adding_and_removing_packages() {
    let p = project()
        .file("src/main.rs", "fn main() {}")
        .file("bar/Cargo.toml", &basic_manifest("bar", "0.0.1"))
        .file("bar/src/lib.rs", "")
        .build();

    p.cargo("generate-lockfile").run();

    let lock1 = p.read_lockfile();

    // add a dep
    p.change_file(
        "Cargo.toml",
        r#"
            [package]
            name = "foo"
            authors = []
            version = "0.0.1"

            [dependencies.bar]
            path = "bar"
        "#,
    );
    p.cargo("generate-lockfile").run();
    let lock2 = p.read_lockfile();
    assert_ne!(lock1, lock2);

    // change the dep
    p.change_file("bar/Cargo.toml", &basic_manifest("bar", "0.0.2"));
    p.cargo("generate-lockfile").run();
    let lock3 = p.read_lockfile();
    assert_ne!(lock1, lock3);
    assert_ne!(lock2, lock3);

    // remove the dep
    println!("lock4");
    p.change_file(
        "Cargo.toml",
        r#"
            [package]
            name = "foo"
            authors = []
            version = "0.0.1"
        "#,
    );
    p.cargo("generate-lockfile").run();
    let lock4 = p.read_lockfile();
    assert_eq!(lock1, lock4);
}

fn path_dep_manifest(package: &str, dependency: &str) -> String {
    format!(
        r#"
            [package]
            name = "{package}"
            version = "0.1.0"
            edition = "2024"

            [dependencies]
            {dependency} = {{ path = "../{dependency}" }}
        "#
    )
}

#[cargo_test]
fn narrowed_lockfile_tracks_selected_roots() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [workspace]
                members = ["listed"]
                open-membership = true
                resolver = "3"

                [patch.crates-io]
                unused-patch = { path = "unused-patch" }
            "#,
        )
        .file("listed/Cargo.toml", &basic_manifest("listed", "0.1.0"))
        .file("listed/src/lib.rs", "")
        .file(
            "dynamic-a/Cargo.toml",
            &path_dep_manifest("dynamic-a", "dep-a"),
        )
        .file("dynamic-a/src/lib.rs", "pub fn value() { dep_a::value(); }")
        .file(
            "dynamic-b/Cargo.toml",
            &path_dep_manifest("dynamic-b", "dep-b"),
        )
        .file("dynamic-b/src/lib.rs", "pub fn value() { dep_b::value(); }")
        .file("dep-a/Cargo.toml", &basic_manifest("dep-a", "0.1.0"))
        .file("dep-a/src/lib.rs", "pub fn value() {}")
        .file("dep-b/Cargo.toml", &basic_manifest("dep-b", "0.1.0"))
        .file("dep-b/src/lib.rs", "pub fn value() {}")
        .file("dep-c/Cargo.toml", &basic_manifest("dep-c", "0.1.0"))
        .file("dep-c/src/lib.rs", "pub fn value() {}")
        .file(
            "unused-patch/Cargo.toml",
            &basic_manifest("unused-patch", "1.0.0"),
        )
        .file("unused-patch/src/lib.rs", "")
        .build();

    let generate = || {
        p.cargo("generate-lockfile --narrow --manifest-path Cargo.toml")
            .arg("--include-manifest")
            .arg("dynamic-a/Cargo.toml")
            .arg("--include-manifest")
            .arg("dynamic-b/Cargo.toml")
            .arg("--config")
            .arg("resolver.lockfile-path='segment/Cargo.lock'")
            .run();
    };
    generate();

    assert!(!p.root().join("Cargo.lock").exists());
    let lock = p.read_file("segment/Cargo.lock");
    assert!(lock.contains("cargo-narrowed-lockfile = \"1\""));
    for package in ["dynamic-a", "dynamic-b", "dep-a", "dep-b"] {
        assert!(lock.contains(&format!("name = \"{package}\"")));
    }
    for package in ["listed", "unused-patch", "dep-c"] {
        assert!(!lock.contains(&format!("name = \"{package}\"")));
    }

    p.change_file(
        "dynamic-a/Cargo.toml",
        &path_dep_manifest("dynamic-a", "dep-c"),
    );
    p.change_file("dynamic-a/src/lib.rs", "pub fn value() { dep_c::value(); }");
    generate();

    let lock = p.read_file("segment/Cargo.lock");
    assert!(!lock.contains("name = \"dep-a\""));
    assert!(lock.contains("name = \"dep-c\""));
    assert!(lock.contains("name = \"dynamic-b\""));

    for package in ["dynamic-a", "dynamic-b"] {
        p.cargo("check --locked")
            .arg("--manifest-path")
            .arg(format!("{package}/Cargo.toml"))
            .arg("--config")
            .arg("resolver.lockfile-path='segment/Cargo.lock'")
            .run();
    }

    p.cargo("check --manifest-path listed/Cargo.toml")
        .arg("--config")
        .arg("resolver.lockfile-path='segment/Cargo.lock'")
        .with_status(101)
        .with_stderr_data(str![[r#"
[ERROR] package `listed` is outside the graph recorded in narrowed lockfile `[ROOT]/foo/segment/Cargo.lock`

"#]])
        .run();

    p.change_file(
        "Cargo.toml",
        r#"
            [workspace]
            members = ["dynamic-a", "dynamic-b"]
            resolver = "3"
        "#,
    );
    p.cargo("check --workspace --locked")
        .arg("--config")
        .arg("resolver.lockfile-path='segment/Cargo.lock'")
        .run();
}

#[cargo_test]
fn narrowed_lockfile_requires_an_alternate_path() {
    let p = project().file("src/main.rs", "fn main() {}").build();

    p.cargo("generate-lockfile --narrow")
        .with_status(101)
        .with_stderr_data(str![[r#"
[ERROR] --narrow requires an alternate lockfile configured with `resolver.lockfile-path`

"#]])
        .run();
}

#[cargo_test]
fn narrowed_lockfile_rejects_an_unattached_manifest() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [workspace]
                members = []
            "#,
        )
        .file("dynamic/Cargo.toml", &basic_manifest("dynamic", "0.1.0"))
        .file("dynamic/src/lib.rs", "")
        .build();

    p.cargo("generate-lockfile --narrow --manifest-path Cargo.toml")
        .arg("--include-manifest")
        .arg("dynamic/Cargo.toml")
        .arg("--config")
        .arg("resolver.lockfile-path='segment/Cargo.lock'")
        .with_status(101)
        .with_stderr_data(str![[r#"
[ERROR] additional package manifest `[ROOT]/foo/dynamic/Cargo.toml` is not a member of workspace `[ROOT]/foo/Cargo.toml` and cannot attach through open membership

"#]])
        .run();
}

#[cargo_test]
fn no_index_update_sparse() {
    let _registry = RegistryBuilder::new().http_index().build();
    no_index_update(
        str![[r#"
[UPDATING] `dummy-registry` index
[LOCKING] 1 package to highest compatible version

"#]],
        str![[r#"
[LOCKING] 1 package to highest compatible version

"#]],
    );
}

#[cargo_test]
fn no_index_update_git() {
    no_index_update(
        str![[r#"
[UPDATING] `dummy-registry` index
[LOCKING] 1 package to highest compatible version

"#]],
        str![[r#"
[LOCKING] 1 package to highest compatible version

"#]],
    );
}

fn no_index_update(expected: impl IntoData, expected_unstable_option: impl IntoData) {
    Package::new("serde", "1.0.0").publish();

    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                authors = []
                version = "0.0.1"

                [dependencies]
                serde = "1.0"
            "#,
        )
        .file("src/main.rs", "fn main() {}")
        .build();

    p.cargo("generate-lockfile")
        .with_stderr_data(expected)
        .run();

    p.cargo("generate-lockfile -Zno-index-update")
        .masquerade_as_nightly_cargo(&["no-index-update"])
        .with_stdout_data("")
        .with_stderr_data(expected_unstable_option)
        .run();
}

#[cargo_test]
fn preserve_metadata() {
    let p = project()
        .file("src/main.rs", "fn main() {}")
        .file("bar/Cargo.toml", &basic_manifest("bar", "0.0.1"))
        .file("bar/src/lib.rs", "")
        .build();

    p.cargo("generate-lockfile").run();

    let metadata = r#"
[metadata]
bar = "baz"
foo = "bar"
"#;
    let lock = p.read_lockfile();
    let data = lock + metadata;
    p.change_file("Cargo.lock", &data);

    // Build and make sure the metadata is still there
    p.cargo("build").run();
    let lock = p.read_lockfile();
    assert!(lock.contains(metadata.trim()), "{}", lock);

    // Update and make sure the metadata is still there
    p.cargo("update").run();
    let lock = p.read_lockfile();
    assert!(lock.contains(metadata.trim()), "{}", lock);
}

#[cargo_test]
fn preserve_line_endings_issue_2076() {
    let p = project()
        .file("src/main.rs", "fn main() {}")
        .file("bar/Cargo.toml", &basic_manifest("bar", "0.0.1"))
        .file("bar/src/lib.rs", "")
        .build();

    let lockfile = p.root().join("Cargo.lock");
    p.cargo("generate-lockfile").run();
    assert!(lockfile.is_file());
    p.cargo("generate-lockfile").run();

    let lock0 = p.read_lockfile();

    assert!(lock0.starts_with("# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\n"));

    let lock1 = lock0.replace("\n", "\r\n");
    p.change_file("Cargo.lock", &lock1);

    p.cargo("generate-lockfile").run();

    let lock2 = p.read_lockfile();

    assert!(lock2.starts_with("# This file is automatically @generated by Cargo.\r\n# It is not intended for manual editing.\r\n"));
    assert_eq!(lock1, lock2);
}

#[cargo_test]
fn cargo_update_generate_lockfile() {
    let p = project().file("src/main.rs", "fn main() {}").build();

    let lockfile = p.root().join("Cargo.lock");
    assert!(!lockfile.is_file());
    p.cargo("update").with_stderr_data("").run();
    assert!(lockfile.is_file());

    fs::remove_file(p.root().join("Cargo.lock")).unwrap();

    assert!(!lockfile.is_file());
    p.cargo("update").with_stderr_data("").run();
    assert!(lockfile.is_file());
}

#[cargo_test]
fn duplicate_entries_in_lockfile() {
    let _a = ProjectBuilder::new(paths::root().join("a"))
        .file(
            "Cargo.toml",
            r#"
            [package]
            name = "a"
            authors = []
            version = "0.0.1"

            [dependencies]
            common = {path="common"}
            "#,
        )
        .file("src/lib.rs", "")
        .build();

    let common_toml = &basic_manifest("common", "0.0.1");

    let _common_in_a = ProjectBuilder::new(paths::root().join("a/common"))
        .file("Cargo.toml", common_toml)
        .file("src/lib.rs", "")
        .build();

    let b = ProjectBuilder::new(paths::root().join("b"))
        .file(
            "Cargo.toml",
            r#"
            [package]
            name = "b"
            authors = []
            version = "0.0.1"
            edition = "2015"

            [dependencies]
            common = {path="common"}
            a = {path="../a"}
            "#,
        )
        .file("src/lib.rs", "")
        .build();

    let _common_in_b = ProjectBuilder::new(paths::root().join("b/common"))
        .file("Cargo.toml", common_toml)
        .file("src/lib.rs", "")
        .build();

    // should fail due to a duplicate package `common` in the lock file
    b.cargo("build")
        .with_status(101)
        .with_stderr_data(str![[r#"
[ERROR] package collision in the lockfile: packages common v0.0.1 ([ROOT]/a/common) and common v0.0.1 ([ROOT]/b/common) are different, but only one can be written to lockfile unambiguously

"#]])
        .run();
}

#[cargo_test]
fn generate_lockfile_holds_lock_and_offline() {
    Package::new("syn", "1.0.0").publish();

    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"

                [dependencies]
                syn = "1.0"
            "#,
        )
        .file("src/lib.rs", "")
        .build();

    p.cargo("generate-lockfile")
        .with_stderr_data(str![[r#"
[UPDATING] `dummy-registry` index
[LOCKING] 1 package to highest compatible version

"#]])
        .run();

    p.cargo("generate-lockfile --offline")
        .with_stderr_data(str![[r#"
[LOCKING] 1 package to highest compatible version

"#]])
        .run();
}

#[cargo_test]
fn publish_time_feature_gated() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"

                [dependencies]
            "#,
        )
        .file("src/lib.rs", "")
        .build();

    p.cargo("generate-lockfile --publish-time 2025-03-01T06:00:00Z")
        .masquerade_as_nightly_cargo(&["publish-time"])
        .with_status(101)
        .with_stderr_data(str![[r#"
[ERROR] the `--publish-time` flag is unstable, pass `-Z unstable-options` to enable it
See https://github.com/rust-lang/cargo/issues/5221 for more information about the `--publish-time` flag.

"#]])
        .run();
}

#[cargo_test]
fn publish_time() {
    Package::new("has_time", "2025.1.1")
        .pubtime("2025-01-01T06:00:00Z")
        .publish();
    Package::new("has_time", "2025.6.1")
        .pubtime("2025-06-01T06:00:00Z")
        .publish();
    Package::new("no_time", "2025.1.1").publish();
    Package::new("no_time", "2025.6.1").publish();

    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"

                [dependencies]
                has_time = "2025.0"
                no_time = "2025.0"
            "#,
        )
        .file("src/lib.rs", "")
        .build();

    p.cargo("generate-lockfile --publish-time 2025-03-01T06:00:00Z -Zunstable-options")
        .masquerade_as_nightly_cargo(&["publish-time"])
        .with_stderr_data(str![[r#"
[UPDATING] `dummy-registry` index
[LOCKING] 2 packages to highest compatible versions as of 2025-03-01T06:00:00Z
[ADDING] has_time v2025.1.1 (available: v2025.6.1)

"#]])
        .run();

    let lock = p.read_lockfile();
    assert_e2e().eq(
        lock,
        str![[r##"
# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "foo"
version = "0.0.0"
dependencies = [
 "has_time",
 "no_time",
]

[[package]]
name = "has_time"
version = "2025.1.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "105ca3acbc796da3e728ff310cafc6961cebc48d0106285edb8341015b5cc2d7"

[[package]]
name = "no_time"
version = "2025.6.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "01e688c07975f1e85f526c033322273181a4d8fe97800543d813d0a0adc134e3"

"##]],
    );
}

#[cargo_test]
fn publish_time_no_candidates() {
    Package::new("has_time", "2025.6.1")
        .pubtime("2025-06-01T06:00:00Z")
        .publish();

    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"

                [dependencies]
                has_time = "2025.0"
            "#,
        )
        .file("src/lib.rs", "")
        .build();

    p.cargo("generate-lockfile --publish-time 2025-03-01T06:00:00Z -Zunstable-options")
        .masquerade_as_nightly_cargo(&["publish-time"])
        .with_status(101)
        .with_stderr_data(str![[r#"
[UPDATING] `dummy-registry` index
[ERROR] failed to select a version for the requirement `has_time = "^2025.0"`
  version 2025.6.1 is unavailable
location searched: `dummy-registry` index (which is replacing registry `crates-io`)
required by package `foo v0.0.0 ([ROOT]/foo)`

"#]])
        .run();
}

#[cargo_test]
fn publish_time_invalid() {
    Package::new("has_time", "2025.6.1")
        .pubtime("2025-06-01T06:00:00")
        .publish();

    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"

                [dependencies]
                has_time = "2025.0"
            "#,
        )
        .file("src/lib.rs", "")
        .build();

    p.cargo("generate-lockfile --publish-time 2025-03-01T06:00:00Z -Zunstable-options")
        .masquerade_as_nightly_cargo(&["publish-time"])
        .with_status(101)
        .with_stderr_data(str![[r#"
[UPDATING] `dummy-registry` index
[ERROR] failed to select a version for the requirement `has_time = "^2025.0"`
  version 2025.6.1's index entry is invalid
location searched: `dummy-registry` index (which is replacing registry `crates-io`)
required by package `foo v0.0.0 ([ROOT]/foo)`

"#]])
        .run();
}
