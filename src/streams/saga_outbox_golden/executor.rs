//! The executor's golden fixtures.

use std::num::NonZeroU32;
use std::time::Duration;

use crate::streams::feed::{ConsumerGroup, EventFeed};
use crate::streams::outbox::{ExecutorError, ExecutorStep, Record};
use crate::streams::saga::RenderedIntentKey;
use crate::streams::EventMetadata;

use super::support::{
    intent_envelope, limit, rendered, stream_len, wait_until, Fx, Hook, Port, PortErr, Rig, SOURCE,
};

/// The executor performs an intent through the port - keyed, not
/// payload-matched - appends `Done`, and acks past the intent.
#[tokio::test]
async fn executor_performs_intents_and_appends_done() {
    let rig = Rig::new();
    let key = rendered("saga-9", SOURCE, "o-1", 1, 0);
    rig.seed_intent("saga-9", &key, Fx::Export { item: 30 })
        .await;
    let intent_position = rig.outbox_tip().await;

    let port = Port::default();
    let executor = rig.executor("saga-9", port.clone(), Hook::default(), 5);
    let step = executor.step(limit(10)).await.expect("the step succeeds");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: intent_position
        }
    );
    assert_eq!(
        &*port.calls.lock().unwrap(),
        &[(key.clone(), Fx::Export { item: 30 })],
        "the port sees the rendered key and the payload"
    );
    assert_eq!(
        rig.outbox_records("saga-9").await,
        vec![
            (
                Record::Intent {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    request: Fx::Export { item: 30 },
                },
                intent_envelope(&key)
            ),
            (
                Record::Done {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                },
                EventMetadata::new()
            ),
        ],
        "the intent stands and the done record lands after it"
    );

    // The done record itself is an outcome entry on the next poll:
    // skipped and acked, never re-performed.
    let done_position = rig.outbox_tip().await;
    let step = executor
        .step(limit(10))
        .await
        .expect("the second step succeeds");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: done_position
        }
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);
    assert_eq!(
        executor.step(limit(10)).await.expect("third step"),
        ExecutorStep::Idle
    );
}

/// Entries from another saga's stream in the shared category are
/// skipped and acked past, never performed.
#[tokio::test]
async fn executor_skips_foreign_sagas_streams() {
    let rig = Rig::new();
    let foreign_key = rendered("saga-a", SOURCE, "o-1", 1, 0);
    let own_key = rendered("saga-b", SOURCE, "o-1", 2, 0);
    rig.seed_intent("saga-a", &foreign_key, Fx::Export { item: 40 })
        .await;
    rig.seed_intent("saga-b", &own_key, Fx::Export { item: 41 })
        .await;
    let own_position = rig.outbox_tip().await;

    let port = Port::default();
    let executor = rig.executor("saga-b", port.clone(), Hook::default(), 5);
    let step = executor.step(limit(10)).await.expect("the step succeeds");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: own_position
        },
        "both entries are consumed: one skipped, one performed"
    );
    assert_eq!(
        &*port.calls.lock().unwrap(),
        &[(own_key.clone(), Fx::Export { item: 41 })],
        "the foreign intent is never performed"
    );
    assert_eq!(
        rig.outbox_records("saga-a").await.len(),
        1,
        "the foreign stream is untouched"
    );
    assert_eq!(
        rig.outbox_records("saga-b").await,
        vec![
            (
                Record::Intent {
                    intent: RenderedIntentKey::from_rendered(own_key.clone()),
                    request: Fx::Export { item: 41 },
                },
                intent_envelope(&own_key)
            ),
            (
                Record::Done {
                    intent: RenderedIntentKey::from_rendered(own_key),
                },
                EventMetadata::new()
            ),
        ]
    );
}

/// A failing effect appends `Failed` with the port's rendered error
/// and HOLDS the cursor at the intent: the next poll redelivers the
/// same intent and the port is called again.
#[tokio::test]
async fn a_failing_effect_appends_failed_and_holds_the_cursor() {
    let rig = Rig::new();
    let key = rendered("saga-11", SOURCE, "o-1", 1, 0);
    rig.seed_intent("saga-11", &key, Fx::Export { item: 50 })
        .await;

    let port = Port::default();
    port.fail_for.lock().unwrap().insert(key.clone(), 5);
    let executor = rig.executor("saga-11", port.clone(), Hook::default(), 3);

    let step = executor.step(limit(10)).await.expect("the step succeeds");
    assert_eq!(
        step,
        ExecutorStep::Holding {
            intent: RenderedIntentKey::from_rendered(key.clone())
        }
    );
    assert_eq!(
        rig.outbox_records("saga-11").await,
        vec![
            (
                Record::Intent {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    request: Fx::Export { item: 50 },
                },
                intent_envelope(&key)
            ),
            (
                Record::Failed {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    error: PortErr::flaky().text(),
                },
                EventMetadata::new()
            ),
        ]
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);

    // The held cursor redelivers the intent; the second attempt fails
    // again inside the budget, so the executor holds again.
    let step = executor
        .step(limit(10))
        .await
        .expect("the second step succeeds");
    assert_eq!(
        step,
        ExecutorStep::Holding {
            intent: RenderedIntentKey::from_rendered(key.clone())
        }
    );
    assert_eq!(port.calls.lock().unwrap().len(), 2);
    assert_eq!(
        rig.outbox_records("saga-11").await.len(),
        3,
        "intent, first failed, second failed"
    );
}

/// Budget exhaustion: the last failed attempt appends `Parked` with
/// the durable attempt count, fires the hook exactly once with the
/// notice, and acks past the intent permanently. The parked record
/// and the failed records after it are outcome entries - skipped and
/// acked, and the hook does not fire for them.
#[tokio::test]
async fn budget_exhaustion_parks_and_fires_the_hook_once() {
    let rig = Rig::new();
    let key = rendered("saga-12", SOURCE, "o-1", 1, 0);
    rig.seed_intent("saga-12", &key, Fx::Export { item: 60 })
        .await;
    let intent_position = rig.outbox_tip().await;

    let port = Port::default();
    port.fail_for.lock().unwrap().insert(key.clone(), 5);
    let hook = Hook::default();
    let executor = rig.executor("saga-12", port.clone(), hook.clone(), 1);

    let step = executor.step(limit(10)).await.expect("the step parks");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: intent_position
        },
        "the group advances past the parked intent permanently"
    );
    assert_eq!(
        rig.outbox_records("saga-12").await,
        vec![
            (
                Record::Intent {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    request: Fx::Export { item: 60 },
                },
                intent_envelope(&key)
            ),
            (
                Record::Failed {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    error: PortErr::flaky().text(),
                },
                EventMetadata::new()
            ),
            (
                Record::Parked {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    attempts: NonZeroU32::new(1).expect("1 is nonzero"),
                    error: PortErr::flaky().text(),
                },
                EventMetadata::new()
            ),
        ]
    );
    assert_eq!(
        &*hook.fires.lock().unwrap(),
        &[(key.clone(), 1, PortErr::flaky().text())],
        "the hook fires once, at park time, with the durable count"
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);

    // The parked record itself arrives on the next poll: an audit
    // fact, not a second trigger. Skipped, acked, no hook, no port.
    let parked_position = rig.outbox_tip().await;
    let step = executor
        .step(limit(10))
        .await
        .expect("the second step succeeds");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: parked_position
        }
    );
    assert_eq!(hook.fires.lock().unwrap().len(), 1);
    assert_eq!(port.calls.lock().unwrap().len(), 1);
    assert_eq!(rig.outbox_records("saga-12").await.len(), 3);
}

/// The crash between parking and ack, replayed: the parked record
/// already stands and the failed count is at the budget, so the
/// replay re-fires the idempotent hook, appends nothing, and acks
/// past. The port is never asked to perform past its budget.
#[tokio::test]
async fn a_replayed_park_refires_the_hook_and_appends_nothing() {
    let rig = Rig::new();
    let key = rendered("saga-13", SOURCE, "o-1", 1, 0);
    let parked_error = "port failure: seed";
    rig.seed_intent("saga-13", &key, Fx::Export { item: 70 })
        .await;
    rig.seed_failed("saga-13", &key, parked_error).await;
    rig.seed_failed("saga-13", &key, parked_error).await;
    rig.seed_parked("saga-13", &key, 2, parked_error).await;
    let parked_position = rig.outbox_tip().await;

    let port = Port::default();
    let hook = Hook::default();
    let executor = rig.executor("saga-13", port.clone(), hook.clone(), 2);

    let step = executor
        .step(limit(10))
        .await
        .expect("the replay acks as a no-op");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: parked_position
        },
        "the replay acks through the parked record"
    );
    assert_eq!(
        &*hook.fires.lock().unwrap(),
        &[(key.clone(), 2, parked_error.to_owned())],
        "the hook re-fires with the durable notice"
    );
    assert!(
        port.calls.lock().unwrap().is_empty(),
        "the budget is exhausted: no further perform"
    );
    assert_eq!(
        rig.outbox_records("saga-13").await.len(),
        4,
        "intent, two failed, parked - nothing appended on replay"
    );
    assert_eq!(
        executor.step(limit(10)).await.expect("second step"),
        ExecutorStep::Idle
    );
}

/// A hook that rejects the notice at park time: the parked record
/// stands, the cursor does not advance, and the next step re-fires
/// the hook - which is the idempotency obligation replaying - and
/// then acks without a second park.
#[tokio::test]
async fn a_rejecting_hook_leaves_the_park_standing_and_refires() {
    let rig = Rig::new();
    let key = rendered("saga-14", SOURCE, "o-1", 1, 0);
    rig.seed_intent("saga-14", &key, Fx::Export { item: 80 })
        .await;
    let intent_position = rig.outbox_tip().await;

    let port = Port::default();
    port.fail_for.lock().unwrap().insert(key.clone(), 5);
    let hook = Hook::default();
    *hook.reject_next.lock().unwrap() = true;
    let executor = rig.executor("saga-14", port.clone(), hook.clone(), 1);

    let error = executor
        .step(limit(10))
        .await
        .expect_err("the hook rejects the park");
    assert!(matches!(error, ExecutorError::Hook(_)));
    assert_eq!(
        rig.outbox_records("saga-14").await.len(),
        3,
        "intent, failed, parked - the park stands"
    );

    // The cursor never moved past the intent, so it redelivers.
    let entries = rig
        .outbox_feed
        .poll(
            &ConsumerGroup::new("saga-14").expect("a test group is nonempty"),
            limit(10),
        )
        .await
        .expect("probe poll succeeds");
    assert_eq!(
        entries.first().expect("the intent redelivers").position(),
        intent_position
    );

    let step = executor
        .step(limit(10))
        .await
        .expect("the re-fire succeeds");
    assert!(matches!(step, ExecutorStep::Advanced { .. }));
    assert_eq!(hook.fires.lock().unwrap().len(), 2);
    assert_eq!(
        rig.outbox_records("saga-14").await.len(),
        3,
        "no second park: the replay's append is the no-op"
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);
}

/// The crash between the last `Failed` and the `Parked` append,
/// replayed: the durable count is at the budget with no park on
/// record, so the replay parks NOW - `Parked` with the durable count
/// and the last recorded failure - fires the hook once, acks through,
/// and never performs past the budget.
#[tokio::test]
async fn a_crash_before_the_park_lands_parks_on_the_replay() {
    let rig = Rig::new();
    let key = rendered("saga-15", SOURCE, "o-1", 1, 0);
    let failed_error = "port failure: seed";
    rig.seed_intent("saga-15", &key, Fx::Export { item: 85 })
        .await;
    rig.seed_failed("saga-15", &key, failed_error).await;
    rig.seed_failed("saga-15", &key, failed_error).await;
    let failed_position = rig.outbox_tip().await;

    let port = Port::default();
    let hook = Hook::default();
    let executor = rig.executor("saga-15", port.clone(), hook.clone(), 2);

    let step = executor.step(limit(10)).await.expect("the replay parks");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: failed_position
        },
        "the park lands; the page's own entries ack through, the parked record arrives next poll"
    );
    let parked_position = rig.outbox_tip().await;
    assert!(parked_position.get() > failed_position.get());

    assert_eq!(
        &*hook.fires.lock().unwrap(),
        &[(key.clone(), 2, failed_error.to_owned())],
        "the hook fires once at park time with the durable count and last failure"
    );
    assert!(
        port.calls.lock().unwrap().is_empty(),
        "the budget is exhausted: no further perform"
    );
    assert_eq!(
        rig.outbox_records("saga-15").await,
        vec![
            (
                Record::Intent {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    request: Fx::Export { item: 85 },
                },
                intent_envelope(&key)
            ),
            (
                Record::Failed {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    error: failed_error.to_owned(),
                },
                EventMetadata::new()
            ),
            (
                Record::Failed {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    error: failed_error.to_owned(),
                },
                EventMetadata::new()
            ),
            (
                Record::Parked {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    attempts: NonZeroU32::new(2).expect("2 is nonzero"),
                    error: failed_error.to_owned(),
                },
                EventMetadata::new()
            ),
        ]
    );
    assert_eq!(
        executor
            .step(limit(10))
            .await
            .expect("the parked record skips"),
        ExecutorStep::Advanced {
            acked_to: parked_position
        }
    );
    assert_eq!(
        hook.fires.lock().unwrap().len(),
        1,
        "an audit fact, not a trigger"
    );
    assert_eq!(
        executor.step(limit(10)).await.expect("third step"),
        ExecutorStep::Idle
    );
}

/// The executor's poll loop drains the backlog: every intent is
/// performed and recorded, in order, until the loop is stopped.
#[tokio::test]
async fn executor_run_drains_the_backlog() {
    let rig = Rig::new();
    let key_a = rendered("saga-19", SOURCE, "o-1", 1, 0);
    let key_b = rendered("saga-19", SOURCE, "o-1", 2, 0);
    rig.seed_intent("saga-19", &key_a, Fx::Export { item: 90 })
        .await;
    rig.seed_intent("saga-19", &key_b, Fx::Notify { item: 91 })
        .await;

    let port = Port::default();
    let executor = rig.executor("saga-19", port.clone(), Hook::default(), 5);
    let task = tokio::spawn(async move {
        executor
            .run(limit(5), Duration::from_millis(10))
            .await
            .expect("the loop only stops by abort")
    });

    let outbox = rig.outbox.clone();
    let calls = port.clone();
    wait_until(|| {
        let outbox = outbox.clone();
        let calls = calls.clone();
        async move {
            stream_len(&outbox, "saga-19").await >= 4 && calls.calls.lock().unwrap().len() >= 2
        }
    })
    .await;

    assert_eq!(
        &*port.calls.lock().unwrap(),
        &[
            (key_a.clone(), Fx::Export { item: 90 }),
            (key_b.clone(), Fx::Notify { item: 91 })
        ],
        "intents perform in stream order"
    );
    let records = rig.outbox_records("saga-19").await;
    assert_eq!(records.len(), 4);
    assert!(matches!(records[2].0, Record::Done { .. }));
    assert!(matches!(records[3].0, Record::Done { .. }));
    task.abort();
}
