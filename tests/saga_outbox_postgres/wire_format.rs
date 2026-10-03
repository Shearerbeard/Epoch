//! The wire-format pins: serde variant names and `event_type` strings
//! are persisted rows, frozen here so a rename cannot drift them.

use std::num::NonZeroU32;

use epoch::decider::Event;
use epoch::streams::outbox::Record;

use super::support::{key_of, Fx, PortErr};

/// The record enum's serde variant names are the persisted wire
/// format: they are the JSONB payloads in `stream_events`, so a type
/// rename must leave them untouched. Frozen structural JSON for all
/// four variants.
#[test]
fn wire_format_record_variants_serialize_to_the_frozen_json() {
    let key = key_of("saga-1/orders/o-1/7/0".to_owned());
    let error = PortErr::flaky().text();
    let cases: Vec<(Record<Fx>, serde_json::Value)> = vec![
        (
            Record::Intent {
                intent: key.clone(),
                request: Fx::Export { item: 7 },
            },
            serde_json::json!({
                "Intent": {
                    "intent": "saga-1/orders/o-1/7/0",
                    "request": { "Export": { "item": 7 } },
                }
            }),
        ),
        (
            Record::Done {
                intent: key.clone(),
            },
            serde_json::json!({
                "Done": { "intent": "saga-1/orders/o-1/7/0" }
            }),
        ),
        (
            Record::Failed {
                intent: key.clone(),
                error: error.clone(),
            },
            serde_json::json!({
                "Failed": {
                    "intent": "saga-1/orders/o-1/7/0",
                    "error": "port failure: flaky",
                }
            }),
        ),
        (
            Record::Parked {
                intent: key,
                attempts: NonZeroU32::new(2).expect("2 is nonzero"),
                error,
            },
            serde_json::json!({
                "Parked": {
                    "intent": "saga-1/orders/o-1/7/0",
                    "attempts": 2,
                    "error": "port failure: flaky",
                }
            }),
        ),
    ];
    for (record, expected) in cases {
        assert_eq!(
            serde_json::to_value(&record).expect("a record serializes"),
            expected
        );
    }
}

/// The four `event_type()` strings name the rows' type column in
/// `stream_events`: wire format like the variant names, pinned so a
/// type rename cannot drift them.
#[test]
fn wire_format_event_type_strings_are_pinned() {
    let key = key_of("saga-1/orders/o-1/7/0".to_owned());
    let cases: Vec<(Record<Fx>, &str)> = vec![
        (
            Record::Intent {
                intent: key.clone(),
                request: Fx::Export { item: 7 },
            },
            "OutboxIntent",
        ),
        (
            Record::Done {
                intent: key.clone(),
            },
            "OutboxDone",
        ),
        (
            Record::Failed {
                intent: key.clone(),
                error: "e".to_owned(),
            },
            "OutboxFailed",
        ),
        (
            Record::Parked {
                intent: key,
                attempts: NonZeroU32::new(1).expect("1 is nonzero"),
                error: "e".to_owned(),
            },
            "OutboxParked",
        ),
    ];
    for (record, expected) in cases {
        assert_eq!(record.event_type(), expected);
    }
}
