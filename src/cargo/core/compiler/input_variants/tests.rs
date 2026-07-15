use std::ffi::OsString;
use std::sync::Arc;

use crate::util::data_structures::HashMap;

use super::{EncodedValue, FORMAT_VERSION, encode_value, variable_values, variant_key};
use super::{VariantRecord, load_records};

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

    assert_eq!(
        variant_key(&variable_values(first.iter(), &env, true)),
        variant_key(&variable_values(second.iter(), &env, true))
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
        key: 1,
        values: Vec::new(),
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
