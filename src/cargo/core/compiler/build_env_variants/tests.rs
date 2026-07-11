use std::ffi::OsString;
use std::sync::Arc;

use crate::util::data_structures::HashMap;

use super::{VariantRecord, variable_values, variant_key};

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
        variant_key(&variable_values(first.iter(), &env)),
        variant_key(&variable_values(second.iter(), &env))
    );
}

#[test]
fn record_distinguishes_unset_and_empty_values() {
    let unset = VariantRecord {
        variables: vec![("VALUE".to_owned(), None)],
    };
    let empty = VariantRecord {
        variables: vec![("VALUE".to_owned(), Some(String::new()))],
    };

    assert_ne!(unset, empty);
}
