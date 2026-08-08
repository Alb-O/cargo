use std::ffi::OsString;
use std::sync::Arc;

use crate::util::data_structures::HashMap;

use super::{EncodedValue, FORMAT_VERSION, InputEnvironment, TrackedPath, TrackedRoot};
use super::{VariantRecord, encode_value, file_stamp, load_records, variant_key, write_record};

#[test]
fn variant_key_is_independent_of_declaration_order() {
    let env = Arc::new(
        [
            ("A".to_owned(), OsString::from("one")),
            ("B".to_owned(), OsString::from("two")),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>(),
    );
    let mut first = vec!["A".to_owned(), "B".to_owned()];
    let mut second = vec!["B".to_owned(), "A".to_owned()];
    first.sort();
    second.sort();
    let root = tempfile::tempdir().unwrap();
    let environment = InputEnvironment::new(&root.path().join("Cargo.toml"), &env, true);

    assert_eq!(
        variant_key(&environment.values(&first)),
        variant_key(&environment.values(&second))
    );
}

#[test]
fn values_distinguish_unset_empty_and_nonempty() {
    let unset = EncodedValue::Unset;
    let empty = encode_value(Some(OsString::new()));
    let nonempty = encode_value(Some(OsString::from("value")));

    assert_ne!(unset, empty);
    assert_ne!(empty, nonempty);
    assert_ne!(unset, nonempty);
}

#[cfg(unix)]
#[test]
fn non_utf8_values_have_stable_keys() {
    use std::os::unix::ffi::OsStringExt as _;

    let value = OsString::from_vec(vec![0x66, 0x80, 0x6f]);
    assert_eq!(encode_value(Some(value.clone())), encode_value(Some(value)));
}

#[test]
fn schema_generation_makes_old_records_ineligible() {
    let root = tempfile::tempdir().unwrap();
    let old = VariantRecord {
        version: FORMAT_VERSION,
        generation: 0,
        context_id: "context".to_owned(),
        key: 1,
        values: Vec::new(),
        tracked_paths: Vec::new(),
        created: 0,
        last_used: 0,
    };
    std::fs::write(
        root.path().join("old.json"),
        serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();

    assert!(load_records(root.path(), Some(1)).unwrap().is_empty());
    assert!(!root.path().join("old.json").exists());
}

#[test]
fn malformed_records_are_removed_as_cache_misses() {
    let root = tempfile::tempdir().unwrap();
    let malformed = root.path().join("malformed.json");
    std::fs::write(&malformed, b"not json").unwrap();

    assert!(load_records(root.path(), Some(0)).unwrap().is_empty());
    assert!(!malformed.exists());
}

#[test]
fn tracked_paths_only_invalidate_their_variant() {
    let root = tempfile::tempdir().unwrap();
    let first_path = root.path().join("first");
    let second_path = root.path().join("second");
    std::fs::write(&first_path, "first").unwrap();
    std::fs::write(&second_path, "second").unwrap();
    let record = |path: &std::path::Path| VariantRecord {
        version: FORMAT_VERSION,
        generation: 0,
        context_id: "context".to_owned(),
        key: 1,
        values: Vec::new(),
        tracked_paths: vec![TrackedPath {
            root: TrackedRoot::Absolute,
            path: path.to_path_buf(),
            stamp: file_stamp(path),
        }],
        created: 0,
        last_used: 0,
    };
    let first = record(&first_path);
    let second = record(&second_path);

    std::fs::remove_file(second_path).unwrap();

    assert!(!first.is_stale(root.path(), root.path()));
    assert!(second.is_stale(root.path(), root.path()));
}

#[test]
fn dependency_contexts_retain_separate_records() {
    let root = tempfile::tempdir().unwrap();
    let record = |context_id: &str| VariantRecord {
        version: FORMAT_VERSION,
        generation: 0,
        context_id: context_id.to_owned(),
        key: 1,
        values: Vec::new(),
        tracked_paths: Vec::new(),
        created: 0,
        last_used: 0,
    };

    write_record(root.path(), &record("first")).unwrap();
    write_record(root.path(), &record("second")).unwrap();

    assert_eq!(load_records(root.path(), Some(0)).unwrap().len(), 2);
}
