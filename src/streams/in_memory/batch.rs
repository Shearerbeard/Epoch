//! The in-memory [`AtomicStreams`] implementation (ADR 0006).
//!
//! The root's mutex is the whole locking story: one guard covers every
//! stream the batch touches or constrains, so there is no acquisition
//! order to get wrong and no window between the head observations and
//! the writes. Every check runs before the first insert, which is what
//! makes a rejected batch leave the log untouched.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use crate::decider::Event;
use crate::streams::batch::{
    expectation_satisfied, AtomicStreams, Batch, BatchBuilder, BatchConflict, ConstraintViolation,
    DuplicateWrite, StreamRef, TransactError,
};
use crate::streams::{EventBatch, ExpectedVersion, StreamId};

use super::{CategoryTypeMismatch, ErasedEvent, InMemoryDatabase, Root};

/// An event erased for the in-memory root at push, carrying the type
/// its category is claimed with at transact.
#[derive(Clone)]
pub struct MemoryEvent {
    payload: ErasedEvent,
    event_type: TypeId,
}

/// A batch the in-memory database can commit.
pub type InMemoryBatch = Batch<MemoryEvent>;

/// The builder for one.
pub type InMemoryBatchBuilder = BatchBuilder<MemoryEvent>;

impl InMemoryBatchBuilder {
    /// Add this stream's write to the batch. `expected` is checked
    /// against the stream's pre-batch head when the batch commits, the
    /// same check single append makes.
    pub fn write<Id, E>(
        &mut self,
        category: &str,
        id: &Id,
        expected: ExpectedVersion,
        events: &EventBatch<E>,
    ) -> Result<(), DuplicateWrite>
    where
        Id: StreamId,
        E: Event + Clone + Send + Sync + 'static,
    {
        // `EventBatch::as_slice` is an E1 hole outside this card's fill
        // bound; the batch's own invariant makes this nonempty.
        let erased = events
            .0
            .iter()
            .map(|event| MemoryEvent {
                payload: Arc::new(event.clone()) as Arc<dyn Any + Send + Sync>,
                event_type: TypeId::of::<E>(),
            })
            .collect();
        self.push(StreamRef::new(category, id), expected, erased)
    }
}

impl AtomicStreams for InMemoryDatabase {
    type Batch = InMemoryBatch;
    type Error = CategoryTypeMismatch;

    async fn transact(&self, batch: InMemoryBatch) -> Result<(), TransactError<Self::Error>> {
        let mut root = self.root.lock().expect("event store lock poisoned");

        // Every rejection happens before the first insert, so a failed
        // transact leaves no trace: not a stored event, and not a
        // category type claimed by a batch that never committed.
        admits_every_write(&root, &batch)?;

        for constrained in batch.constraints() {
            let observed = root.head(constrained.stream().category(), constrained.stream().key());
            if !constrained.constraint().satisfied_by(observed) {
                return Err(TransactError::ConstraintViolated(ConstraintViolation::new(
                    constrained.stream().clone(),
                    constrained.constraint(),
                    observed,
                )));
            }
        }

        for write in batch.writes() {
            let observed = root.head(write.stream().category(), write.stream().key());
            if !expectation_satisfied(write.expected(), observed) {
                return Err(TransactError::Conflict(BatchConflict::new(
                    write.stream().clone(),
                    write.expected(),
                    observed,
                )));
            }
        }

        for write in batch.writes() {
            let stream = write.stream();
            for event in write.events() {
                root.claim(stream.category(), event.event_type)?;
                root.append_erased(stream.category(), stream.key(), Arc::clone(&event.payload));
            }
        }
        Ok(())
    }
}

/// No write may contradict the event type its category already holds,
/// nor the type another write in the same batch puts in that category.
/// The second half matters for a category nothing has claimed yet:
/// checking only against the root would pass both writes, and the
/// commit loop would then append the first and reject the second.
fn admits_every_write(root: &Root, batch: &InMemoryBatch) -> Result<(), CategoryTypeMismatch> {
    let mut batch_types: HashMap<&str, TypeId> = HashMap::new();
    for write in batch.writes() {
        let category = write.stream().category();
        for event in write.events() {
            root.admits(category, event.event_type)?;
            let agreed = batch_types.insert(category, event.event_type);
            if agreed.is_some_and(|other| other != event.event_type) {
                return Err(CategoryTypeMismatch {
                    category: category.to_owned(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::batch::BatchConstraint;
    use crate::streams::{EventStreams, StreamState, StreamVersion};

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct KidHoldsCard;

    impl Event for KidHoldsCard {
        type EntityId = ();

        fn event_type(&self) -> String {
            "KidHoldsCard".to_owned()
        }

        fn get_id(&self) -> Self::EntityId {}
    }

    /// A second, unrelated event type: a batch spans categories whose
    /// event types differ, which is ADR 0006's mixed-type driver.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct CardAssigned;

    impl Event for CardAssigned {
        type EntityId = ();

        fn event_type(&self) -> String {
            "CardAssigned".to_owned()
        }

        fn get_id(&self) -> Self::EntityId {}
    }

    const KIDS: &str = "kids";
    const CHORES: &str = "chores";

    fn kid_event() -> EventBatch<KidHoldsCard> {
        // `EventBatch::new` is an E1 hole outside this card's fill
        // bound; crate-internal construction here is trivially nonempty.
        EventBatch(vec![KidHoldsCard])
    }

    fn chore_event() -> EventBatch<CardAssigned> {
        EventBatch(vec![CardAssigned])
    }

    /// A draw: one write in each of two categories, at the heads both
    /// streams were observed at.
    fn draw(db: &InMemoryDatabase, kid: &str, chore: &str) -> InMemoryBatch {
        let mut builder = db.batch();
        builder
            .write(
                KIDS,
                &kid.to_owned(),
                ExpectedVersion::NoStream,
                &kid_event(),
            )
            .expect("one write per stream");
        builder
            .write(
                CHORES,
                &chore.to_owned(),
                ExpectedVersion::NoStream,
                &chore_event(),
            )
            .expect("one write per stream");
        builder.build().expect("the batch has writes")
    }

    async fn head(db: &InMemoryDatabase, category: &str, key: &str) -> StreamVersion {
        let store = db
            .category::<KidHoldsCard>(category)
            .expect("the category holds kid events");
        match store.load_stream(&key.to_owned()).await {
            Ok(StreamState::Missing) => StreamVersion::NoStream,
            Ok(StreamState::Present(events)) => super::super::version_of(events.0.len() as u64),
            Err(error) => match error {},
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overlapping_batches_leave_exactly_one_winner() {
        for iteration in 0..20 {
            let db = InMemoryDatabase::new();
            let kid = format!("kid-{iteration}");
            let chore = format!("chore-{iteration}");

            // Both batches touch both streams, so neither can commit
            // without the other's locks: the loser must see a head its
            // `NoStream` expectation cannot accept.
            let first = draw(&db, &kid, &chore);
            let second = draw(&db, &kid, &chore);
            let (a, b) = tokio::join!(db.transact(first), db.transact(second));

            match (a, b) {
                (Ok(()), Err(TransactError::Conflict(_)))
                | (Err(TransactError::Conflict(_)), Ok(())) => {}
                (a, b) => panic!("expected exactly one winner and one conflict: {a:?}, {b:?}"),
            }

            // One winner wrote each stream once; the loser wrote
            // neither, so nothing partial survived.
            let kid_store = db.category::<KidHoldsCard>(KIDS).expect("kid category");
            let chore_store = db.category::<CardAssigned>(CHORES).expect("chore category");
            assert_eq!(
                kid_store.load_stream(&kid).await.expect("load succeeds"),
                StreamState::Present(EventBatch(vec![KidHoldsCard]))
            );
            assert_eq!(
                chore_store
                    .load_stream(&chore)
                    .await
                    .expect("load succeeds"),
                StreamState::Present(EventBatch(vec![CardAssigned]))
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_violated_constraint_aborts_the_whole_batch() {
        let db = InMemoryDatabase::new();
        let mut builder = db.batch();
        builder
            .write(
                KIDS,
                &"kid-c".to_owned(),
                ExpectedVersion::NoStream,
                &kid_event(),
            )
            .expect("one write per stream");
        builder
            .write(
                CHORES,
                &"chore-c".to_owned(),
                ExpectedVersion::NoStream,
                &chore_event(),
            )
            .expect("one write per stream");
        // A pin on a stream the batch does not write, and does not hold.
        builder.require("pool", &"pool-c".to_owned(), BatchConstraint::StreamExists);

        let outcome = db
            .transact(builder.build().expect("the batch has writes"))
            .await;
        let violation = match outcome {
            Err(TransactError::ConstraintViolated(violation)) => violation,
            other => panic!("expected a constraint violation, got {other:?}"),
        };
        assert_eq!(violation.stream().to_string(), "pool/pool-c");
        assert_eq!(violation.observed(), StreamVersion::NoStream);

        assert_eq!(head(&db, KIDS, "kid-c").await, StreamVersion::NoStream);
        assert_eq!(
            db.category::<CardAssigned>(CHORES)
                .expect("chore category")
                .load_stream(&"chore-c".to_owned())
                .await
                .expect("load succeeds"),
            StreamState::Missing,
            "a violated constraint leaves no partial insert"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_and_an_append_contend_on_one_stream() {
        let db = InMemoryDatabase::new();
        let kids = db.category::<KidHoldsCard>(KIDS).expect("kid category");
        kids.append(
            ExpectedVersion::NoStream,
            &"kid-shared".to_owned(),
            &kid_event(),
        )
        .await
        .expect("the append lands first");

        // The batch expects the stream the append just created to be
        // empty: same head, same check, so it conflicts.
        let outcome = db.transact(draw(&db, "kid-shared", "chore-shared")).await;
        let conflict = match outcome {
            Err(TransactError::Conflict(conflict)) => conflict,
            other => panic!("expected a version conflict, got {other:?}"),
        };
        assert_eq!(conflict.stream().to_string(), "kids/kid-shared");
        assert_eq!(
            head(&db, CHORES, "chore-shared").await,
            StreamVersion::NoStream,
            "the batch's other write rolled back with it"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_cannot_contradict_a_category_event_type() {
        let db = InMemoryDatabase::new();
        db.category::<KidHoldsCard>(KIDS).expect("kid category");

        let mut builder = db.batch();
        builder
            .write(
                KIDS,
                &"kid-x".to_owned(),
                ExpectedVersion::NoStream,
                &chore_event(),
            )
            .expect("one write per stream");

        assert!(matches!(
            db.transact(builder.build().expect("the batch has writes"))
                .await,
            Err(TransactError::Backend(CategoryTypeMismatch { .. }))
        ));
        assert_eq!(head(&db, KIDS, "kid-x").await, StreamVersion::NoStream);
    }

    /// Two writes disagreeing over a category nothing has claimed yet.
    /// Neither contradicts the root, so only the writes' agreement with
    /// each other can catch this - and it has to be caught before the
    /// first append, or the first write lands and the second aborts a
    /// batch that has already stored something.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_writes_cannot_disagree_over_an_unclaimed_category() {
        let db = InMemoryDatabase::new();

        let mut builder = db.batch();
        builder
            .write(
                KIDS,
                &"kid-first".to_owned(),
                ExpectedVersion::NoStream,
                &kid_event(),
            )
            .expect("one write per stream");
        builder
            .write(
                KIDS,
                &"kid-second".to_owned(),
                ExpectedVersion::NoStream,
                &chore_event(),
            )
            .expect("a different stream in the same category");

        assert!(matches!(
            db.transact(builder.build().expect("the batch has writes"))
                .await,
            Err(TransactError::Backend(CategoryTypeMismatch { .. }))
        ));
        assert_eq!(head(&db, KIDS, "kid-first").await, StreamVersion::NoStream);
        assert_eq!(
            head(&db, KIDS, "kid-second").await,
            StreamVersion::NoStream,
            "the rejected batch stored neither write"
        );
    }
}
