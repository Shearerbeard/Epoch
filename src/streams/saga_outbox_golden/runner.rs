//! The runner's golden fixtures.

use std::time::Duration;

use crate::streams::batch::{AtomicStreams, StreamRef};
use crate::streams::outbox::{Record, INTENT_METADATA_KEY, OUTBOX_CATEGORY};
use crate::streams::saga::{Command, EffectRequest, Error, Reaction, RunnerStep};
use crate::streams::{
    EventBatch, EventMetadata, ExpectedVersion, RecordedEvent, RenderedIntentKey,
};

use super::support::{
    intent_envelope, limit, rendered, stream_len, wait_until, Cmd, Fx, MemFold, Rig, Scripted, Src,
    LEDGER, SOURCE,
};

/// One event, two command arms on two streams and one effect arm: the
/// step commits every stream in one batch, acks the entry, and every
/// reaction record carries its minted key in its envelope.
#[tokio::test]
async fn runner_step_folds_reactions_into_one_batch_and_acks() {
    let rig = Rig::new();
    let position = rig.place(Src::Placed { item: 7 }).await;

    let saga = Scripted::new(
        "saga-1",
        vec![(
            Src::Placed { item: 7 },
            vec![
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-1".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Reserved { item: 7 },
                )),
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-2".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Charged { item: 7 },
                )),
                Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 7 })),
            ],
        )],
    );
    let fold = MemFold::default();
    let runner = rig.runner(saga, fold.clone());

    let step = runner.step(limit(10)).await.expect("the step succeeds");
    assert_eq!(
        step,
        RunnerStep::Advanced { acked_to: position },
        "the entry is processed and its position acked"
    );

    // Whole streams: the command records carry their own keys, and the
    // intent carries its key in the payload AND the envelope.
    let key0 = rendered("saga-1", SOURCE, "o-1", position.get(), 0);
    let key1 = rendered("saga-1", SOURCE, "o-1", position.get(), 1);
    let key2 = rendered("saga-1", SOURCE, "o-1", position.get(), 2);
    assert_eq!(
        rig.ledger_records("o-1").await,
        vec![(Cmd::Reserved { item: 7 }, intent_envelope(&key0))]
    );
    assert_eq!(
        rig.ledger_records("o-2").await,
        vec![(Cmd::Charged { item: 7 }, intent_envelope(&key1))]
    );
    assert_eq!(
        rig.outbox_records("saga-1").await,
        vec![(
            Record::Intent {
                intent: RenderedIntentKey::from_rendered(key2.clone()),
                request: Fx::Export { item: 7 },
            },
            intent_envelope(&key2)
        )]
    );

    // One group per stream per batch, in reaction order.
    assert_eq!(
        &*fold.log.command_groups.lock().unwrap(),
        &[
            ("ledger/o-1".to_owned(), ExpectedVersion::NoStream, 1),
            ("ledger/o-2".to_owned(), ExpectedVersion::NoStream, 1),
        ]
    );
    assert_eq!(
        &*fold.log.intent_groups.lock().unwrap(),
        &[("saga-outbox/saga-1".to_owned(), 1)]
    );

    // The cursor stands at the acked entry: the next step is idle.
    assert_eq!(
        runner.step(limit(10)).await.expect("second step"),
        RunnerStep::Idle
    );
}

/// Two commands to one stream and two effects to the outbox stream,
/// all from one event: each stream gets ONE merged write, payloads
/// concatenated in reaction order.
#[tokio::test]
async fn commands_to_one_stream_merge_into_one_write() {
    let rig = Rig::new();
    let position = rig.place(Src::Placed { item: 8 }).await;

    let saga = Scripted::new(
        "saga-2",
        vec![(
            Src::Placed { item: 8 },
            vec![
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-2".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Reserved { item: 8 },
                )),
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-2".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Charged { item: 8 },
                )),
                Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 8 })),
                Reaction::EffectRequest(EffectRequest::new(Fx::Notify { item: 8 })),
            ],
        )],
    );
    let fold = MemFold::default();
    let runner = rig.runner(saga, fold.clone());

    let step = runner.step(limit(10)).await.expect("the step succeeds");
    assert_eq!(step, RunnerStep::Advanced { acked_to: position });

    let key0 = rendered("saga-2", SOURCE, "o-1", position.get(), 0);
    let key1 = rendered("saga-2", SOURCE, "o-1", position.get(), 1);
    let key2 = rendered("saga-2", SOURCE, "o-1", position.get(), 2);
    let key3 = rendered("saga-2", SOURCE, "o-1", position.get(), 3);
    assert_eq!(
        rig.ledger_records("o-2").await,
        vec![
            (Cmd::Reserved { item: 8 }, intent_envelope(&key0)),
            (Cmd::Charged { item: 8 }, intent_envelope(&key1)),
        ]
    );
    assert_eq!(
        rig.outbox_records("saga-2").await,
        vec![
            (
                Record::Intent {
                    intent: RenderedIntentKey::from_rendered(key2.clone()),
                    request: Fx::Export { item: 8 },
                },
                intent_envelope(&key2)
            ),
            (
                Record::Intent {
                    intent: RenderedIntentKey::from_rendered(key3.clone()),
                    request: Fx::Notify { item: 8 },
                },
                intent_envelope(&key3)
            ),
        ]
    );

    assert_eq!(
        &*fold.log.command_groups.lock().unwrap(),
        &[("ledger/o-2".to_owned(), ExpectedVersion::NoStream, 2)]
    );
    assert_eq!(
        &*fold.log.intent_groups.lock().unwrap(),
        &[("saga-outbox/saga-2".to_owned(), 2)]
    );
}

/// Commands one event addressed at one stream must agree on their
/// expectation: the runner rejects the group before any write, and the
/// entry is not acked.
#[tokio::test]
async fn a_split_expectation_is_rejected_before_the_fold() {
    let rig = Rig::new();
    let position = rig.place(Src::Placed { item: 9 }).await;

    let one = crate::streams::StreamSequence::new(1).expect("1 is a position");
    let saga = Scripted::new(
        "saga-3",
        vec![(
            Src::Placed { item: 9 },
            vec![
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-3".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Reserved { item: 9 },
                )),
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-3".to_owned()),
                    ExpectedVersion::Exact(one),
                    Cmd::Charged { item: 9 },
                )),
            ],
        )],
    );
    let fold = MemFold::default();
    let runner = rig.runner(saga, fold.clone());

    let error = runner
        .step(limit(10))
        .await
        .expect_err("the group conflicts");
    assert!(matches!(error, Error::ConflictingExpectations(_)));

    assert!(rig.ledger_records("o-3").await.is_empty());
    assert!(rig.outbox_records("saga-3").await.is_empty());
    assert!(
        fold.log.command_groups.lock().unwrap().is_empty(),
        "the fold never runs for a rejected group"
    );
    assert_eq!(
        rig.outstanding_for_group("saga-3").await,
        vec![position.get()],
        "the entry is not acked and reappears on the next poll"
    );
}

/// An event with no reactions acks without a batch.
#[tokio::test]
async fn an_empty_reaction_set_acks_without_a_batch() {
    let rig = Rig::new();
    let position = rig.place(Src::Cancelled { item: 10 }).await;

    let saga = Scripted::new("saga-4", vec![(Src::Placed { item: 10 }, Vec::new())]);
    let fold = MemFold::default();
    let runner = rig.runner(saga, fold.clone());

    let step = runner.step(limit(10)).await.expect("the step succeeds");
    assert_eq!(step, RunnerStep::Advanced { acked_to: position });
    assert!(rig.outbox_records("saga-4").await.is_empty());
    assert!(
        fold.log.command_groups.lock().unwrap().is_empty()
            && fold.log.intent_groups.lock().unwrap().is_empty()
    );
    assert_eq!(
        runner.step(limit(10)).await.expect("second step"),
        RunnerStep::Idle
    );
}

/// A poll that delivers nothing is idle, and the cursor does not move.
#[tokio::test]
async fn a_poll_with_no_entries_is_idle() {
    let rig = Rig::new();
    let saga = Scripted::new("saga-5", Vec::new());
    let runner = rig.runner(saga, MemFold::default());
    assert_eq!(
        runner.step(limit(10)).await.expect("the step succeeds"),
        RunnerStep::Idle
    );
}

/// The crash window between append and ack, replayed: the reactions
/// already committed (the batch landed, the ack did not), so the
/// replay's `NoStream` command arm conflicts and the classification
/// rule must find the minted intent key on the outbox stream and ack
/// the redelivery as a no-op - no second append anywhere.
#[tokio::test]
async fn a_replayed_no_stream_command_acks_as_a_noop_through_the_conflict_path() {
    let rig = Rig::new();
    let position = rig.place(Src::Placed { item: 11 }).await;

    // The crashed attempt's committed state, by hand: the command with
    // its envelope key and the intent with payload and envelope keys.
    let key0 = rendered("saga-6", SOURCE, "o-1", position.get(), 0);
    let key1 = rendered("saga-6", SOURCE, "o-1", position.get(), 1);
    let mut builder = rig.db.batch();
    builder
        .write(
            LEDGER,
            &"o-4".to_owned(),
            ExpectedVersion::NoStream,
            &EventBatch::from_records(vec![RecordedEvent::keyed(
                Cmd::Reserved { item: 11 },
                intent_envelope(&key0),
            )])
            .expect("one record is nonempty"),
        )
        .expect("one write per stream");
    builder
        .write(
            OUTBOX_CATEGORY,
            &"saga-6".to_owned(),
            ExpectedVersion::Any,
            &EventBatch::from_records(vec![RecordedEvent::keyed(
                Record::Intent {
                    intent: RenderedIntentKey::from_rendered(key1.clone()),
                    request: Fx::Export { item: 11 },
                },
                intent_envelope(&key1),
            )])
            .expect("one record is nonempty"),
        )
        .expect("one write per stream");
    rig.db
        .transact(builder.build().expect("the batch has writes"))
        .await
        .expect("the crashed attempt's batch commits");

    let saga = Scripted::new(
        "saga-6",
        vec![(
            Src::Placed { item: 11 },
            vec![
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-4".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Reserved { item: 11 },
                )),
                Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 11 })),
            ],
        )],
    );
    let runner = rig.runner(saga, MemFold::default());

    // The replay: react re-runs, the batch aborts on the NoStream
    // command arm, and the re-read finds the intent key present.
    let step = runner
        .step(limit(10))
        .await
        .expect("the redelivery acks as a no-op");
    assert_eq!(step, RunnerStep::Advanced { acked_to: position });

    // Nothing doubled: each stream still holds exactly its one record.
    assert_eq!(
        rig.ledger_records("o-4").await,
        vec![(Cmd::Reserved { item: 11 }, intent_envelope(&key0))]
    );
    assert_eq!(
        rig.outbox_records("saga-6").await,
        vec![(
            Record::Intent {
                intent: RenderedIntentKey::from_rendered(key1.clone()),
                request: Fx::Export { item: 11 },
            },
            intent_envelope(&key1)
        )]
    );
    assert_eq!(
        runner.step(limit(10)).await.expect("second step"),
        RunnerStep::Idle
    );
}

/// A command arm that really conflicts - the stream moved past the
/// arm's expectation and the minted keys are nowhere on the outbox
/// stream - surfaces as an error, stores nothing, and does not ack.
#[tokio::test]
async fn a_real_conflict_surfaces_and_does_not_ack() {
    let rig = Rig::new();
    rig.seed_ledger("o-5", Cmd::Reserved { item: 12 }).await;
    let position = rig.place(Src::Placed { item: 12 }).await;

    let saga = Scripted::new(
        "saga-7",
        vec![(
            Src::Placed { item: 12 },
            vec![
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-5".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Charged { item: 12 },
                )),
                Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 12 })),
            ],
        )],
    );
    let runner = rig.runner(saga, MemFold::default());

    let error = runner
        .step(limit(10))
        .await
        .expect_err("the conflict surfaces");
    assert!(matches!(error, Error::Conflict(_)));

    assert_eq!(
        rig.ledger_records("o-5").await,
        vec![(Cmd::Reserved { item: 12 }, EventMetadata::new())],
        "the pre-seeded record stands and nothing partial landed"
    );
    assert!(
        rig.outbox_records("saga-7").await.is_empty(),
        "the batch rolled back whole: no intent records"
    );
    assert_eq!(
        rig.outstanding_for_group("saga-7").await,
        vec![position.get()],
        "the failing entry is not acked"
    );
}

/// Each entry acks after its own batch commits: an error on a later
/// entry leaves the earlier entries acked, so only the failing entry
/// redelivers.
#[tokio::test]
async fn a_conflict_mid_step_leaves_earlier_entries_acked() {
    let rig = Rig::new();
    rig.seed_ledger("o-7", Cmd::Reserved { item: 13 }).await;
    let first = rig.place(Src::Placed { item: 13 }).await;
    let second = rig.place(Src::Placed { item: 14 }).await;

    let saga = Scripted::new(
        "saga-8",
        vec![
            (
                Src::Placed { item: 13 },
                vec![Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-6".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Reserved { item: 13 },
                ))],
            ),
            (
                Src::Placed { item: 14 },
                vec![Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-7".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Charged { item: 14 },
                ))],
            ),
        ],
    );
    let runner = rig.runner(saga, MemFold::default());

    let error = runner
        .step(limit(10))
        .await
        .expect_err("the second entry conflicts");
    assert!(matches!(error, Error::Conflict(_)));

    let key0 = rendered("saga-8", SOURCE, "o-1", first.get(), 0);
    assert_eq!(
        rig.ledger_records("o-6").await,
        vec![(Cmd::Reserved { item: 13 }, intent_envelope(&key0))],
        "the first entry's batch committed and stayed"
    );
    assert_eq!(
        rig.outstanding_for_group("saga-8").await,
        vec![second.get()],
        "only the failing entry redelivers"
    );
}

/// The rendered key's escaping is injective: the classic collision
/// (saga "a/b" on stream "c" vs saga "a" on stream "b/c") renders to
/// two different strings, each the canonical escaped form. The
/// rendering rule itself is pinned by every envelope assertion above;
/// this fixture pins it under adversarial components.
#[tokio::test]
async fn key_rendering_escapes_components_injectively() {
    // Saga "a/b" reacting on stream "c": key a\/b/orders/c/1/0.
    let rig_a = Rig::new();
    let position_a = rig_a.place_on("c", Src::Placed { item: 1 }).await;
    let saga_a = Scripted::new(
        "a/b",
        vec![(
            Src::Placed { item: 1 },
            vec![Reaction::Command(Command::new(
                StreamRef::new(LEDGER, &"c".to_owned()),
                ExpectedVersion::Any,
                Cmd::Reserved { item: 1 },
            ))],
        )],
    );
    rig_a
        .runner(saga_a, MemFold::default())
        .step(limit(10))
        .await
        .expect("the step succeeds");

    // Saga "a" reacting on stream "b/c": key a/orders/b\/c/1/0.
    let rig_b = Rig::new();
    let position_b = rig_b.place_on("b/c", Src::Placed { item: 1 }).await;
    let saga_b = Scripted::new(
        "a",
        vec![(
            Src::Placed { item: 1 },
            vec![Reaction::Command(Command::new(
                StreamRef::new(LEDGER, &"b/c".to_owned()),
                ExpectedVersion::Any,
                Cmd::Reserved { item: 1 },
            ))],
        )],
    );
    rig_b
        .runner(saga_b, MemFold::default())
        .step(limit(10))
        .await
        .expect("the step succeeds");

    let records_a = rig_a.ledger_records("c").await;
    let records_b = rig_b.ledger_records("b/c").await;
    let rendered_a = records_a[0]
        .1
        .get(INTENT_METADATA_KEY)
        .expect("the command record carries its key")
        .to_owned();
    let rendered_b = records_b[0]
        .1
        .get(INTENT_METADATA_KEY)
        .expect("the command record carries its key")
        .to_owned();

    assert_eq!(
        position_a, position_b,
        "the fixtures hold the same position"
    );
    assert_eq!(
        rendered_a,
        rendered("a/b", SOURCE, "c", 1, 0),
        "the slash in the saga id is escaped"
    );
    assert_eq!(
        rendered_b,
        rendered("a", SOURCE, "b/c", 1, 0),
        "the slash in the stream key is escaped"
    );
    assert_ne!(rendered_a, rendered_b, "escaping makes rendering injective");
}

/// The poll loop drains the backlog: every delivered event folds, its
/// intents land, and the loop keeps polling until stopped.
#[tokio::test]
async fn runner_run_drains_the_backlog() {
    let rig = Rig::new();
    let script: Vec<(Src, Vec<Reaction<Cmd, Fx>>)> = (20..23)
        .map(|item| {
            (
                Src::Placed { item },
                vec![
                    Reaction::Command(Command::new(
                        StreamRef::new(LEDGER, &"o-r".to_owned()),
                        ExpectedVersion::Any,
                        Cmd::Reserved { item },
                    )),
                    Reaction::EffectRequest(EffectRequest::new(Fx::Export { item })),
                ],
            )
        })
        .collect();
    let saga = Scripted::new("saga-run", script);
    for item in 20..23 {
        rig.place(Src::Placed { item }).await;
    }

    let runner = rig.runner(saga, MemFold::default());
    let task = tokio::spawn(async move {
        runner
            .run(limit(5), Duration::from_millis(10))
            .await
            .expect("the loop only stops by abort")
    });

    let outbox = rig.outbox.clone();
    let ledger = rig.ledger.clone();
    wait_until(|| {
        let outbox = outbox.clone();
        let ledger = ledger.clone();
        async move {
            stream_len(&outbox, "saga-run").await >= 3 && stream_len(&ledger, "o-r").await >= 3
        }
    })
    .await;

    let outbox_records = rig.outbox_records("saga-run").await;
    assert_eq!(outbox_records.len(), 3);
    assert!(outbox_records
        .iter()
        .all(|(event, _)| matches!(event, Record::Intent { .. })));
    assert_eq!(rig.ledger_records("o-r").await.len(), 3);
    task.abort();
}
