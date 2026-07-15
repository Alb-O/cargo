//! Tests for running multiple `cargo` processes at the same time.

use std::fs;
use std::net::TcpListener;
use std::process::Stdio;
use std::sync::mpsc::channel;
use std::thread;
use std::{env, str};

use crate::prelude::*;
use crate::utils::cargo_process;
use cargo_test_support::git;
use cargo_test_support::install::assert_has_installed_exe;
use cargo_test_support::paths;
use cargo_test_support::registry::Package;
use cargo_test_support::str;
use cargo_test_support::{
    basic_lib_manifest, basic_manifest, execs, project, retry, sleep_ms, slow_cpu_multiplier,
};

fn pkg(name: &str, vers: &str) {
    Package::new(name, vers)
        .file("src/main.rs", "fn main() {{}}")
        .publish();
}

#[cargo_test]
fn multiple_installs() {
    let p = project()
        .no_manifest()
        .file("a/Cargo.toml", &basic_manifest("foo", "0.0.0"))
        .file("a/src/main.rs", "fn main() {}")
        .file("b/Cargo.toml", &basic_manifest("bar", "0.0.0"))
        .file("b/src/main.rs", "fn main() {}");
    let p = p.build();

    let mut a = p.cargo("install").cwd("a").build_command();
    let mut b = p.cargo("install").cwd("b").build_command();

    a.stdout(Stdio::piped()).stderr(Stdio::piped());
    b.stdout(Stdio::piped()).stderr(Stdio::piped());

    let a = a.spawn().unwrap();
    let b = b.spawn().unwrap();
    let a = thread::spawn(move || a.wait_with_output().unwrap());
    let b = b.wait_with_output().unwrap();
    let a = a.join().unwrap();

    execs().run_output(&a);
    execs().run_output(&b);

    assert_has_installed_exe(paths::cargo_home(), "foo");
    assert_has_installed_exe(paths::cargo_home(), "bar");
}

#[cargo_test]
fn concurrent_installs() {
    const LOCKED_BUILD: &str = "waiting for file lock on build directory";

    pkg("foo", "0.0.1");
    pkg("bar", "0.0.1");

    let mut a = cargo_process("install foo").build_command();
    let mut b = cargo_process("install bar").build_command();

    a.stdout(Stdio::piped()).stderr(Stdio::piped());
    b.stdout(Stdio::piped()).stderr(Stdio::piped());

    let a = a.spawn().unwrap();
    let b = b.spawn().unwrap();
    let a = thread::spawn(move || a.wait_with_output().unwrap());
    let b = b.wait_with_output().unwrap();
    let a = a.join().unwrap();

    assert!(!str::from_utf8(&a.stderr).unwrap().contains(LOCKED_BUILD));
    assert!(!str::from_utf8(&b.stderr).unwrap().contains(LOCKED_BUILD));

    execs().run_output(&a);
    execs().run_output(&b);

    assert_has_installed_exe(paths::cargo_home(), "foo");
    assert_has_installed_exe(paths::cargo_home(), "bar");
}

#[cargo_test]
fn one_install_should_be_bad() {
    let p = project()
        .no_manifest()
        .file("a/Cargo.toml", &basic_manifest("foo", "0.0.0"))
        .file("a/src/main.rs", "fn main() {}")
        .file("b/Cargo.toml", &basic_manifest("foo", "0.0.0"))
        .file("b/src/main.rs", "fn main() {}");
    let p = p.build();

    let mut a = p.cargo("install").cwd("a").build_command();
    let mut b = p.cargo("install").cwd("b").build_command();

    a.stdout(Stdio::piped()).stderr(Stdio::piped());
    b.stdout(Stdio::piped()).stderr(Stdio::piped());

    let a = a.spawn().unwrap();
    let b = b.spawn().unwrap();
    let a = thread::spawn(move || a.wait_with_output().unwrap());
    let b = b.wait_with_output().unwrap();
    let a = a.join().unwrap();

    execs().run_output(&a);
    execs().run_output(&b);

    assert_has_installed_exe(paths::cargo_home(), "foo");
}

#[cargo_test]
fn multiple_registry_fetches() {
    let mut pkg = Package::new("bar", "1.0.2");
    for i in 0..10 {
        let name = format!("foo{}", i);
        Package::new(&name, "1.0.0").publish();
        pkg.dep(&name, "*");
    }
    pkg.publish();

    let p = project()
        .no_manifest()
        .file(
            "a/Cargo.toml",
            r#"
                [package]
                name = "foo"
                authors = []
                version = "0.0.0"

                [dependencies]
                bar = "*"
            "#,
        )
        .file("a/src/main.rs", "fn main() {}")
        .file(
            "b/Cargo.toml",
            r#"
                [package]
                name = "bar"
                authors = []
                version = "0.0.0"

                [dependencies]
                bar = "*"
            "#,
        )
        .file("b/src/main.rs", "fn main() {}");
    let p = p.build();

    let mut a = p.cargo("build").cwd("a").build_command();
    let mut b = p.cargo("build").cwd("b").build_command();

    a.stdout(Stdio::piped()).stderr(Stdio::piped());
    b.stdout(Stdio::piped()).stderr(Stdio::piped());

    let a = a.spawn().unwrap();
    let b = b.spawn().unwrap();
    let a = thread::spawn(move || a.wait_with_output().unwrap());
    let b = b.wait_with_output().unwrap();
    let a = a.join().unwrap();

    execs().run_output(&a);
    execs().run_output(&b);

    let suffix = env::consts::EXE_SUFFIX;
    assert!(
        p.root()
            .join("a/target/debug")
            .join(format!("foo{}", suffix))
            .is_file()
    );
    assert!(
        p.root()
            .join("b/target/debug")
            .join(format!("bar{}", suffix))
            .is_file()
    );
}

#[cargo_test]
fn git_same_repo_different_tags() {
    let a = git::new("dep", |project| {
        project
            .file("Cargo.toml", &basic_manifest("dep", "0.5.0"))
            .file("src/lib.rs", "pub fn tag1() {}")
    });

    let repo = git2::Repository::open(&a.root()).unwrap();
    git::tag(&repo, "tag1");

    a.change_file("src/lib.rs", "pub fn tag2() {}");
    git::add(&repo);
    git::commit(&repo);
    git::tag(&repo, "tag2");

    let p = project()
        .no_manifest()
        .file(
            "a/Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo"
                    authors = []
                    version = "0.0.0"

                    [dependencies]
                    dep = {{ git = '{}', tag = 'tag1' }}
                "#,
                a.url()
            ),
        )
        .file(
            "a/src/main.rs",
            "extern crate dep; fn main() { dep::tag1(); }",
        )
        .file(
            "b/Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "bar"
                    authors = []
                    version = "0.0.0"

                    [dependencies]
                    dep = {{ git = '{}', tag = 'tag2' }}
                "#,
                a.url()
            ),
        )
        .file(
            "b/src/main.rs",
            "extern crate dep; fn main() { dep::tag2(); }",
        );
    let p = p.build();

    let mut a = p.cargo("build -v").cwd("a").build_command();
    let mut b = p.cargo("build -v").cwd("b").build_command();

    a.stdout(Stdio::piped()).stderr(Stdio::piped());
    b.stdout(Stdio::piped()).stderr(Stdio::piped());

    let a = a.spawn().unwrap();
    let b = b.spawn().unwrap();
    let a = thread::spawn(move || a.wait_with_output().unwrap());
    let b = b.wait_with_output().unwrap();
    let a = a.join().unwrap();

    execs().run_output(&a);
    execs().run_output(&b);
}

#[cargo_test]
fn git_same_branch_different_revs() {
    let a = git::new("dep", |project| {
        project
            .file("Cargo.toml", &basic_manifest("dep", "0.5.0"))
            .file("src/lib.rs", "pub fn f1() {}")
    });

    let p = project()
        .no_manifest()
        .file(
            "a/Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo"
                    authors = []
                    version = "0.0.0"

                    [dependencies]
                    dep = {{ git = '{}' }}
                "#,
                a.url()
            ),
        )
        .file(
            "a/src/main.rs",
            "extern crate dep; fn main() { dep::f1(); }",
        )
        .file(
            "b/Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "bar"
                    authors = []
                    version = "0.0.0"

                    [dependencies]
                    dep = {{ git = '{}' }}
                "#,
                a.url()
            ),
        )
        .file(
            "b/src/main.rs",
            "extern crate dep; fn main() { dep::f2(); }",
        );
    let p = p.build();

    // Generate a Cargo.lock pointing at the current rev, then clear out the
    // target directory
    p.cargo("build").cwd("a").run();
    fs::remove_dir_all(p.root().join("a/target")).unwrap();

    // Make a new commit on the master branch
    let repo = git2::Repository::open(&a.root()).unwrap();
    a.change_file("src/lib.rs", "pub fn f2() {}");
    git::add(&repo);
    git::commit(&repo);

    // Now run both builds in parallel. The build of `b` should pick up the
    // newest commit while the build of `a` should use the locked old commit.
    let mut a = p.cargo("build").cwd("a").build_command();
    let mut b = p.cargo("build").cwd("b").build_command();

    a.stdout(Stdio::piped()).stderr(Stdio::piped());
    b.stdout(Stdio::piped()).stderr(Stdio::piped());

    let a = a.spawn().unwrap();
    let b = b.spawn().unwrap();
    let a = thread::spawn(move || a.wait_with_output().unwrap());
    let b = b.wait_with_output().unwrap();
    let a = a.join().unwrap();

    execs().run_output(&a);
    execs().run_output(&b);
}

#[cargo_test]
fn same_project() {
    let p = project()
        .file("src/main.rs", "fn main() {}")
        .file("src/lib.rs", "");
    let p = p.build();

    let mut a = p.cargo("build").build_command();
    let mut b = p.cargo("build").build_command();

    a.stdout(Stdio::piped()).stderr(Stdio::piped());
    b.stdout(Stdio::piped()).stderr(Stdio::piped());

    let a = a.spawn().unwrap();
    let b = b.spawn().unwrap();
    let a = thread::spawn(move || a.wait_with_output().unwrap());
    let b = b.wait_with_output().unwrap();
    let a = a.join().unwrap();

    execs().run_output(&a);
    execs().run_output(&b);
}

// Make sure that if Cargo dies while holding a lock that it's released and the
// next Cargo to come in will take over cleanly.
#[cargo_test]
fn killing_cargo_releases_the_lock() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                authors = []
                version = "0.0.0"
                build = "build.rs"
            "#,
        )
        .file("src/main.rs", "fn main() {}")
        .file(
            "build.rs",
            r#"
                use std::net::TcpStream;

                fn main() {
                    if std::env::var("A").is_ok() {
                        TcpStream::connect(&std::env::var("ADDR").unwrap()[..])
                                  .unwrap();
                        std::thread::sleep(std::time::Duration::new(10, 0));
                    }
                }
            "#,
        );
    let p = p.build();

    // Our build script will connect to our local TCP socket to inform us that
    // it's started  and that's how we know that `a` will have the lock
    // when we kill it.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut a = p.cargo("build").build_command();
    let mut b = p.cargo("build").build_command();
    a.stdout(Stdio::piped()).stderr(Stdio::piped());
    b.stdout(Stdio::piped()).stderr(Stdio::piped());
    a.env("ADDR", l.local_addr().unwrap().to_string())
        .env("A", "a");
    b.env("ADDR", l.local_addr().unwrap().to_string())
        .env_remove("A");

    // Spawn `a`, wait for it to get to the build script (at which point the
    // lock is held), then kill it.
    let mut a = a.spawn().unwrap();
    l.accept().unwrap();
    a.kill().unwrap();

    // Spawn `b`, then just finish the output of a/b the same way the above
    // tests does.
    let b = b.spawn().unwrap();
    let a = thread::spawn(move || a.wait_with_output().unwrap());
    let b = b.wait_with_output().unwrap();
    let a = a.join().unwrap();

    // We killed `a`, so it shouldn't succeed, but `b` should have succeeded.
    assert!(!a.status.success());
    execs().run_output(&b);
}

#[cargo_test]
fn debug_release_ok() {
    let p = project().file("src/main.rs", "fn main() {}");
    let p = p.build();

    p.cargo("build").run();
    fs::remove_dir_all(p.root().join("target")).unwrap();

    let mut a = p.cargo("build").build_command();
    let mut b = p.cargo("build --release").build_command();
    a.stdout(Stdio::piped()).stderr(Stdio::piped());
    b.stdout(Stdio::piped()).stderr(Stdio::piped());
    let a = a.spawn().unwrap();
    let b = b.spawn().unwrap();
    let a = thread::spawn(move || a.wait_with_output().unwrap());
    let b = b.wait_with_output().unwrap();
    let a = a.join().unwrap();

    execs()
        .with_stderr_data(str![[r#"
...
[COMPILING] foo v0.0.1 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run_output(&a);
    execs()
        .with_stderr_data(str![[r#"
...
[COMPILING] foo v0.0.1 ([ROOT]/foo)
[FINISHED] `release` profile [optimized] target(s) in [ELAPSED]s

"#]])
        .run_output(&b);
}

#[cargo_test]
fn no_deadlock_with_git_dependencies() {
    let dep1 = git::new("dep1", |project| {
        project
            .file("Cargo.toml", &basic_manifest("dep1", "0.5.0"))
            .file("src/lib.rs", "")
    });

    let dep2 = git::new("dep2", |project| {
        project
            .file("Cargo.toml", &basic_manifest("dep2", "0.5.0"))
            .file("src/lib.rs", "")
    });

    let p = project()
        .file(
            "Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo"
                    authors = []
                    version = "0.0.0"

                    [dependencies]
                    dep1 = {{ git = '{}' }}
                    dep2 = {{ git = '{}' }}
                "#,
                dep1.url(),
                dep2.url()
            ),
        )
        .file("src/main.rs", "fn main() { }");
    let p = p.build();

    let n_concurrent_builds = 5;

    let (tx, rx) = channel();
    for _ in 0..n_concurrent_builds {
        let cmd = p
            .cargo("build")
            .build_command()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let tx = tx.clone();
        thread::spawn(move || {
            let result = cmd.unwrap().wait_with_output().unwrap();
            tx.send(result).unwrap()
        });
    }

    for _ in 0..n_concurrent_builds {
        let result = rx.recv_timeout(slow_cpu_multiplier(30)).expect("Deadlock!");
        execs().run_output(&result);
    }
}

#[cargo_test]
fn verbose_file_lock_blocking() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                edition = "2024"
            "#,
        )
        .file("src/main.rs", "fn main() {}")
        .file(
            "build.rs",
            r#"
                fn main() {
                    let blocking = std::env::var("BLOCKING").unwrap_or_default();
                    if blocking == "1" {
                        std::fs::write("blocking", "").unwrap();
                        let path = std::path::Path::new("ready");
                        loop {
                            if path.exists() {
                                break;
                            } else {
                                std::thread::sleep(std::time::Duration::from_millis(100))
                            }
                        }
                    }
                }
            "#,
        )
        .build();

    // start a build that will hold the lock on the build directory
    let mut a = p
        .cargo("check")
        .build_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("BLOCKING", "1")
        .spawn()
        .unwrap();

    // wait for the build script to start
    retry(100, || p.root().join("blocking").exists().then_some(()));

    let blocked_p = p
        .cargo("check -vv")
        .build_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("BLOCKING", "0")
        .spawn()
        .unwrap();

    sleep_ms(500);

    // release the lock on the build directory
    std::fs::write(p.root().join("ready"), "").unwrap();

    let blocked_p_otpt = blocked_p.wait_with_output().unwrap();

    assert!(a.wait().unwrap().success());
    execs()
        .with_stderr_contains("[BLOCKING] waiting for file lock on build directory ([ROOT]/foo/target/debug/.cargo-build-lock)")
        .run_output(&blocked_p_otpt);
}

#[cargo_test]
fn fine_grain_builds_share_variants_and_artifact_destinations() {
    let build_script = r#"
        fn main() {
            use std::io::Write;

            let branch = std::env::var("BRANCH").unwrap();
            println!("cargo::rerun-if-env-changed=BRANCH");
            println!("cargo::rerun-if-changed=trigger");
            println!("cargo::rerun-if-changed=input-{branch}");
            println!("cargo::rustc-env=BRANCH_VALUE={branch}");

            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("runs-{branch}"))
                .unwrap()
                .write_all(b"x")
                .unwrap();
            std::fs::write(format!("started-{branch}"), "").unwrap();
            while !std::path::Path::new(&format!("ready-{branch}")).exists() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    "#;
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.0.0"
                edition = "2024"
            "#,
        )
        .file("build.rs", build_script)
        .file(
            "src/main.rs",
            r#"fn main() { println!("{}", env!("BRANCH_VALUE")); }"#,
        )
        .file("trigger", "initial")
        .file("input-A", "")
        .file("input-B", "")
        .build();

    let mut a = p.cargo("-Zfine-grain-locking run");
    a.masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .env("BRANCH", "A");
    let mut a = a.build_command();
    a.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut b = p.cargo("-Zfine-grain-locking run");
    b.masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .env("BRANCH", "B");
    let mut b = b.build_command();
    b.stdout(Stdio::piped()).stderr(Stdio::piped());

    let a = a.spawn().unwrap();
    let b = b.spawn().unwrap();
    retry(100, || {
        (p.root().join("started-A").exists() || p.root().join("started-B").exists())
            .then_some(())
    });
    sleep_ms(200);
    fs::write(p.root().join("ready-A"), "").unwrap();
    fs::write(p.root().join("ready-B"), "").unwrap();

    let a = a.wait_with_output().unwrap();
    let b = b.wait_with_output().unwrap();
    execs().run_output(&a);
    execs().run_output(&b);
    assert_eq!(str::from_utf8(&a.stdout).unwrap().trim(), "A");
    assert_eq!(str::from_utf8(&b.stdout).unwrap().trim(), "B");

    fs::remove_file(p.root().join("ready-A")).unwrap();
    fs::remove_file(p.root().join("ready-B")).unwrap();
    fs::remove_file(p.root().join("started-A")).unwrap();
    fs::remove_file(p.root().join("started-B")).unwrap();
    p.change_file("trigger", "dirty both variants");

    let mut a = p.cargo("-Zfine-grain-locking build");
    a.masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .env("BRANCH", "A");
    let mut a = a.build_command();
    a.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut b = p.cargo("-Zfine-grain-locking build");
    b.masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .env("BRANCH", "B");
    let mut b = b.build_command();
    b.stdout(Stdio::piped()).stderr(Stdio::piped());

    let a = a.spawn().unwrap();
    let b = b.spawn().unwrap();
    let mut both_started = false;
    for _ in 0..100 {
        if p.root().join("started-A").exists() && p.root().join("started-B").exists() {
            both_started = true;
            break;
        }
        sleep_ms(20);
    }
    fs::write(p.root().join("ready-A"), "").unwrap();
    fs::write(p.root().join("ready-B"), "").unwrap();

    let a = a.wait_with_output().unwrap();
    let b = b.wait_with_output().unwrap();
    execs().run_output(&a);
    execs().run_output(&b);
    assert!(both_started, "different retained variants did not run concurrently");
    assert_eq!(fs::read_to_string(p.root().join("runs-A")).unwrap(), "xx");
    assert_eq!(fs::read_to_string(p.root().join("runs-B")).unwrap(), "xx");
    let published = std::process::Command::new(p.bin("foo")).output().unwrap();
    assert!(published.status.success());
    let published_branch = str::from_utf8(&published.stdout).unwrap().trim();
    let dep_info = fs::read_to_string(p.root().join("target/debug/foo.d")).unwrap();
    assert!(dep_info.contains(&format!("input-{published_branch}")));

    for branch in ["A", "B"] {
        p.cargo("-Zfine-grain-locking run -v")
            .masquerade_as_nightly_cargo(&["fine-grain-locking"])
            .env("BRANCH", branch)
            .with_stdout_contains(branch)
            .with_stderr_does_not_contain("[COMPILING]")
            .with_stderr_does_not_contain("[RUNNING] `rustc [..]")
            .run();
    }

    fs::remove_file(p.root().join("ready-A")).unwrap();
    fs::remove_file(p.root().join("started-A")).unwrap();
    p.change_file("trigger", "dirty the same variant again");

    let mut first = p.cargo("-Zfine-grain-locking build");
    first
        .masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .env("BRANCH", "A");
    let mut first = first.build_command();
    first.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut second = p.cargo("-Zfine-grain-locking build");
    second
        .masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .env("BRANCH", "A");
    let mut second = second.build_command();
    second.stdout(Stdio::piped()).stderr(Stdio::piped());

    let first = first.spawn().unwrap();
    let second = second.spawn().unwrap();
    retry(100, || p.root().join("started-A").exists().then_some(()));
    fs::write(p.root().join("ready-A"), "").unwrap();

    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    execs().run_output(&first);
    execs().run_output(&second);
    assert_eq!(fs::read_to_string(p.root().join("runs-A")).unwrap(), "xxx");
    let published = std::process::Command::new(p.bin("foo")).output().unwrap();
    assert!(published.status.success());
    assert_eq!(str::from_utf8(&published.stdout).unwrap().trim(), "A");
    let dep_info = fs::read_to_string(p.root().join("target/debug/foo.d")).unwrap();
    assert!(dep_info.contains("input-A"));
}

#[cargo_test]
fn fine_grain_serializes_input_schema_expansion() {
    let p = project()
        .file("Cargo.toml", &basic_manifest("foo", "0.0.0"))
        .file(
            "build.rs",
            r#"
                fn main() {
                    println!("cargo::rerun-if-changed=trigger");
                    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
                    std::fs::write(root.join("started"), "").unwrap();
                    while !root.join("ready").exists() {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            "#,
        )
        .file("src/main.rs", "mod expanded; fn main() { println!(\"{}\", expanded::value()); }")
        .file("src/expanded.rs", "pub fn value() -> &'static str { \"base\" }")
        .file("trigger", "initial")
        .file("ready", "")
        .build();

    let mut initial = p.cargo("-Zfine-grain-locking run");
    initial.masquerade_as_nightly_cargo(&["fine-grain-locking"]);
    initial.with_stdout_data("base\n").run();

    fs::remove_file(p.root().join("ready")).unwrap();
    fs::remove_file(p.root().join("started")).unwrap();
    p.change_file("trigger", "expand the schema");
    p.change_file(
        "src/expanded.rs",
        r#"pub fn value() -> &'static str { env!("EXPANDED") }"#,
    );

    let mut a = p.cargo("-Zfine-grain-locking run");
    a.masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .env("EXPANDED", "A");
    let mut a = a.build_command();
    a.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut b = p.cargo("-Zfine-grain-locking run");
    b.masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .env("EXPANDED", "B");
    let mut b = b.build_command();
    b.stdout(Stdio::piped()).stderr(Stdio::piped());

    let a = a.spawn().unwrap();
    let b = b.spawn().unwrap();
    retry(100, || p.root().join("started").exists().then_some(()));
    sleep_ms(200);
    fs::write(p.root().join("ready"), "").unwrap();

    let a = a.wait_with_output().unwrap();
    let b = b.wait_with_output().unwrap();
    execs().run_output(&a);
    execs().run_output(&b);
    assert_eq!(str::from_utf8(&a.stdout).unwrap().trim(), "A");
    assert_eq!(str::from_utf8(&b.stdout).unwrap().trim(), "B");

    for expanded in ["A", "B"] {
        p.cargo("-Zfine-grain-locking run -v")
            .masquerade_as_nightly_cargo(&["fine-grain-locking"])
            .env("EXPANDED", expanded)
            .with_stdout_data(format!("{expanded}\n"))
            .with_stderr_does_not_contain("[RUNNING] `rustc [..]")
            .run();
    }
}

#[cargo_test]
#[cfg(target_os = "linux")]
fn fine_grain_queued_jobs_do_not_hold_lock_descriptors() {
    let mut builder = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.0.0"
                edition = "2024"
                [dependencies]
                crate0 = { path = "crate0" }
            "#,
        )
        .file("src/main.rs", "fn main() {}");

    for index in 0..32 {
        let dependency = if index == 31 {
            String::new()
        } else {
            format!(
                "[dependencies]\ncrate{} = {{ path = \"../crate{}\" }}",
                index + 1,
                index + 1
            )
        };
        builder = builder
            .file(
                &format!("crate{index}/Cargo.toml"),
                &format!(
                    r#"
                        [package]
                        name = "crate{index}"
                        version = "0.0.0"
                        edition = "2024"
                        {dependency}
                    "#,
                ),
            )
            .file(&format!("crate{index}/src/lib.rs"), "");
    }
    builder = builder.file(
        "crate31/build.rs",
        r#"
            fn main() {
                let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
                std::fs::write(root.join("started"), "").unwrap();
                while !root.join("ready").exists() {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        "#,
    );
    let p = builder.build();

    let mut command = p.cargo("-Zfine-grain-locking build -j1");
    command.masquerade_as_nightly_cargo(&["fine-grain-locking"]);
    let mut command = command.build_command();
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    retry(200, || p.root().join("started").exists().then_some(()));

    let descriptors = fs::read_dir(format!("/proc/{}/fd", child.id()))
        .unwrap()
        .count();
    fs::write(p.root().join("ready"), "").unwrap();
    let output = child.wait_with_output().unwrap();
    execs().run_output(&output);
    assert!(
        descriptors < 128,
        "queued jobs retained {descriptors} file descriptors"
    );
}

#[cargo_test]
fn fine_grain_rechecks_dependencies_after_waiting() {
    let p = project()
        .no_manifest()
        .file(
            "Cargo.toml",
            r#"
                [workspace]
                members = ["foo", "bar", "blocker"]
                resolver = "2"
            "#,
        )
        .file(
            "foo/Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.0.0"
                edition = "2024"
                [dependencies]
                bar = { path = "../bar" }
            "#,
        )
        .file("foo/src/main.rs", "fn main() { println!(\"{}\", bar::value()); }")
        .file("bar/Cargo.toml", &basic_lib_manifest("bar"))
        .file("bar/src/lib.rs", "pub fn value() -> &'static str { \"old\" }")
        .file("blocker/Cargo.toml", &basic_lib_manifest("blocker"))
        .file("blocker/src/lib.rs", "pub fn blocker() {}")
        .file(
            "blocker/build.rs",
            r#"
                fn main() {
                    println!("cargo::rerun-if-env-changed=BLOCK_RECHECK");
                    if std::env::var_os("BLOCK_RECHECK").is_none() {
                        return;
                    }
                    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .parent()
                        .unwrap();
                    std::fs::write(root.join("blocker-started"), "").unwrap();
                    while !root.join("blocker-ready").exists() {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            "#,
        )
        .build();

    let mut initial = p.cargo("-Zfine-grain-locking build -p blocker -p foo");
    initial
        .masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .run();

    let mut waiting = p.cargo("-Zfine-grain-locking build -p blocker -p foo -j1");
    waiting
        .masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .env("BLOCK_RECHECK", "1");
    let mut waiting = waiting.build_command();
    waiting.stdout(Stdio::piped()).stderr(Stdio::piped());
    let waiting = waiting.spawn().unwrap();
    retry(200, || {
        p.root().join("blocker-started").exists().then_some(())
    });

    fs::write(
        p.root().join("bar/src/lib.rs"),
        "pub fn value() -> &'static str { \"new\" }",
    )
    .unwrap();
    let mut dependency = p.cargo("-Zfine-grain-locking build -p bar");
    dependency
        .masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .run();

    fs::write(p.root().join("blocker-ready"), "").unwrap();
    let waiting = waiting.wait_with_output().unwrap();
    execs().run_output(&waiting);
    p.process(&p.bin("foo"))
        .with_stdout_data("new\n")
        .run();
}

#[cargo_test]
fn fine_grain_locks_preserve_metadata_pipelining() {
    let wrapper = project()
        .at("pipeline-wrapper")
        .file("Cargo.toml", &basic_manifest("pipeline-wrapper", "0.0.0"))
        .file(
            "src/main.rs",
            r#"
                use std::ffi::OsString;
                use std::process::Command;

                fn contains_rmeta(path: &std::path::Path) -> bool {
                    let Ok(entries) = std::fs::read_dir(path) else {
                        return false;
                    };
                    entries.filter_map(Result::ok).any(|entry| {
                        let path = entry.path();
                        if path.is_dir() {
                            contains_rmeta(&path)
                        } else {
                            path.extension().is_some_and(|extension| extension == "rmeta")
                        }
                    })
                }

                fn main() {
                    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
                    let rustc = args.remove(0);
                    let expanded = args
                        .iter()
                        .flat_map(|arg| {
                            let arg = arg.to_string_lossy();
                            if let Some(path) = arg.strip_prefix('@') {
                                std::fs::read_to_string(path)
                                    .unwrap()
                                    .lines()
                                    .map(OsString::from)
                                    .collect()
                            } else {
                                vec![OsString::from(arg.as_ref())]
                            }
                        })
                        .collect::<Vec<_>>();
                    let crate_name = expanded
                        .windows(2)
                        .find(|args| args[0] == "--crate-name")
                        .map(|args| args[1].to_string_lossy().into_owned());
                    let root = std::path::PathBuf::from(std::env::var_os("PIPELINE_ROOT").unwrap());

                    if crate_name.as_deref() == Some("foo") {
                        std::fs::write(root.join("foo-started"), "").unwrap();
                    }
                    if crate_name.as_deref() != Some("bar") {
                        let status = Command::new(rustc).args(args).status().unwrap();
                        std::process::exit(status.code().unwrap_or(1));
                    }

                    let mut child = Command::new(rustc).args(args).spawn().unwrap();
                    loop {
                        if contains_rmeta(&root.join("target")) {
                            std::fs::write(root.join("bar-rmeta"), "").unwrap();
                            while !root.join("ready-bar").exists() {
                                std::thread::sleep(std::time::Duration::from_millis(20));
                            }
                            break;
                        }
                        if let Some(status) = child.try_wait().unwrap() {
                            std::process::exit(status.code().unwrap_or(1));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    let status = child.wait().unwrap();
                    std::process::exit(status.code().unwrap_or(1));
                }
            "#,
        )
        .build();
    wrapper.cargo("build").run();

    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.0.0"
                edition = "2024"
                [dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file("src/lib.rs", "pub use bar::*;")
        .file("bar/Cargo.toml", &basic_lib_manifest("bar"))
        .file("bar/src/lib.rs", "pub fn bar() {}")
        .build();

    let mut command = p.cargo("-Zfine-grain-locking build -j2");
    command
        .masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .env("RUSTC_WRAPPER", wrapper.bin("pipeline-wrapper"))
        .env("PIPELINE_ROOT", p.root());
    let mut command = command.build_command();
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    retry(200, || p.root().join("bar-rmeta").exists().then_some(()));
    let mut pipelined = false;
    for _ in 0..100 {
        if p.root().join("foo-started").exists() {
            pipelined = true;
            break;
        }
        sleep_ms(20);
    }
    fs::write(p.root().join("ready-bar"), "").unwrap();
    let output = child.wait_with_output().unwrap();
    execs().run_output(&output);
    assert!(pipelined, "dependent rustc waited for the full upstream unit");
}

#[cargo_test]
fn fine_grain_holds_the_compatibility_artifact_lock() {
    let p = project()
        .file(
            "build.rs",
            r#"
                fn main() {
                    let kind = std::env::var("BUILD_KIND").unwrap();
                    println!("cargo::rerun-if-env-changed=BUILD_KIND");
                    std::fs::write(format!("started-{kind}"), "").unwrap();
                    while !std::path::Path::new(&format!("ready-{kind}")).exists() {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            "#,
        )
        .file("src/main.rs", "fn main() {}")
        .build();

    let mut fine = p.cargo("-Zfine-grain-locking build");
    fine.masquerade_as_nightly_cargo(&["fine-grain-locking"])
        .env("CARGO_BUILD_BUILD_DIR", p.root().join("build-fine"))
        .env("BUILD_KIND", "fine");
    let mut fine = fine.build_command();
    fine.stdout(Stdio::piped()).stderr(Stdio::piped());
    let fine = fine.spawn().unwrap();
    retry(200, || p.root().join("started-fine").exists().then_some(()));

    let mut legacy = p.cargo("build");
    legacy
        .env("CARGO_BUILD_BUILD_DIR", p.root().join("build-legacy"))
        .env("BUILD_KIND", "legacy");
    let mut legacy = legacy.build_command();
    legacy.stdout(Stdio::piped()).stderr(Stdio::piped());
    let legacy = legacy.spawn().unwrap();
    for _ in 0..50 {
        if p.root().join("started-legacy").exists() {
            break;
        }
        sleep_ms(20);
    }
    let compatibility_blocked = !p.root().join("started-legacy").exists();

    fs::write(p.root().join("ready-fine"), "").unwrap();
    let fine = fine.wait_with_output().unwrap();
    execs().run_output(&fine);
    retry(200, || p.root().join("started-legacy").exists().then_some(()));
    fs::write(p.root().join("ready-legacy"), "").unwrap();
    let legacy = legacy.wait_with_output().unwrap();
    execs().run_output(&legacy);
    assert!(compatibility_blocked, "legacy Cargo published concurrently");
}
