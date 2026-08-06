<!-- vale off -->
<!-- Archived research report, kept near-verbatim (quotes upstream code,
     protos, and talks); style lint suppressed on purpose. -->

# EvidentDB research report

Research pass for the 2026-08 redesign (feeds ADR 0006; the
`BatchConstraint` vocabulary is adopted there minus Subject).

## Project identity and status

- **What it is:** EvidentDB is "an event store for use in event sourcing...
  written in Kotlin and built atop pluggable backend storage systems,
  including Apache Kafka" (README). Events are CNCF **CloudEvents**; the
  client/server API is **gRPC**. Copyright 2022 Evident Systems LLC (Bobby
  Calderwood), **Apache-2.0** licensed.
- **Status: effectively dead/dormant.** The canonical repo
  `github.com/evidentsystems/evident-db` (note: `evident-db`, not
  `evidentdb`) now returns **404** — deleted or made private; GitHub's
  search index still points at it. A complete copy with Calderwood's commit
  history survives at **https://github.com/devdoshi/evident-db** (last
  commit by Bobby Calderwood, 2024-01-26, "Optimize imports"). The README
  declares it "ALPHA quality software... shouldn't be used in production,"
  never published to artifact repositories (`0.1.0-alpha-SNAPSHOT` only).
  The **evidentdb.com domain is parked for sale on GoDaddy**. The
  `evidentsystems` GitHub org still exists but no longer lists the repo;
  Calderwood's active commercial work is Evidentstack/oNote
  (event-modeling SaaS) and the Confluent "Practical Event Modeling"
  course.

## 1. Core model: database / batch / stream, and conditional appends

The model is heavily **Datomic-flavored** at the API surface:
`client.createDatabase("foo")`, `connectDatabase` → `Connection`,
`conn.db()` returns an **immutable database value** at a revision;
`conn.sync(revision)` and `databaseAtRevision` give as-of database values;
`conn.log()` walks the batch log (mirrors Datomic's `conn`/`db`/`sync`/
`log`). From `clients/jvm/README.md`: "No print statements, databases are
immutable!"

- **Database** = the unit of the log and of consistency:
  `message Database { string name; uint64 revision; }`. `revision` is a
  single monotonically increasing counter over *all* events in the database
  (event revisions are global sequence numbers, not per-stream).
- **Batch** = the transaction:
  `message Batch { string database; repeated CloudEvent events; Timestamp timestamp; uint64 basis_revision; }`.
  A batch records the `basis_revision` (database revision it was validated
  against).
- **Stream** = a *named index within one database*, not a physical log.
  Each proposed CloudEvent designates its stream via its `source`
  attribute (the autonomo example builds events with
  `.withSource(URI("vehicles"))`; the server resolves it against the
  database's base URI). Queries exist for `stream`, `subject`,
  `subjectStream` (subject-within-stream), and `eventType` — streams and
  CloudEvents `subject` are both first-class index dimensions.

**Conditional append / optimistic concurrency — the multi-stream-atomic
claim is TRUE.** The transaction RPC (`interface/service.proto`):

```proto
rpc transactBatch(TransactBatchRequest) returns (TransactBatchReply) {}

message TransactBatchRequest {
  string database = 1;
  repeated io.cloudevents.v1.CloudEvent events = 2;   // events may target DIFFERENT streams
  repeated BatchConstraint constraints = 3;           // constraints may reference ANY streams/subjects
}
```

`BatchConstraint` (`interface/domain.proto`) is a `oneof` with nine
variants: `StreamExists`, `StreamDoesNotExist`,
`StreamMaxRevision{stream, revision}`, plus the same three for `Subject`
and for `SubjectOnStream`. So one batch can carry events for streams A, B,
C **and** a list of preconditions over any streams/subjects (including
streams the batch doesn't write), all evaluated against one consistent
`basis_revision` of the whole database, and either the entire batch
commits (bumping the single database revision) or the whole thing is
rejected. The server-side check is in
`server/domain/src/main/kotlin/com/evidentdb/server/domain_model/api.kt`
(`satisfiesBatchConstraint` on `ActiveDatabaseCommandModel` /
`DirtyDatabaseCommandModel`), invoked from `CommandService.transactBatch`
(`server/domain/.../application/command.kt`), which validates the
`ProposedBatch(events, constraints)` and persists via one
`repository.saveDatabase(dirtyModel)`. This is strictly more expressive
than EventStoreDB-style per-stream `expectedVersion`: it's a multi-stream
compare-and-append against a database-wide snapshot.

The client mirrors it: `BatchProposal(cloudEvents, constraints)` where
constraints are built from a db value, e.g.
`BatchConstraint.SubjectMaxRevisionOnStream("vehicles", vin, db.revision)`
(autonomo example, `examples/autonomo/.../adapters/evidentdb.kt`).

## 2. The "event router" claim — mostly garbled

No component named "event router" exists in EvidentDB, and multi-stream
*use cases* are not spawned by routing. Two real things the claim likely
conflates:

- **Internal Kafka plumbing (routing, not use cases):** In the Kafka
  Streams transactor topology (`server/transactor/.../topology.kt`, older
  generation of the code), accepted userspace events flow through an
  `EVENT_INDEXER` to a `USERSPACE_EVENTS` sink that uses a
  `DatabaseTopicNameExtractor` (`server/adapters/.../kafka/topic.kt`) to
  route each event to its **per-database log topic** via a `topic` header.
  That is an event router in the Kafka `TopicNameExtractor` sense, but it
  routes to database topics for storage/replication — it does not trigger
  command processing.
- **The `React`/Saga domain function (Calderwood's Domain Functions model,
  application-side):** In his Confluent "Practical Event Modeling" course
  and in the repo's `examples/autonomo/.../domain_functions.kt`, the
  function types are `Decide (C, S) -> Result<List<E>>`,
  `Evolve (S, E) -> S`, and `React (AR) -> List<A>` (`data class Saga`).
  React "facilitates integration among event streams": it translates
  events from one stream into commands against another (the example —
  commented out in the repo — maps `RideScheduled` →
  `MarkVehicleOccupied` on the vehicle stream). This is the mechanism that
  "spawns" cross-stream workflows, but it lives in **application code, not
  the database**, and its consistency is **eventual** — the follow-on
  command goes through its own decide/transactBatch with its own
  constraints, and can fail/retry independently. EvidentDB itself only
  guarantees the atomic batch.

Accurate statement: *EvidentDB supports multi-stream use cases directly
via atomic multi-stream batches with database-wide constraints;
cross-stream automation in Calderwood's model is done by
application-level React/Saga functions (event→command translation), not by
a database-internal event router.*

## 3. Design lineage and multi-aggregate invariants

The older-generation server is a **Datomic-style single-writer transactor
built on Kafka Streams**: gRPC frontends publish `TransactBatch` commands
to an internal commands topic **partitioned by database name**
(`DatabaseStreamPartitioner`, `partitionByDatabase`); a single
`COMMAND_PROCESSOR` per partition serially validates each command against
RocksDB state stores (DATABASE/LOG/BATCH/STREAM/EVENT stores) and emits
accepted events — so all writes to a given database are serialized through
one processor, exactly the Commander-pattern single-writer Calderwood
described in "Toward a Functional Programming Analogy for Microservices"
(Capital One Tech, 2017): commands and events on immutable logs, "single
writer principle," pure functional core deciding command→events,
aggregations building materialized views, exactly-once via Kafka
transactions. Per-database log topics are created with **1 partition**
(`DATABASE_LOG_TOPIC_PARTITIONS = 1`), infinite retention. The Jan-2024
rewrite restructured this hexagonally (`CommandService` + `Decider` +
pluggable `WritableDatabaseRepository`, with an in-memory adapter
alongside Kafka).

**Consequence for multi-aggregate invariants:** because the serialization
unit is the *database* (not the stream/aggregate) and constraints range
over the whole database at one `basis_revision`, invariants spanning
multiple aggregates *can be enforced transactionally* in one batch — e.g.
append a `RideScheduled` to the rides stream while asserting
`SubjectMaxRevisionOnStream("vehicles", vin, r)` so it fails if the
vehicle changed concurrently. The trade-off is classic Datomic: one writer
per database caps write throughput, and scaling out means sharding into
multiple databases, across which you're back to sagas/React functions and
eventual consistency.

## 4. Sources

- Repo copy (canonical content, Calderwood's commits):
  https://github.com/devdoshi/evident-db — key files: `/README.md`,
  `/clients/jvm/README.md`, `/interface/domain.proto`,
  `/interface/service.proto`,
  `/server/domain/src/main/kotlin/com/evidentdb/server/application/command.kt`,
  `/server/domain/src/main/kotlin/com/evidentdb/server/domain_model/api.kt`,
  `/server/transactor/src/main/kotlin/com/evidentdb/transactor/topology.kt`,
  `/server/adapters/src/main/kotlin/com/evidentdb/kafka/topic.kt`,
  `/examples/autonomo/src/main/kotlin/com/evidentdb/examples/autonomo/domain_functions.kt`
- Dead canonical repo (404): https://github.com/evidentsystems/evident-db
- Domain parked for sale: https://evidentdb.com
- [Toward a Functional Programming Analogy for Microservices — Calderwood, Capital One Tech](https://medium.com/capital-one-tech/toward-a-functional-programming-analogy-for-microservices-ba6f49b94ad)
  (also on [Confluent's blog](https://www.confluent.io/blog/toward-functional-programming-analogy-microservices/),
  [talk video](https://www.youtube.com/watch?v=idxguWO6VLE))
- [Practical Event Modeling course — Domain Functions (Confluent)](https://developer.confluent.io/courses/event-modeling/domain-functions/)
- [Confluent podcast: Using Event Modeling ft. Bobby Calderwood](https://developer.confluent.io/podcast/using-event-modeling-to-architect-event-driven-information-systems-ft-bobby-calderwood)
- [Kafka Summit 2020 slides — Building Information Systems using Event Modeling](https://www.slideshare.net/slideshow/building-information-systems-using-event-modeling-bobby-calderwood-evident-systems-kafka-summit-2020/238435497)
- [Evidentstack team page](https://evidentstack.com/team)
