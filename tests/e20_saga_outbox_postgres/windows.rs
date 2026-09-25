//! The five crash-window scenarios.

use epoch::streams::feed::{ConsumerGroup, EventFeed};
use epoch::streams::outbox::{ExecutorStep, Record, OUTBOX_CATEGORY};
use epoch::streams::saga::{Command, EffectRequest, Error, Reaction, RunnerStep};
use epoch::streams::spec::under_deadline;
use epoch::streams::{AtomicStreams, EventBatch, ExpectedVersion, RecordedEvent, StreamRef};

use super::support::{
    intent_envelope, is_command_record, is_done_record, is_failed_record, is_intent_record,
    is_parked_record, key_of, limit, rendered, unique, Cmd, Fx, Hook, Port, PortErr, Rig, Scripted,
    SimulatedCrash, Src, LEDGER, POLL_LIMIT, STEP_BOUND, STEP_DEADLINE,
};

/// THE typed-outcome proof. The source event's command arm expects
/// `Any` - it can never conflict, so only the uniqueness index can
/// abort the replayed batch. The crashed attempt's batch (command +
/// intent, no ack) is committed by hand; a fresh runner replays, the
/// intent insert hits `stream_events_outbox_intent`, the typed
/// `TransactError::DuplicateIntent` surfaces, the classification
/// re-read finds the minted key present, and the entry acks as a
/// no-op. Without the unique-violation mapping this test fails with a
/// propagated `Backend` error after retries - that is the point.
#[tokio::test(flavor = "multi_thread")]
async fn crash_between_append_and_ack_rejects_the_duplicate_intent() {
    under_deadline(async {
        let rig = Rig::new().await;
        let saga_id = unique("e20-dup-saga");
        let ledger_stream = unique("e20-ledger");
        let position = rig.place_on("o-1", Src::Placed { item: 7 }).await;

        // The crashed attempt's minted keys, recomputed by hand from
        // the rendering rule.
        let key0 = rendered(&saga_id, &rig.source_category, "o-1", position.get(), 0);
        let key1 = rendered(&saga_id, &rig.source_category, "o-1", position.get(), 1);

        // The batch that committed before the crash: the command and
        // the intent, each with its envelope key. The ack never
        // landed.
        let mut builder = rig.db.batch();
        builder
            .write(
                LEDGER,
                &ledger_stream,
                ExpectedVersion::Any,
                &EventBatch::from_records(vec![RecordedEvent::keyed(
                    Cmd::Reserved { item: 7 },
                    intent_envelope(&key0),
                )])
                .expect("one record is nonempty"),
            )
            .expect("the command write encodes");
        builder
            .write(
                OUTBOX_CATEGORY,
                &saga_id,
                ExpectedVersion::Any,
                &EventBatch::from_records(vec![RecordedEvent::keyed(
                    Record::Intent {
                        intent: key_of(key1.clone()),
                        request: Fx::Export { item: 7 },
                    },
                    intent_envelope(&key1),
                )])
                .expect("one record is nonempty"),
            )
            .expect("the intent write encodes");
        rig.db
            .transact(builder.build().expect("the batch has writes"))
            .await
            .expect("the crashed attempt's batch commits");

        // A fresh runner: its group cursor has never acked - the
        // crash state.
        let saga = Scripted::new(
            &saga_id,
            vec![(
                Src::Placed { item: 7 },
                vec![
                    Reaction::Command(Command::new(
                        StreamRef::new(LEDGER, &ledger_stream),
                        ExpectedVersion::Any,
                        Cmd::Reserved { item: 7 },
                    )),
                    Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 7 })),
                ],
            )],
        );
        let runner = rig.runner(saga);

        // The replay: react re-runs, the batch re-appends, the intent
        // insert hits the uniqueness index, and the classification
        // re-read finds the key present.
        let step = runner
            .step(limit(10))
            .await
            .expect("the redelivery acks as a no-op");
        assert_eq!(step, RunnerStep::Advanced { acked_to: position });

        // Nothing doubled: each stream holds exactly its one record.
        let ledger = rig.ledger_records(&ledger_stream).await;
        assert_eq!(
            ledger.len(),
            1,
            "the rolled-back re-append left no second row"
        );
        assert!(is_command_record(
            &ledger[0],
            &Cmd::Reserved { item: 7 },
            &key0
        ));
        let outbox = rig.outbox_records(&saga_id).await;
        assert_eq!(outbox.len(), 1, "exactly the pre-committed intent stands");
        assert!(is_intent_record(&outbox[0], &key1, &Fx::Export { item: 7 }));

        assert_eq!(
            runner.step(limit(10)).await.expect("second step"),
            RunnerStep::Idle
        );
    })
    .await;
}

/// The expectation-sensitive replay: the same crash replay, but the
/// command arm carries `ExpectedVersion::NoStream`. The manual
/// pre-commit creates the command stream, so the replay's batch aborts
/// as a Conflict - the expectation check runs before the inserts ever
/// reach the index - and the classification finds the intent key
/// present and acks the no-op.
#[tokio::test(flavor = "multi_thread")]
async fn replayed_no_stream_command_acks_through_the_conflict_path() {
    under_deadline(async {
        let rig = Rig::new().await;
        let saga_id = unique("e20-nostr-saga");
        let ledger_stream = unique("e20-ledger");
        let position = rig.place_on("o-1", Src::Placed { item: 11 }).await;

        let key0 = rendered(&saga_id, &rig.source_category, "o-1", position.get(), 0);
        let key1 = rendered(&saga_id, &rig.source_category, "o-1", position.get(), 1);

        // The crashed attempt's committed state, by hand: the NoStream
        // command creates its stream, the intent lands, the ack does
        // not.
        let mut builder = rig.db.batch();
        builder
            .write(
                LEDGER,
                &ledger_stream,
                ExpectedVersion::NoStream,
                &EventBatch::from_records(vec![RecordedEvent::keyed(
                    Cmd::Reserved { item: 11 },
                    intent_envelope(&key0),
                )])
                .expect("one record is nonempty"),
            )
            .expect("the command write encodes");
        builder
            .write(
                OUTBOX_CATEGORY,
                &saga_id,
                ExpectedVersion::Any,
                &EventBatch::from_records(vec![RecordedEvent::keyed(
                    Record::Intent {
                        intent: key_of(key1.clone()),
                        request: Fx::Export { item: 11 },
                    },
                    intent_envelope(&key1),
                )])
                .expect("one record is nonempty"),
            )
            .expect("the intent write encodes");
        rig.db
            .transact(builder.build().expect("the batch has writes"))
            .await
            .expect("the crashed attempt's batch commits");

        let saga = Scripted::new(
            &saga_id,
            vec![(
                Src::Placed { item: 11 },
                vec![
                    Reaction::Command(Command::new(
                        StreamRef::new(LEDGER, &ledger_stream),
                        ExpectedVersion::NoStream,
                        Cmd::Reserved { item: 11 },
                    )),
                    Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 11 })),
                ],
            )],
        );
        let runner = rig.runner(saga);

        // The replay: the expectation check aborts the batch before
        // any insert, and the re-read finds the intent key present.
        let step = runner
            .step(limit(10))
            .await
            .expect("the redelivery acks as a no-op");
        assert_eq!(step, RunnerStep::Advanced { acked_to: position });

        // Nothing doubled: each stream still holds exactly its one
        // record.
        let ledger = rig.ledger_records(&ledger_stream).await;
        assert_eq!(ledger.len(), 1, "nothing doubled");
        assert!(is_command_record(
            &ledger[0],
            &Cmd::Reserved { item: 11 },
            &key0
        ));
        let outbox = rig.outbox_records(&saga_id).await;
        assert_eq!(outbox.len(), 1, "nothing doubled");
        assert!(is_intent_record(
            &outbox[0],
            &key1,
            &Fx::Export { item: 11 }
        ));

        assert_eq!(
            runner.step(limit(10)).await.expect("second step"),
            RunnerStep::Idle
        );
    })
    .await;
}

/// A command arm that really conflicts - the ledger stream moved past
/// the arm's `NoStream` expectation before the source event landed,
/// and the minted keys are nowhere on the outbox stream (no manual
/// pre-commit, no intents anywhere). The conflict surfaces as an
/// error, stores nothing, and the failing entry is not acked.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_conflict_on_postgres_surfaces() {
    under_deadline(async {
        let rig = Rig::new().await;
        let saga_id = unique("e20-conflict-saga");
        let ledger_stream = unique("e20-ledger");

        // The ledger stream already moved: the saga's NoStream arm can
        // never land.
        rig.seed_ledger(&ledger_stream, Cmd::Reserved { item: 12 })
            .await;
        let position = rig.place_on("o-1", Src::Placed { item: 12 }).await;

        let saga = Scripted::new(
            &saga_id,
            vec![(
                Src::Placed { item: 12 },
                vec![
                    Reaction::Command(Command::new(
                        StreamRef::new(LEDGER, &ledger_stream),
                        ExpectedVersion::NoStream,
                        Cmd::Charged { item: 12 },
                    )),
                    Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 12 })),
                ],
            )],
        );
        let error = rig
            .runner(saga)
            .step(limit(10))
            .await
            .expect_err("the conflict surfaces");
        assert!(matches!(error, Error::Conflict(_)));

        assert!(
            rig.outbox_records(&saga_id).await.is_empty(),
            "the batch rolled back whole: no intent records"
        );

        // The failing entry is not acked: a fresh poll of the runner's
        // group still delivers its position.
        let redelivered = rig
            .source_feed
            .poll(
                &ConsumerGroup::new(saga_id.clone()).expect("a saga id is a nonempty group"),
                limit(10),
            )
            .await
            .expect("probe poll succeeds");
        assert_eq!(
            redelivered
                .iter()
                .map(|entry| entry.position())
                .collect::<Vec<_>>(),
            vec![position],
            "the failing entry is not acked"
        );
    })
    .await;
}

/// The executor's crash window: the port performs the external effect
/// and the process dies before the `Done` append. The poisoned port
/// stages the crash - apply, then `panic_any` - inside a spawned
/// step, so the executor "crashes" with the effect already applied.
/// A fresh executor on a healthy port (sharing the external system)
/// redelivers the intent: the port is called a SECOND time, the
/// port-side dedupe absorbs the replay, `Done` lands, and the cursor
/// advances. At-least-once delivery, idempotency at the port.
#[tokio::test(flavor = "multi_thread")]
async fn executor_crash_between_perform_and_done_replays_to_a_dedupe() {
    under_deadline(async {
        let rig = Rig::new().await;
        let saga_id = unique("e20-exec-saga");
        let position = rig.place_on("o-1", Src::Placed { item: 30 }).await;

        // One intent, committed by a real runner step.
        let saga = Scripted::new(
            &saga_id,
            vec![(
                Src::Placed { item: 30 },
                vec![Reaction::EffectRequest(EffectRequest::new(Fx::Export {
                    item: 30,
                }))],
            )],
        );
        let step = rig
            .runner(saga)
            .step(limit(10))
            .await
            .expect("the intent commits");
        assert_eq!(step, RunnerStep::Advanced { acked_to: position });

        let key = rendered(&saga_id, &rig.source_category, "o-1", position.get(), 0);

        // The poisoned port: its FIRST perform for the key applies the
        // external effect and then panics - the crash between the
        // external call and the outcome append. The step runs inside
        // tokio::spawn, so the crash is a JoinError, the
        // process-death shape.
        let port = Port::default();
        port.crash_on_apply.lock().unwrap().push(key.clone());
        let executor = rig.executor(&saga_id, port.clone(), Hook::default(), 5);

        let crashed = tokio::spawn(async move {
            for _ in 0..STEP_BOUND {
                executor
                    .step(limit(POLL_LIMIT))
                    .await
                    .expect("the executor steps succeed until the simulated crash");
            }
            panic!("the poisoned port never reached the intent within the step bound");
        });
        let outcome = tokio::time::timeout(STEP_DEADLINE, crashed)
            .await
            .expect("the crash lands inside the deadline");
        match outcome {
            Err(join_error) => {
                assert!(
                    join_error.is_panic(),
                    "the executor task failed without panicking: {join_error}"
                );
                assert!(
                    join_error
                        .into_panic()
                        .downcast_ref::<SimulatedCrash>()
                        .is_some(),
                    "the crash was not the port's simulated perform crash"
                );
            }
            Ok(_) => panic!("the executor survived the poisoned port"),
        }
        assert_eq!(
            &*port.calls.lock().unwrap(),
            &[(key.clone(), Fx::Export { item: 30 })],
            "the crash recorded the first perform"
        );
        assert_eq!(
            &*port.applied.lock().unwrap(),
            &[key.clone()],
            "the external effect was applied before the crash"
        );

        // A fresh executor on a healthy port over the same external
        // system: the intent redelivers, the port is called a second
        // time, the dedupe absorbs it, Done appends, the cursor
        // advances.
        let fresh = rig.executor(&saga_id, port.healthy_sibling(), Hook::default(), 5);
        let mut steps = 0;
        let records = loop {
            let outcome = tokio::time::timeout(STEP_DEADLINE, fresh.step(limit(POLL_LIMIT)))
                .await
                .expect("the fresh step lands inside the deadline")
                .expect("the fresh executor steps succeed");
            assert!(
                matches!(outcome, ExecutorStep::Advanced { .. } | ExecutorStep::Idle),
                "unexpected executor outcome: {outcome:?}"
            );
            steps += 1;
            assert!(
                steps <= STEP_BOUND,
                "the fresh executor never reached the intent"
            );
            let records = rig.outbox_records(&saga_id).await;
            if records.len() == 2 && is_intent_record(&records[0], &key, &Fx::Export { item: 30 }) {
                break records;
            }
        };
        assert!(
            is_done_record(&records[1], &key),
            "exactly one Done record lands after the intent: {records:?}"
        );
        assert_eq!(
            &*port.applied.lock().unwrap(),
            &[key.clone()],
            "the external effect was applied exactly once"
        );
        assert_eq!(
            port.calls.lock().unwrap().len(),
            2,
            "at-least-once: the port was called twice, the dedupe absorbed the replay"
        );
    })
    .await;
}

/// Budget exhaustion on postgres: with a budget of 2 and a port that
/// always fails, the first perform appends `Failed` and HOLDS the
/// cursor, the second parks the intent - `Parked` with the durable
/// count and the port error's Display text - and fires the hook
/// exactly once. After the park, one more step skips the outcome
/// records and the final state is stable.
#[tokio::test(flavor = "multi_thread")]
async fn repeated_failure_parks_and_fires_the_hook_on_postgres() {
    under_deadline(async {
        let rig = Rig::new().await;
        let saga_id = unique("e20-park-saga");
        let position = rig.place_on("o-1", Src::Placed { item: 60 }).await;

        // One intent, committed by a real runner step.
        let saga = Scripted::new(
            &saga_id,
            vec![(
                Src::Placed { item: 60 },
                vec![Reaction::EffectRequest(EffectRequest::new(Fx::Export {
                    item: 60,
                }))],
            )],
        );
        let step = rig
            .runner(saga)
            .step(limit(10))
            .await
            .expect("the intent commits");
        assert_eq!(step, RunnerStep::Advanced { acked_to: position });

        let key = rendered(&saga_id, &rig.source_category, "o-1", position.get(), 0);
        let error_text = PortErr::flaky().text();

        let port = Port::default();
        port.fail_for.lock().unwrap().insert(key.clone(), u32::MAX);
        let hook = Hook::default();
        let executor = rig.executor(&saga_id, port.clone(), hook.clone(), 2);

        // Each step appends Failed or parks; bounded loop until the
        // park lands (the first steps may chew foreign backlog).
        let mut steps = 0;
        let records = loop {
            let outcome = tokio::time::timeout(STEP_DEADLINE, executor.step(limit(POLL_LIMIT)))
                .await
                .expect("the step lands inside the deadline")
                .expect("the executor steps succeed until the park");
            steps += 1;
            assert!(
                steps <= STEP_BOUND,
                "the park never landed within the step bound"
            );
            let records = rig.outbox_records(&saga_id).await;
            if records
                .iter()
                .any(|record| is_parked_record(record, &key, 2, &error_text))
            {
                assert!(
                    matches!(outcome, ExecutorStep::Advanced { .. }),
                    "the park acks past the intent permanently, got {outcome:?}"
                );
                break records;
            }
            match &outcome {
                ExecutorStep::Advanced { .. } => {}
                ExecutorStep::Holding { intent } => {
                    assert_eq!(intent.as_str(), key, "the hold names the failing intent");
                }
                ExecutorStep::Idle => panic!("the executor went idle before the park"),
            }
        };

        // The whole stream, in order: Intent, Failed, Failed, Parked.
        assert_eq!(records.len(), 4, "intent, two failed, parked: {records:?}");
        assert!(is_intent_record(
            &records[0],
            &key,
            &Fx::Export { item: 60 }
        ));
        assert!(is_failed_record(&records[1], &key, &error_text));
        assert!(is_failed_record(&records[2], &key, &error_text));
        assert!(is_parked_record(&records[3], &key, 2, &error_text));
        assert_eq!(
            &*hook.fires.lock().unwrap(),
            &[(key.clone(), 2, error_text.clone())],
            "the hook fires once, at park time, with the durable count"
        );
        assert_eq!(
            port.calls.lock().unwrap().len(),
            2,
            "the budget allowed exactly two performs"
        );

        // After the park: one more step skips the outcome records
        // (ack past) and the final state is stable.
        let outcome = tokio::time::timeout(STEP_DEADLINE, executor.step(limit(POLL_LIMIT)))
            .await
            .expect("the post-park step lands inside the deadline")
            .expect("the post-park step succeeds");
        assert!(
            matches!(outcome, ExecutorStep::Advanced { .. } | ExecutorStep::Idle),
            "unexpected post-park outcome: {outcome:?}"
        );
        assert_eq!(
            rig.outbox_records(&saga_id).await.len(),
            4,
            "no more appends after the park"
        );
        assert_eq!(hook.fires.lock().unwrap().len(), 1);
        assert_eq!(port.calls.lock().unwrap().len(), 2);
    })
    .await;
}
