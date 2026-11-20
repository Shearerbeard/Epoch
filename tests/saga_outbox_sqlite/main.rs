#![cfg(feature = "sqlite")]

mod support;

use std::num::NonZeroU32;

use epoch::streams::feed::{ConsumerGroup, EventFeed, FeedPosition, PollLimit};
use epoch::streams::outbox::{
    ExecutorError, ExecutorStep, Record, INTENT_METADATA_KEY, OUTBOX_CATEGORY,
};
use epoch::streams::saga::{Error, RunnerStep};
use epoch::streams::spec::under_deadline;
use epoch::streams::sqlite::SqliteBatch;
use epoch::streams::{
    AtomicStreams, EventBatch, EventMetadata, EventStreams, ExpectedVersion, RecordedEvent,
    RenderedIntentKey, TransactError,
};

use support::{
    CommandEvent, Effect, Hook, HookBehavior, Port, PortBehavior, PortError, Rig, SimulatedCrash,
    Source, COMMAND_KEY, LEDGER, SAGA, SOURCE, SOURCE_KEY,
};

fn limit() -> PollLimit {
    PollLimit::new(10).expect("the fixture has fewer than ten records per category")
}

fn group() -> ConsumerGroup {
    ConsumerGroup::new(SAGA).expect("the fixture saga has a nonempty name")
}

fn key(position: FeedPosition, index: u64) -> RenderedIntentKey {
    serde_json::from_value(serde_json::Value::String(format!(
        "{SAGA}/{SOURCE}/{SOURCE_KEY}/{}/{index}",
        position.get()
    )))
    .expect("the public serde form accepts a rendered key")
}

fn envelope(key: &RenderedIntentKey) -> EventMetadata {
    let mut metadata = EventMetadata::new();
    metadata.insert(INTENT_METADATA_KEY, key.as_str());
    metadata
}

fn intent(position: FeedPosition, item: u32) -> (Record<Effect>, EventMetadata) {
    let key = key(position, 1);
    (
        Record::Intent {
            intent: key.clone(),
            request: Effect::Export { item },
        },
        envelope(&key),
    )
}

fn failed(position: FeedPosition) -> (Record<Effect>, EventMetadata) {
    (
        Record::Failed {
            intent: key(position, 1),
            error: PortError.to_string(),
        },
        EventMetadata::new(),
    )
}

fn done(position: FeedPosition) -> (Record<Effect>, EventMetadata) {
    (
        Record::Done {
            intent: key(position, 1),
        },
        EventMetadata::new(),
    )
}

fn replay_batch(rig: &Rig, position: FeedPosition, expected: ExpectedVersion) -> SqliteBatch {
    let mut builder = rig.db.batch();
    builder
        .write(
            LEDGER,
            &COMMAND_KEY.to_owned(),
            expected,
            &EventBatch::from_records(vec![RecordedEvent::keyed(
                CommandEvent::ReserveStock { item: 7 },
                envelope(&key(position, 0)),
            )])
            .expect("one command is nonempty"),
        )
        .expect("the command encodes");
    let (payload, metadata) = intent(position, 7);
    builder
        .write(
            OUTBOX_CATEGORY,
            &SAGA.to_owned(),
            ExpectedVersion::Any,
            &EventBatch::from_records(vec![RecordedEvent::keyed(payload, metadata)])
                .expect("one intent is nonempty"),
        )
        .expect("the intent encodes");
    builder.build().expect("the batch has two writes")
}

async fn assert_committed(rig: &Rig, position: FeedPosition) {
    assert_eq!(
        rig.records::<CommandEvent>(LEDGER, COMMAND_KEY).await,
        vec![(
            CommandEvent::ReserveStock { item: 7 },
            envelope(&key(position, 0))
        )]
    );
    assert_eq!(
        rig.records::<Record<Effect>>(OUTBOX_CATEGORY, SAGA).await,
        vec![intent(position, 7)]
    );
}

/// Saving the command without its intent would lose the requested effect.
/// A successful runner step must persist both records in one batch
/// and acknowledge the source only after that batch commits.
#[tokio::test]
async fn runner_commits_command_and_intent_atomically() {
    under_deadline(async {
        let dir = tempfile::tempdir().expect("temporary database directory");
        let path = dir.path().join("saga.db");
        let rig = Rig::open(&path).await;
        let position = rig.place(7).await;
        assert_eq!(
            rig.runner(ExpectedVersion::Any)
                .step(limit())
                .await
                .unwrap(),
            RunnerStep::Advanced { acked_to: position }
        );
        assert_committed(&rig, position).await;
        rig.close().await;

        let rig = Rig::open(&path).await;
        assert_committed(&rig, position).await;
        assert_eq!(
            rig.runner(ExpectedVersion::Any)
                .step(limit())
                .await
                .unwrap(),
            RunnerStep::Idle
        );
        rig.close().await;
    })
    .await;
}

/// Simulate a crash after the batch commits but before source acknowledgement.
/// With `Any`, version checks allow replay, but intent uniqueness must reject
/// the entire repeated batch as `DuplicateIntent`. The runner must recognize
/// the stored intent and acknowledge without leaving another command or intent.
#[tokio::test]
async fn any_replay_is_typed_duplicate_and_acks_once() {
    under_deadline(async {
        let dir = tempfile::tempdir().expect("temporary database directory");
        let path = dir.path().join("saga.db");
        let rig = Rig::open(&path).await;
        let position = rig.place(7).await;
        rig.db
            .transact(replay_batch(&rig, position, ExpectedVersion::Any))
            .await
            .unwrap();
        rig.close().await;

        let rig = Rig::open(&path).await;
        assert!(matches!(
            rig.db
                .transact(replay_batch(&rig, position, ExpectedVersion::Any))
                .await,
            Err(TransactError::DuplicateIntent(_))
        ));
        assert_committed(&rig, position).await;
        assert_eq!(
            rig.runner(ExpectedVersion::Any)
                .step(limit())
                .await
                .unwrap(),
            RunnerStep::Advanced { acked_to: position }
        );
        assert_committed(&rig, position).await;
        rig.close().await;

        let rig = Rig::open(&path).await;
        assert_eq!(
            rig.runner(ExpectedVersion::Any)
                .step(limit())
                .await
                .unwrap(),
            RunnerStep::Idle
        );
        assert_committed(&rig, position).await;
        rig.close().await;
    })
    .await;
}

/// Replay the same commit-before-ack crash with a `NoStream` command write.
/// The first commit created the stream, so replay hits `Conflict` before the
/// uniqueness check. The stored intent must still prove prior success: the
/// runner acknowledges, and neither the command nor the intent is duplicated.
#[tokio::test]
async fn no_stream_replay_is_conflict_and_acks_once() {
    under_deadline(async {
        let dir = tempfile::tempdir().expect("temporary database directory");
        let path = dir.path().join("saga.db");
        let rig = Rig::open(&path).await;
        let position = rig.place(7).await;
        rig.db
            .transact(replay_batch(&rig, position, ExpectedVersion::NoStream))
            .await
            .unwrap();
        rig.close().await;

        let rig = Rig::open(&path).await;
        assert!(matches!(
            rig.db
                .transact(replay_batch(&rig, position, ExpectedVersion::NoStream))
                .await,
            Err(TransactError::Conflict(_))
        ));
        assert_committed(&rig, position).await;
        assert_eq!(
            rig.runner(ExpectedVersion::NoStream)
                .step(limit())
                .await
                .unwrap(),
            RunnerStep::Advanced { acked_to: position }
        );
        assert_committed(&rig, position).await;
        rig.close().await;

        let rig = Rig::open(&path).await;
        assert_eq!(
            rig.runner(ExpectedVersion::NoStream)
                .step(limit())
                .await
                .unwrap(),
            RunnerStep::Idle
        );
        assert_committed(&rig, position).await;
        rig.close().await;
    })
    .await;
}

/// Another write has already invalidated the command's expected version, and
/// no matching intent proves an earlier saga commit. Expect a conflict with
/// no partial batch writes. The source event must remain unacknowledged so
/// the failure is not silently skipped.
#[tokio::test]
async fn real_command_conflict_remains_unacked() {
    under_deadline(async {
        let dir = tempfile::tempdir().expect("temporary database directory");
        let path = dir.path().join("saga.db");
        let rig = Rig::open(&path).await;
        let seeded = CommandEvent::ReserveStock { item: 99 };
        rig.db
            .category::<String, CommandEvent>(LEDGER)
            .append(
                ExpectedVersion::NoStream,
                &COMMAND_KEY.to_owned(),
                &EventBatch::new(vec![seeded.clone()]).unwrap(),
            )
            .await
            .unwrap();
        let position = rig.place(7).await;
        assert!(matches!(
            rig.runner(ExpectedVersion::NoStream).step(limit()).await,
            Err(Error::Conflict(_))
        ));
        rig.close().await;

        let rig = Rig::open(&path).await;
        assert_eq!(
            rig.records::<CommandEvent>(LEDGER, COMMAND_KEY).await,
            vec![(seeded, EventMetadata::new())]
        );
        assert!(rig
            .records::<Record<Effect>>(OUTBOX_CATEGORY, SAGA)
            .await
            .is_empty());
        let entries = rig
            .db
            .feed::<Source>(SOURCE)
            .poll(&group(), limit())
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].position(), position);
        assert_eq!(entries[0].record().event(), &Source::Placed { item: 7 });
        rig.close().await;
    })
    .await;
}

/// The port applies the external effect, then crashes before `Done` is stored.
/// Reopening SQLite must redeliver the intent with the same key. Expect two
/// port calls but one applied effect because the port deduplicates by key;
/// the successful retry then stores `Done` and acknowledges the intent.
#[tokio::test]
async fn perform_crash_reopens_and_redelivers_once_effect() {
    under_deadline(async {
        let dir = tempfile::tempdir().expect("temporary database directory");
        let path = dir.path().join("saga.db");
        let rig = Rig::open(&path).await;
        let position = rig.place(7).await;
        rig.runner(ExpectedVersion::Any)
            .step(limit())
            .await
            .unwrap();
        let port = Port::new(PortBehavior::CrashAfterApply);
        let hook = Hook::new(HookBehavior::Healthy);
        let executor = rig.executor(port.clone(), hook.clone(), 2);
        let crash = tokio::spawn(async move { executor.step(limit()).await })
            .await
            .expect_err("the port must interrupt the executor");
        assert!(crash.is_panic(), "the executor task must panic");
        assert!(
            crash
                .into_panic()
                .downcast_ref::<SimulatedCrash>()
                .is_some(),
            "only the injected perform crash is acceptable"
        );
        assert_eq!(
            &*port.calls.lock().unwrap(),
            &[(key(position, 1), Effect::Export { item: 7 })]
        );
        assert_eq!(*port.applied.lock().unwrap(), [key(position, 1)].into());
        assert_committed(&rig, position).await;
        rig.close().await;

        let rig = Rig::open(&path).await;
        let feed = rig.db.feed::<Record<Effect>>(OUTBOX_CATEGORY);
        let entries = feed.poll(&group(), limit()).await.unwrap();
        assert_eq!(entries.len(), 1);
        let intent_position = entries[0].position();
        assert_eq!(entries[0].record().event(), &intent(position, 7).0);
        drop(feed);
        let executor = rig.executor(port.with_behavior(PortBehavior::Healthy), hook.clone(), 2);
        assert_eq!(
            executor.step(limit()).await.unwrap(),
            ExecutorStep::Advanced {
                acked_to: intent_position
            }
        );
        assert_eq!(
            rig.records::<Record<Effect>>(OUTBOX_CATEGORY, SAGA).await,
            vec![
                intent(position, 7),
                (
                    Record::Done {
                        intent: key(position, 1)
                    },
                    EventMetadata::new()
                ),
            ]
        );
        assert_eq!(
            &*port.calls.lock().unwrap(),
            &[
                (key(position, 1), Effect::Export { item: 7 }),
                (key(position, 1), Effect::Export { item: 7 }),
            ]
        );
        assert_eq!(*port.applied.lock().unwrap(), [key(position, 1)].into());
        assert!(hook.calls.lock().unwrap().is_empty());
        assert!(matches!(
            executor.step(limit()).await.unwrap(),
            ExecutorStep::Advanced { .. }
        ));
        assert_eq!(executor.step(limit()).await.unwrap(), ExecutorStep::Idle);
        drop(executor);
        rig.close().await;

        let rig = Rig::open(&path).await;
        assert_eq!(
            rig.executor(port.with_behavior(PortBehavior::Healthy), hook, 2)
                .step(limit())
                .await
                .unwrap(),
            ExecutorStep::Idle
        );
        assert_eq!(port.calls.lock().unwrap().len(), 2);
        rig.close().await;
    })
    .await;
}

/// The port succeeded and `Done` persisted, but the interruption landed
/// before the acknowledgement: the outbox holds the intent and its
/// `Done` while the group still owes the intent. The `Done` is staged
/// through the store because every crash seam the fixture can inject
/// lands before the outcome append, not between that append and the
/// ack. Replaying against a port that fails if called must acknowledge
/// from the durable success alone: no port call, no compensation, no
/// new outcome records.
#[tokio::test]
async fn success_before_ack_replays_without_port_or_hook() {
    under_deadline(async {
        let dir = tempfile::tempdir().expect("temporary database directory");
        let path = dir.path().join("saga.db");
        let rig = Rig::open(&path).await;
        let position = rig.place(7).await;
        rig.runner(ExpectedVersion::Any)
            .step(limit())
            .await
            .unwrap();
        rig.db
            .category::<String, Record<Effect>>(OUTBOX_CATEGORY)
            .append(
                ExpectedVersion::Any,
                &SAGA.to_owned(),
                &EventBatch::new(vec![done(position).0]).expect("one outcome is nonempty"),
            )
            .await
            .expect("the staged success commits");
        let expected = vec![intent(position, 7), done(position)];
        rig.close().await;

        let rig = Rig::open(&path).await;
        assert_eq!(
            rig.records::<Record<Effect>>(OUTBOX_CATEGORY, SAGA).await,
            expected
        );
        let feed = rig.db.feed::<Record<Effect>>(OUTBOX_CATEGORY);
        let entries = feed.poll(&group(), limit()).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].record().event(), &intent(position, 7).0);
        let done_position = entries[1].position();
        drop(feed);

        let port = Port::new(PortBehavior::Fail);
        let hook = Hook::new(HookBehavior::Healthy);
        let executor = rig.executor(port.clone(), hook.clone(), 2);
        assert_eq!(
            executor.step(limit()).await.unwrap(),
            ExecutorStep::Advanced {
                acked_to: done_position
            }
        );
        assert!(
            port.calls.lock().unwrap().is_empty(),
            "a stored Done must spare the port"
        );
        assert!(
            hook.calls.lock().unwrap().is_empty(),
            "a stored Done must spare the compensation hook"
        );
        assert_eq!(
            rig.records::<Record<Effect>>(OUTBOX_CATEGORY, SAGA).await,
            expected,
            "replay must append no new outcome records"
        );
        assert_eq!(executor.step(limit()).await.unwrap(), ExecutorStep::Idle);
        drop(executor);
        rig.close().await;

        let rig = Rig::open(&path).await;
        assert_eq!(
            rig.executor(port.clone(), hook.clone(), 2)
                .step(limit())
                .await
                .unwrap(),
            ExecutorStep::Idle
        );
        assert!(port.calls.lock().unwrap().is_empty());
        assert!(hook.calls.lock().unwrap().is_empty());
        assert_eq!(
            rig.records::<Record<Effect>>(OUTBOX_CATEGORY, SAGA).await,
            expected
        );
        rig.close().await;
    })
    .await;
}

/// Earlier attempts failed durably, then one succeeded, and the
/// interruption again landed before the acknowledgement. The replaying
/// executor runs the tighter budget the attempts never saw, so the
/// stored failure count already sits at the park threshold:
/// failure-budget logic alone would compensate. Durable success must
/// outrank the budget - acknowledge with no port call, no compensation,
/// and no new outcome records.
#[tokio::test]
async fn success_before_ack_beats_the_failure_budget() {
    under_deadline(async {
        let dir = tempfile::tempdir().expect("temporary database directory");
        let path = dir.path().join("saga.db");
        let rig = Rig::open(&path).await;
        let position = rig.place(7).await;
        rig.runner(ExpectedVersion::Any)
            .step(limit())
            .await
            .unwrap();
        let port = Port::new(PortBehavior::Fail);
        let hook = Hook::new(HookBehavior::Healthy);
        let executor = rig.executor(port.clone(), hook.clone(), 3);
        assert_eq!(
            executor.step(limit()).await.unwrap(),
            ExecutorStep::Holding {
                intent: key(position, 1)
            }
        );
        assert_eq!(
            executor.step(limit()).await.unwrap(),
            ExecutorStep::Holding {
                intent: key(position, 1)
            }
        );
        assert_eq!(
            rig.records::<Record<Effect>>(OUTBOX_CATEGORY, SAGA).await,
            vec![intent(position, 7), failed(position), failed(position)]
        );
        drop(executor);
        rig.db
            .category::<String, Record<Effect>>(OUTBOX_CATEGORY)
            .append(
                ExpectedVersion::Any,
                &SAGA.to_owned(),
                &EventBatch::new(vec![done(position).0]).expect("one outcome is nonempty"),
            )
            .await
            .expect("the staged success commits");
        let expected = vec![
            intent(position, 7),
            failed(position),
            failed(position),
            done(position),
        ];
        rig.close().await;

        let rig = Rig::open(&path).await;
        assert_eq!(
            rig.records::<Record<Effect>>(OUTBOX_CATEGORY, SAGA).await,
            expected
        );
        let feed = rig.db.feed::<Record<Effect>>(OUTBOX_CATEGORY);
        let entries = feed.poll(&group(), limit()).await.unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].record().event(), &intent(position, 7).0);
        let done_position = entries[3].position();
        drop(feed);

        let port = Port::new(PortBehavior::Fail);
        let hook = Hook::new(HookBehavior::Healthy);
        let executor = rig.executor(port.clone(), hook.clone(), 2);
        assert_eq!(
            executor.step(limit()).await.unwrap(),
            ExecutorStep::Advanced {
                acked_to: done_position
            }
        );
        assert!(
            port.calls.lock().unwrap().is_empty(),
            "a stored Done must spare the port even at the park threshold"
        );
        assert!(
            hook.calls.lock().unwrap().is_empty(),
            "durable success must outrank the failure budget's compensation"
        );
        assert_eq!(
            rig.records::<Record<Effect>>(OUTBOX_CATEGORY, SAGA).await,
            expected,
            "replay must append no new outcome records"
        );
        assert_eq!(executor.step(limit()).await.unwrap(), ExecutorStep::Idle);
        drop(executor);
        rig.close().await;
    })
    .await;
}

/// A restart that forgets failed attempts would grant endless retries.
/// Stored failures count toward the budget while the intent stays unacked.
///
/// At exhaustion, persist `Parked`; the hook applies compensation but fails
/// before acknowledgement.
///
/// After reopening, retry the hook without calling
/// the effect port or appending another park. Hook calls may repeat, but its
/// key-based deduplication must apply compensation once before the final ack.
#[tokio::test]
async fn durable_failures_park_and_hook_replay_dedupes() {
    under_deadline(async {
        let dir = tempfile::tempdir().expect("temporary database directory");
        let path = dir.path().join("saga.db");
        let rig = Rig::open(&path).await;
        let position = rig.place(7).await;
        rig.runner(ExpectedVersion::Any)
            .step(limit())
            .await
            .unwrap();
        let port = Port::new(PortBehavior::Fail);
        let hook = Hook::new(HookBehavior::FailAfterApply);
        assert_eq!(
            rig.executor(port.clone(), hook.clone(), 2)
                .step(limit())
                .await
                .unwrap(),
            ExecutorStep::Holding {
                intent: key(position, 1)
            }
        );
        let failed = (
            Record::Failed {
                intent: key(position, 1),
                error: PortError.to_string(),
            },
            EventMetadata::new(),
        );
        assert_eq!(
            rig.records::<Record<Effect>>(OUTBOX_CATEGORY, SAGA).await,
            vec![intent(position, 7), failed.clone()]
        );
        assert!(port.applied.lock().unwrap().is_empty());
        assert!(hook.calls.lock().unwrap().is_empty());
        rig.close().await;

        let rig = Rig::open(&path).await;
        let entries = rig
            .db
            .feed::<Record<Effect>>(OUTBOX_CATEGORY)
            .poll(&group(), limit())
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].record().event(), &intent(position, 7).0);
        assert!(matches!(
            rig.executor(port.clone(), hook.clone(), 2)
                .step(limit())
                .await,
            Err(ExecutorError::Hook(_))
        ));
        let expected = vec![
            intent(position, 7),
            failed.clone(),
            failed,
            (
                Record::Parked {
                    intent: key(position, 1),
                    attempts: NonZeroU32::new(2).unwrap(),
                    error: PortError.to_string(),
                },
                EventMetadata::new(),
            ),
        ];
        assert_eq!(
            rig.records::<Record<Effect>>(OUTBOX_CATEGORY, SAGA).await,
            expected
        );
        assert_eq!(
            &*port.calls.lock().unwrap(),
            &[
                (key(position, 1), Effect::Export { item: 7 }),
                (key(position, 1), Effect::Export { item: 7 }),
            ]
        );
        assert!(port.applied.lock().unwrap().is_empty());
        assert_eq!(
            &*hook.calls.lock().unwrap(),
            &[(key(position, 1), 2, PortError.to_string())]
        );
        assert_eq!(*hook.applied.lock().unwrap(), [key(position, 1)].into());
        rig.close().await;

        let rig = Rig::open(&path).await;
        let feed = rig.db.feed::<Record<Effect>>(OUTBOX_CATEGORY);
        let entries = feed.poll(&group(), limit()).await.unwrap();
        assert_eq!(
            entries.len(),
            4,
            "the failed hook must not acknowledge the intent"
        );
        assert_eq!(entries[0].record().event(), &intent(position, 7).0);
        let tail = entries.last().unwrap().position();
        drop(feed);
        let executor = rig.executor(
            port.with_behavior(PortBehavior::Healthy),
            hook.with_behavior(HookBehavior::Healthy),
            2,
        );
        assert_eq!(
            executor.step(limit()).await.unwrap(),
            ExecutorStep::Advanced { acked_to: tail }
        );
        assert_eq!(executor.step(limit()).await.unwrap(), ExecutorStep::Idle);
        assert_eq!(
            rig.records::<Record<Effect>>(OUTBOX_CATEGORY, SAGA).await,
            expected
        );
        assert_eq!(
            port.calls.lock().unwrap().len(),
            2,
            "park replay must not call the port"
        );
        assert_eq!(
            &*hook.calls.lock().unwrap(),
            &[
                (key(position, 1), 2, PortError.to_string()),
                (key(position, 1), 2, PortError.to_string()),
            ]
        );
        assert_eq!(*hook.applied.lock().unwrap(), [key(position, 1)].into());
        drop(executor);
        rig.close().await;

        let rig = Rig::open(&path).await;
        assert_eq!(
            rig.executor(port.clone(), hook.clone(), 2)
                .step(limit())
                .await
                .unwrap(),
            ExecutorStep::Idle
        );
        assert_eq!(hook.calls.lock().unwrap().len(), 2);
        rig.close().await;
    })
    .await;
}
