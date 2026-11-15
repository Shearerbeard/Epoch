#![cfg(feature = "sqlite")]

mod support;

/// Saving the command without its intent would lose the requested effect.
/// A successful runner step must persist both records in one batch
/// and acknowledge the source only after that batch commits.
#[tokio::test]
async fn runner_commits_command_and_intent_atomically() {
    todo!()
}

/// Simulate a crash after the batch commits but before source acknowledgement.
/// With `Any`, version checks allow replay, but intent uniqueness must reject
/// the entire repeated batch as `DuplicateIntent`. The runner must recognize
/// the stored intent and acknowledge without leaving another command or intent.
#[tokio::test]
async fn any_replay_is_typed_duplicate_and_acks_once() {
    todo!()
}

/// Replay the same commit-before-ack crash with a `NoStream` command write.
/// The first commit created the stream, so replay hits `Conflict` before the
/// uniqueness check. The stored intent must still prove prior success: the
/// runner acknowledges, and neither the command nor the intent is duplicated.
#[tokio::test]
async fn no_stream_replay_is_conflict_and_acks_once() {
    todo!()
}

/// Another write has already invalidated the command's expected version, and
/// no matching intent proves an earlier saga commit. Expect a conflict with
/// no partial batch writes. The source event must remain unacknowledged so
/// the failure is not silently skipped.
#[tokio::test]
async fn real_command_conflict_remains_unacked() {
    todo!()
}

/// The port applies the external effect, then crashes before `Done` is stored.
/// Reopening SQLite must redeliver the intent with the same key. Expect two
/// port calls but one applied effect because the port deduplicates by key;
/// the successful retry then stores `Done` and acknowledges the intent.
#[tokio::test]
async fn perform_crash_reopens_and_redelivers_once_effect() {
    todo!()
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
    todo!()
}
