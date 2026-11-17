//! A checkout saga driven by real Epoch SQLite streams, runner, and
//! outbox executor, with a fake Stripe port at the external edge.
//!
//! Run it with:
//!
//! ```text
//! cargo run --example checkout_saga --features sqlite
//! ```
//!
//! The application records one `CheckoutRequested` source event. A
//! pure saga reacts with two durable command requests and one external
//! effect request. The runner writes both command ledgers and the
//! outbox intent in one SQLite transaction; the executor calls the
//! fake port later and records `Done`. The numbered output follows
//! that path and keeps business records, outbox intents, and outbox
//! outcomes visibly separate.
//!
//! Three boundaries are deliberate:
//!
//! 1. **Commands request work.** `ReserveStock` and
//!    `MarkPaymentPending` are named for the work asked of their
//!    ledgers. This example does not run inventory or payment domain
//!    handlers. A real application would trigger payment
//!    from a `StockReserved` event if payment depended on a successful
//!    reservation.
//! 2. **Stripe is outside the transaction.** The runner commits the
//!    intent; the executor calls the port afterward, then appends
//!    `Done` and acks the outbox entry. `Done` means the port call
//!    completed, not that anyone paid. A Stripe webhook or result
//!    would be needed for a business `PaymentSucceeded` event.
//! 3. **The Stripe types are a modeled subset.** They mirror only the
//!    selected REST fields used here, not an SDK binding. The fake
//!    holds no credentials and makes no network calls.

#[cfg(feature = "sqlite")]
mod checkout {
    use std::collections::HashMap;
    use std::convert::Infallible;
    use std::error::Error;
    use std::sync::{Arc, Mutex};

    use serde::{Deserialize, Serialize};
    use thiserror::Error;
    use tokio_rusqlite::Connection;

    use epoch::decider::Event;
    use epoch::streams::feed::PollLimit;
    use epoch::streams::outbox::{
        CompensationHook, EffectPort, Executor, ExecutorStep, ParkedNotice, Record, OUTBOX_CATEGORY,
    };
    use epoch::streams::saga::{
        Command, CommandGroup, EffectRequest, IntentGroup, Reaction, ReactionFold, Runner,
        RunnerStep, Saga,
    };
    use epoch::streams::sqlite::{
        SqliteBatchBuilder, SqliteDatabase, SqliteEventFeed, SqliteEventStreams, SqliteWriteError,
    };
    use epoch::streams::{
        EmptyBatch, EventBatch, EventStreams, ExpectedVersion, RecordedEvent, RenderedIntentKey,
        RetryPolicy, SagaId, StreamRef, StreamState,
    };

    const CHECKOUT_CATEGORY: &str = "checkout";
    const INVENTORY_COMMANDS: &str = "inventory-commands";
    const ORDER_COMMANDS: &str = "order-commands";
    const SAGA_ID: &str = "checkout-saga";

    /// The two wire values this modeled subset can produce.
    const PAYMENT_INTENT_OBJECT: &str = "payment_intent";
    const REQUIRES_PAYMENT_METHOD: &str = "requires_payment_method";

    // -----------------------------------------------------------------
    // The application and the source event
    // -----------------------------------------------------------------

    /// The command that starts a checkout in the tiny application.
    #[derive(Debug)]
    struct InitCheckout {
        checkout_id: String,
        sku: String,
        quantity: u32,
        amount: u32,
        currency: String,
    }

    /// What the application records. The saga is the only consumer.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct CheckoutRequested {
        checkout_id: String,
        sku: String,
        quantity: u32,
        /// Integer smallest currency units, as Stripe's API takes it.
        amount: u32,
        /// Lowercase three-letter code, as Stripe's API takes it.
        currency: String,
    }

    impl Event for CheckoutRequested {
        type EntityId = String;

        fn event_type(&self) -> String {
            "CheckoutRequested".to_owned()
        }

        fn get_id(&self) -> Self::EntityId {
            self.checkout_id.clone()
        }
    }

    /// One command, one source event: the application checks nothing
    /// here, because this example is about the saga seam, not pricing
    /// or stock policy.
    fn handle_init_checkout(command: InitCheckout) -> CheckoutRequested {
        CheckoutRequested {
            checkout_id: command.checkout_id,
            sku: command.sku,
            quantity: command.quantity,
            amount: command.amount,
            currency: command.currency,
        }
    }

    // -----------------------------------------------------------------
    // The saga's command and effect payloads
    // -----------------------------------------------------------------

    /// A durable request for a ledger to do something. Neither variant
    /// claims the business command has already run.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    enum CheckoutCommand {
        ReserveStock {
            checkout_id: String,
            sku: String,
            quantity: u32,
        },
        MarkPaymentPending {
            checkout_id: String,
            /// Integer smallest currency units.
            amount: u32,
        },
    }

    impl Event for CheckoutCommand {
        type EntityId = String;

        fn event_type(&self) -> String {
            match self {
                Self::ReserveStock { .. } => "ReserveStock".to_owned(),
                Self::MarkPaymentPending { .. } => "MarkPaymentPending".to_owned(),
            }
        }

        fn get_id(&self) -> Self::EntityId {
            match self {
                Self::ReserveStock { checkout_id, .. }
                | Self::MarkPaymentPending { checkout_id, .. } => checkout_id.clone(),
            }
        }
    }

    /// The effect request recorded in the outbox. The checkout id is
    /// local correlation, not a Stripe body field.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct CreatePaymentIntent {
        checkout_id: String,
        amount: u32,
        currency: String,
    }

    // -----------------------------------------------------------------
    // The pure saga
    // -----------------------------------------------------------------

    /// Deterministic by construction: the same source event always
    /// produces the same reactions in the same order.
    struct CheckoutSaga {
        id: SagaId,
    }

    impl Saga for CheckoutSaga {
        type Event = CheckoutRequested;
        type Command = CheckoutCommand;
        type Effect = CreatePaymentIntent;

        fn id(&self) -> &SagaId {
            &self.id
        }

        fn react(
            &self,
            event: &CheckoutRequested,
        ) -> Vec<Reaction<CheckoutCommand, CreatePaymentIntent>> {
            vec![
                Command::new(
                    StreamRef::new(INVENTORY_COMMANDS, &event.checkout_id),
                    ExpectedVersion::NoStream,
                    CheckoutCommand::ReserveStock {
                        checkout_id: event.checkout_id.clone(),
                        sku: event.sku.clone(),
                        quantity: event.quantity,
                    },
                )
                .into(),
                Command::new(
                    StreamRef::new(ORDER_COMMANDS, &event.checkout_id),
                    ExpectedVersion::NoStream,
                    CheckoutCommand::MarkPaymentPending {
                        checkout_id: event.checkout_id.clone(),
                        amount: event.amount,
                    },
                )
                .into(),
                EffectRequest::new(CreatePaymentIntent {
                    checkout_id: event.checkout_id.clone(),
                    amount: event.amount,
                    currency: event.currency.clone(),
                })
                .into(),
            ]
        }
    }

    // -----------------------------------------------------------------
    // The fold: typed groups to SQLite writes
    // -----------------------------------------------------------------

    #[derive(Debug, Error)]
    enum FoldError {
        #[error(transparent)]
        Batch(#[from] EmptyBatch),
        #[error(transparent)]
        Write(#[from] SqliteWriteError),
    }

    /// Writes one group per stream, keeping each record's runner
    /// envelope and copying each intent key into its payload.
    struct CheckoutFold;

    impl ReactionFold<SqliteBatchBuilder> for CheckoutFold {
        type Command = CheckoutCommand;
        type Effect = CreatePaymentIntent;
        type Error = FoldError;

        fn push_command_group(
            &self,
            builder: &mut SqliteBatchBuilder,
            group: CommandGroup<'_, CheckoutCommand>,
        ) -> Result<(), FoldError> {
            let records: Vec<RecordedEvent<CheckoutCommand>> = group
                .records()
                .iter()
                .map(|record| {
                    RecordedEvent::keyed(record.payload().clone(), record.metadata().clone())
                })
                .collect();
            let batch = EventBatch::from_records(records)?;
            builder.write(
                group.stream().category(),
                &group.stream().key().to_owned(),
                group.expected(),
                &batch,
            )?;
            Ok(())
        }

        fn push_intent_group(
            &self,
            builder: &mut SqliteBatchBuilder,
            group: IntentGroup<'_, CreatePaymentIntent>,
        ) -> Result<(), FoldError> {
            let records: Vec<RecordedEvent<Record<CreatePaymentIntent>>> = group
                .records()
                .iter()
                .map(|record| {
                    RecordedEvent::keyed(
                        Record::Intent {
                            intent: record.key().clone(),
                            request: record.payload().clone(),
                        },
                        record.metadata().clone(),
                    )
                })
                .collect();
            let batch = EventBatch::from_records(records)?;
            builder.write(
                group.stream().category(),
                &group.stream().key().to_owned(),
                ExpectedVersion::Any,
                &batch,
            )?;
            Ok(())
        }
    }

    // -----------------------------------------------------------------
    // The modeled Stripe subset and its fake port
    // -----------------------------------------------------------------

    /// A modeled subset of Stripe's PaymentIntent response, carrying
    /// only the selected fields this example uses. This is not a
    /// complete API binding.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct PaymentIntent {
        id: String,
        /// Wire spelling of the object type.
        object: &'static str,
        /// Integer smallest currency units.
        amount: u32,
        /// Lowercase three-letter code.
        currency: String,
        /// Wire spelling of the status at creation without confirming.
        status: &'static str,
    }

    #[derive(Default)]
    struct FakeState {
        intents: HashMap<RenderedIntentKey, PaymentIntent>,
        calls: u32,
        created: u32,
    }

    /// A local fake, not a Stripe simulator: it creates one typed
    /// response per idempotency key and returns it again on a repeat.
    #[derive(Default)]
    struct FakeStripe {
        state: Mutex<FakeState>,
    }

    impl FakeStripe {
        fn calls(&self) -> u32 {
            self.state
                .lock()
                .expect("the fake's state is not poisoned")
                .calls
        }

        fn created(&self) -> u32 {
            self.state
                .lock()
                .expect("the fake's state is not poisoned")
                .created
        }

        /// The modeled `POST /v1/payment_intents` call. The key stands
        /// for Stripe's `Idempotency-Key` request header; it is not a
        /// field in the request body. There is a real async yield and
        /// no network call.
        async fn create_payment_intent(
            &self,
            idempotency_key: &RenderedIntentKey,
            request: &CreatePaymentIntent,
        ) -> PaymentIntent {
            tokio::task::yield_now().await;

            let mut state = self.state.lock().expect("the fake's state is not poisoned");
            state.calls += 1;
            if let Some(existing) = state.intents.get(idempotency_key) {
                return existing.clone();
            }

            state.created += 1;
            let intent = PaymentIntent {
                id: format!("pi_fake_{}", state.created),
                object: PAYMENT_INTENT_OBJECT,
                amount: request.amount,
                currency: request.currency.clone(),
                status: REQUIRES_PAYMENT_METHOD,
            };
            state
                .intents
                .insert(idempotency_key.clone(), intent.clone());
            intent
        }
    }

    /// The adapter from the effect payload to the fake. It inspects
    /// the typed response, then reports the call completed.
    struct StripePort {
        stripe: Arc<FakeStripe>,
    }

    impl EffectPort<CreatePaymentIntent> for StripePort {
        type Error = Infallible;

        async fn perform(
            &self,
            intent: &RenderedIntentKey,
            request: &CreatePaymentIntent,
        ) -> Result<(), Infallible> {
            println!(
                "   POST /v1/payment_intents amount={}, currency={}, confirm=false",
                request.amount, request.currency
            );
            println!("   Idempotency-Key: {intent}");

            let response = self.stripe.create_payment_intent(intent, request).await;

            println!("   response: {response:?}");
            assert_eq!(response.object, PAYMENT_INTENT_OBJECT);
            assert_eq!(response.amount, request.amount);
            assert_eq!(response.currency, request.currency);
            assert_eq!(response.status, REQUIRES_PAYMENT_METHOD);
            Ok(())
        }
    }

    /// Nothing in this example reaches the retry budget, so the hook
    /// has no compensation to perform.
    struct NoCompensation;

    impl CompensationHook<CreatePaymentIntent> for NoCompensation {
        type Error = Infallible;

        async fn on_parked(
            &self,
            _notice: &ParkedNotice<CreatePaymentIntent>,
        ) -> Result<(), Infallible> {
            Ok(())
        }
    }

    // -----------------------------------------------------------------
    // Reading and printing
    // -----------------------------------------------------------------

    fn records<E>(state: StreamState<E>) -> Vec<RecordedEvent<E>> {
        match state {
            StreamState::Present(batch) => batch.into_records(),
            StreamState::Missing => Vec::new(),
        }
    }

    fn print_outbox(label: &str, entries: &[RecordedEvent<Record<CreatePaymentIntent>>]) {
        println!("   {label}: {} record(s)", entries.len());
        for entry in entries {
            match entry.event() {
                Record::Intent { intent, request } => {
                    println!("     intent  key={intent} request={request:?}");
                }
                Record::Done { intent } => {
                    println!("     done    key={intent} (port call completed, not payment)");
                }
                Record::Failed { intent, error } => {
                    println!("     failed  key={intent} error={error}");
                }
                Record::Parked {
                    intent,
                    attempts,
                    error,
                } => {
                    println!(
                        "     parked  key={intent} attempts={} error={error}",
                        attempts.get()
                    );
                }
            }
        }
    }

    fn limit(raw: usize) -> PollLimit {
        PollLimit::new(raw).expect("the example asks for a nonzero poll limit")
    }

    // -----------------------------------------------------------------
    // The walkthrough
    // -----------------------------------------------------------------

    pub async fn run() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("checkout.db");
        let path = path.to_str().expect("the temp path is utf8");

        println!("== opening a temporary SQLite database");
        println!("   {path}");

        let database = SqliteDatabase::new(Connection::open(path).await?);
        database.migrate().await?;

        let source: SqliteEventStreams<String, CheckoutRequested> =
            database.category(CHECKOUT_CATEGORY);
        let source_feed: SqliteEventFeed<CheckoutRequested> = database.feed(CHECKOUT_CATEGORY);
        let inventory: SqliteEventStreams<String, CheckoutCommand> =
            database.category(INVENTORY_COMMANDS);
        let orders: SqliteEventStreams<String, CheckoutCommand> = database.category(ORDER_COMMANDS);
        let outbox: SqliteEventStreams<String, Record<CreatePaymentIntent>> =
            database.category(OUTBOX_CATEGORY);
        let outbox_feed: SqliteEventFeed<Record<CreatePaymentIntent>> =
            database.feed(OUTBOX_CATEGORY);

        let saga_key = SAGA_ID.to_owned();
        let saga_id = SagaId::new(SAGA_ID)?;
        let stripe = Arc::new(FakeStripe::default());

        // -------------------------------------------------------------
        println!("\n== 1. the application records the source event");
        // -------------------------------------------------------------
        let command = InitCheckout {
            checkout_id: "checkout-1".to_owned(),
            sku: "sku-42".to_owned(),
            quantity: 2,
            amount: 5_000,
            currency: "usd".to_owned(),
        };
        println!("   command: {command:?}");
        let requested = handle_init_checkout(command);
        let checkout_id = requested.checkout_id.clone();

        source
            .append(
                ExpectedVersion::NoStream,
                &checkout_id,
                &EventBatch::new(vec![requested.clone()])?,
            )
            .await?;

        let source_records = records(source.load_stream(&checkout_id).await?);
        assert_eq!(source_records.len(), 1);
        assert_eq!(source_records[0].event(), &requested);
        println!(
            "   business record {CHECKOUT_CATEGORY}/{checkout_id}: {:?}",
            source_records[0].event()
        );

        // -------------------------------------------------------------
        println!("\n== 2. the runner commits both ledgers and the outbox intent");
        // -------------------------------------------------------------
        let runner = Runner::new(
            CheckoutSaga {
                id: saga_id.clone(),
            },
            database.clone(),
            source_feed.clone(),
            outbox.clone(),
            CheckoutFold,
            RetryPolicy::default(),
        );

        let runner_step = runner.step(limit(10)).await?;
        assert!(
            matches!(runner_step, RunnerStep::Advanced { .. }),
            "the runner processes the source event: {runner_step:?}"
        );
        println!("   runner step: {runner_step:?}");

        let inventory_records = records(inventory.load_stream(&checkout_id).await?);
        let order_records = records(orders.load_stream(&checkout_id).await?);
        assert_eq!(inventory_records.len(), 1);
        assert_eq!(order_records.len(), 1);
        assert_eq!(
            inventory_records[0].event(),
            &CheckoutCommand::ReserveStock {
                checkout_id: checkout_id.clone(),
                sku: "sku-42".to_owned(),
                quantity: 2,
            }
        );
        assert_eq!(
            order_records[0].event(),
            &CheckoutCommand::MarkPaymentPending {
                checkout_id: checkout_id.clone(),
                amount: 5_000,
            }
        );

        println!(
            "   command {INVENTORY_COMMANDS}/{checkout_id}: {:?}",
            inventory_records[0].event()
        );
        println!(
            "   command {ORDER_COMMANDS}/{checkout_id}: {:?}",
            order_records[0].event()
        );

        let before = records(outbox.load_stream(&saga_key).await?);
        assert_eq!(before.len(), 1);
        let (intent_key, intent_request) = before
            .iter()
            .find_map(|entry| match entry.event() {
                Record::Intent { intent, request } => Some((intent.clone(), request.clone())),
                Record::Done { .. } | Record::Failed { .. } | Record::Parked { .. } => None,
            })
            .expect("the runner committed one outbox intent");
        print_outbox("outbox", &before);

        // -------------------------------------------------------------
        println!("\n== 3. before the executor runs");
        // -------------------------------------------------------------
        println!("   business records: 1 source event and 2 durable command requests");
        println!("   no stock reservation, stock validation, or paid order exists");
        println!("   outbox: 1 intent, 0 outcomes");
        println!("   fake Stripe calls: {}", stripe.calls());
        assert_eq!(stripe.calls(), 0, "the runner never calls the port");
        assert_eq!(stripe.created(), 0);

        // -------------------------------------------------------------
        println!("\n== 4. the executor performs the effect and records Done");
        // -------------------------------------------------------------
        let executor = Executor::new(
            outbox_feed,
            outbox.clone(),
            StripePort {
                stripe: Arc::clone(&stripe),
            },
            NoCompensation,
            saga_id,
            RetryPolicy::default(),
        );

        let executor_step = executor.step(limit(10)).await?;
        assert!(
            matches!(executor_step, ExecutorStep::Advanced { .. }),
            "the executor performs the intent and acks it: {executor_step:?}"
        );
        println!("   executor step: {executor_step:?}");
        println!(
            "   fake Stripe calls: {}, objects created: {}",
            stripe.calls(),
            stripe.created()
        );
        assert_eq!(stripe.calls(), 1);
        assert_eq!(stripe.created(), 1);

        // -------------------------------------------------------------
        println!("\n== 5. the outbox after execution");
        // -------------------------------------------------------------
        let after = records(outbox.load_stream(&saga_key).await?);
        assert_eq!(after.len(), 2);
        assert!(matches!(after[0].event(), Record::Intent { .. }));
        assert!(matches!(
            after[1].event(),
            Record::Done { intent } if intent == &intent_key
        ));
        print_outbox("outbox", &after);

        let inventory_after = records(inventory.load_stream(&checkout_id).await?);
        let orders_after = records(orders.load_stream(&checkout_id).await?);
        assert_eq!(inventory_after.len(), 1);
        assert_eq!(orders_after.len(), 1);
        println!(
            "   business records unchanged: inventory={}, orders={}",
            inventory_after.len(),
            orders_after.len()
        );

        // -------------------------------------------------------------
        println!("\n== 6. a repeated call with the same key returns the same fake object");
        // -------------------------------------------------------------
        let first = stripe
            .create_payment_intent(&intent_key, &intent_request)
            .await;
        let second = stripe
            .create_payment_intent(&intent_key, &intent_request)
            .await;
        assert_eq!(first, second);
        assert_eq!(stripe.created(), 1);
        assert_eq!(stripe.calls(), 3);

        println!("   call 2: id={}, status={}", first.id, first.status);
        println!("   call 3: id={}, status={}", second.id, second.status);
        println!(
            "   fake Stripe calls: {}, objects created: {}",
            stripe.calls(),
            stripe.created()
        );
        println!("   the stable key models the header; this is not an exactly-once promise");
        println!("   for real Stripe, whose replay safety the API owns");

        println!("\n== done");
        Ok(())
    }
}

#[cfg(feature = "sqlite")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    checkout::run().await
}

#[cfg(not(feature = "sqlite"))]
fn main() {
    eprintln!(
        "This example drives the SQLite backend. Run it with:\n\
         \n    cargo run --example checkout_saga --features sqlite\n"
    );
    std::process::exit(1);
}
