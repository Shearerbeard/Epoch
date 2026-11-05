# E28 - the state before this card, and what is left after it

Status: written after the card's final reviews. Documentation only; no
code changed in this round. The card's own design note is
`e28-sqlite-streams-DESIGN.md`, and the module's findings ledger is
`../src/streams/sqlite/DESIGN.md`.

This note answers two questions the reviews raised between them. What
did SQLite support before E28, and was anything already wrong then that
this card should have fixed? And what did the reviews leave open, with
what size and priority?

## What existed at the branch point (`e85018b`)

The crate carried two storage surfaces side by side.

| | Old repository surface | New streams surface |
|---|---|---|
| Trait | `VersionedEventRepositoryWithStreams` | `EventStreams`, `AtomicStreams`, `EventFeed` |
| Stream id | `String` | typed `StreamId` |
| Position | per-backend version type (`SqliteVersion(i64)`) | `StreamSequence(NonZeroU64)` |
| SQLite backend | yes, `src/repository/sqlite/`, landed by E26 | **none** |
| PostgreSQL backend | yes | yes |
| In-memory backend | yes | yes |
| Feed | no | PostgreSQL only |
| Schema | `schema.sql`, `CREATE TABLE IF NOT EXISTS` | numbered steps with a ledger |

So SQLite already had a repository backend. What it did not have was a
backend for the new surface, which is what E28 adds. The two are
siblings at the same layer: E28 does not call, wrap, or adapt
`SqliteEventRepository`, and it shares neither its schema nor its
migration style.

## What a consumer could not do before E28

- Run the new surface embedded. A consumer wanting typed stream ids,
  atomic multi-stream batches, or a feed needed PostgreSQL.
- Test-drive saga or feed behavior without a service. This is the gap
  that minted the card, after E27's CLI made it concrete.
- Use the old surface's feed, because there is no old-surface feed.

## What was already wrong before E28

One claim, and it is in the old surface's schema comment:

```
-- The rowid doubles as the global sequence; AUTOINCREMENT
-- keeps ids monotonic and unreused.
```

E28 measured the rollback case while building the feed's honest-cursor
argument: a rolled-back `AUTOINCREMENT` id is reused, because the
high-water mark reverts with the transaction. The reuse is safe, since
it always lands above the highest committed id, but the comment states
the opposite of what happens. The old surface has no cursor, so nothing
depends on the claim. The sentence still misleads a reader, the card's
design note does not repeat it, and it is a one-line fix whenever that
file is next touched.

Nothing else in the old surface is wrong in a way E28's reviews found.
Its `migrate` installs the busy timeout and the WAL pragma. Its
`CREATE TABLE IF NOT EXISTS` is idempotent, and its version type is
correct for its own trait. The ledger-based migration story is a
deliberate improvement on the new surface rather than a repair of a
defect.

## What E28 deliberately did not fix

- **No migration path between the two surfaces.** They coexist, and a
  consumer chooses one. Bridging them is a separate decision with its
  own data-migration cost, and nothing in this card needs it.
- **No saga runner or outbox executor.** The intent-key index is the
  storage piece a later saga card needs; the runner is not here.
- **No stored-byte comparison with another backend.** That is E29's
  deliverable, and the bytes differ by design (D3).

## The follow-ups the reviews left, with sizes

Sizes are estimates for the change only, and none of them is in this
PR.

| # | Item | Source | Size | Why deferred |
|---|---|---|---|---|
| 1 | Cancelled `transact`, poll, and ack cases | Fills review finding 2; store/batch review finding 6 | ~60 lines of tests, no production change | The actor property is already pinned for append; these are coverage symmetry. One card covers all three. |
| 2 | A fixture that pins the clamped-bind edge at `i64::MAX` | Store review finding 2 | ~40 lines, both backends | Observable only with ~9.2e18 events in one stream. Pinning it for both backends together is one test. |
| 3 | A checksum on the migration ledger | Store review finding 7 | ~40 lines plus one migration step | Immutability is convention-only today, and PostgreSQL holds the same posture. Additive if drift ever bites. |
| 4 | `map_or(0, ...)` becomes an `expect` in `load_stream_from` | Store review finding 1 | 1 line | Posture alignment with PostgreSQL; the arm is unreachable either way. |
| 5 | Corrupt-row panic posture | Frontier feed review finding 4 | Not sized | A cross-backend decision. SQLite behaving differently from PostgreSQL here would be worse than the shared posture. |
| 6 | Read-path `SQLITE_BUSY` classification | Store review finding 3 | Not sized | Unreachable under WAL, and a non-WAL database fails on the missing table first. Only matters if non-WAL becomes supported. |
| 7 | The old schema's `AUTOINCREMENT` comment | This note | 1 line in `src/repository/sqlite/schema.sql` | It states the opposite of the measured behavior. Worth taking whenever that file is next touched. |

No follow-up changes behavior. Items 1, 2, and 4 are tests or a
posture assertion; 3 and 6 are conditional on events that have not
happened; 5 belongs to a cross-backend decision; 7 is a comment.

## Recommendation

Merge E28 as it stands. Open one follow-up card for items 1 and 2,
which are the same class - conformance coverage for boundaries the
suite reasons about but does not exercise. Leave the rest as recorded
posture rather than backlog. The checksum has no caller yet and the
BUSY path cannot fire. The panic policy should change for all three
backends at once if it changes at all. Item 7 is a one-line correction
to take with the next touch of the old surface.
