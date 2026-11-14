#![cfg(feature = "sqlite")]

mod support;

#[tokio::test]
async fn runner_commits_command_and_intent_atomically() {
    todo!()
}

#[tokio::test]
async fn any_replay_is_typed_duplicate_and_acks_once() {
    todo!()
}

#[tokio::test]
async fn no_stream_replay_is_conflict_and_acks_once() {
    todo!()
}

#[tokio::test]
async fn real_command_conflict_remains_unacked() {
    todo!()
}

#[tokio::test]
async fn perform_crash_reopens_and_redelivers_once_effect() {
    todo!()
}

#[tokio::test]
async fn durable_failures_park_and_hook_replay_dedupes() {
    todo!()
}
