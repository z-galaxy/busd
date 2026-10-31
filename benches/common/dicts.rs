//! The body of the method calls of the `dicts` benchmarks: a dict of dicts of variants,
//! `a{sa{sv}}`, which is how `GetManagedObjects` and `InterfacesAdded` carry the properties of each
//! object.
//!
//! The bus checks the body of each message against its signature, which costs far more for a dict
//! of variants, as each variant has a signature of its own to check before its value can be, than
//! for a byte array, which takes no more than its length. The service replies without reading the
//! body, so that the timings are the bus's as far as they can be.

use std::collections::BTreeMap;

use zbus::{DynamicType, Variant};

/// The body of each call: the properties of an object, by the name of their interface.
pub type Dicts = BTreeMap<String, BTreeMap<String, Variant<'static>>>;

/// Makes the dicts of `INTERFACES` interfaces of `PROPERTIES` properties each, ordered by name, so
/// that every run sends the same bytes.
pub fn dicts() -> Dicts {
    let dicts: Dicts = (0..INTERFACES)
        .map(|i| {
            let properties = (0..PROPERTIES)
                .map(|j| (format!("Property{j}"), property(i, j)))
                .collect();

            (format!("org.dbus2.benchmark.Interface{i}"), properties)
        })
        .collect();
    // The signature follows from the types of the dicts. Checking it here, once, keeps a change of
    // them from having the bus check bodies of another signature than these benchmarks are about.
    assert_eq!(dicts.signature().to_string(), "a{sa{sv}}");

    dicts
}

/// The value of property `j` of interface `i`, which cycles through four types, as the properties
/// of real objects mix them: a number, a string, a boolean and an array of strings.
fn property(i: u32, j: u32) -> Variant<'static> {
    match j % 4 {
        0 => (i * PROPERTIES + j).into(),
        1 => format!("value {i}.{j}").into(),
        2 => (i < INTERFACES / 2).into(),
        _ => (0..3)
            .map(|k| format!("item {i}.{j}.{k}"))
            .collect::<Vec<_>>()
            .into(),
    }
}

/// How many interfaces the dicts of each call have.
const INTERFACES: u32 = 10;

/// How many properties each interface of the dicts has, each of which is a variant.
const PROPERTIES: u32 = 10;
