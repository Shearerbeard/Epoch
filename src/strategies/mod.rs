use core::time;
use std::{fmt::Debug, thread};

use crate::{
    decider::{DeciderWithContext, Evolver},
    repository::{
        self, event::VersionedRepositoryError, state::VersionedStateRepository, RepositoryVersion,
    },
};
use async_trait::async_trait;
use repository::event::{StreamIdFromEvent, VersionedEventRepositoryWithStreams};

#[async_trait]
pub trait StateFromEventRepository
where
    <Self::Ev as Evolver>::Evt: Send + Sync + Debug,
    <Self::Ev as Evolver>::State: Send + Sync + Debug,
{
    type Ev: Evolver + Send + Sync;

    async fn load<'a, Err>(
        event_repository: &(impl VersionedEventRepositoryWithStreams<'a, <Self::Ev as Evolver>::Evt, Err>
              + Send
              + Sync),
    ) -> Result<<Self::Ev as Evolver>::State, VersionedRepositoryError<Err>>
    where
        Err: Debug + Send + Sync,
    {
        Ok(event_repository
            .load(None)
            .await?
            .0
            .iter()
            .fold(<Self::Ev as Evolver>::init(), Self::Ev::evolve))
    }

    async fn load_by_id<'a, Err, StreamId>(
        event_repository: &(impl VersionedEventRepositoryWithStreams<
            'a,
            <Self::Ev as Evolver>::Evt,
            Err,
            StreamId = StreamId,
        > + Send
              + Sync),
        stream_id: &StreamId,
    ) -> Result<<Self::Ev as Evolver>::State, VersionedRepositoryError<Err>>
    where
        Err: Debug + Send + Sync,
        StreamId: Send + Sync,
    {
        Ok(event_repository
            .load(Some(stream_id))
            .await?
            .0
            .iter()
            .fold(<Self::Ev as Evolver>::init(), Self::Ev::evolve))
    }
}

#[async_trait]
pub trait LoadDecideAppend
where
    <Self::Decide as Evolver>::State: Send + Sync + Debug,
    <Self::Decide as DeciderWithContext>::Ctx: Send + Sync + Debug,
    <Self::Decide as DeciderWithContext>::Cmd: Send + Sync + Debug,
    <Self::Decide as Evolver>::Evt: Clone + Send + Sync + Debug,
    <Self::Decide as DeciderWithContext>::Err: Send + Sync + Debug,
{
    type Decide: DeciderWithContext + Send + Sync;

    fn to_lda_error<DecErr: Send + Sync, RepoErr: Send + Sync>(
        err: VersionedRepositoryError<RepoErr>,
    ) -> LoadDecideAppendError<DecErr, RepoErr> {
        match err {
            VersionedRepositoryError::VersionConflict(_) => LoadDecideAppendError::VersionError,
            VersionedRepositoryError::RepoErr(e) => LoadDecideAppendError::RepositoryErr(e),
        }
    }

    async fn execute<'a, RepoErr, StreamId>(
        event_repository: &mut (impl VersionedEventRepositoryWithStreams<
            'a,
            <Self::Decide as Evolver>::Evt,
            RepoErr,
            StreamId = StreamId,
        > + Send
                  + Sync),
        stream_id: &StreamState<StreamId>,
        ctx: &<<Self as LoadDecideAppend>::Decide as DeciderWithContext>::Ctx,
        cmd: &<<Self as LoadDecideAppend>::Decide as DeciderWithContext>::Cmd,
        retrys: Option<u32>,
    ) -> Result<
        Vec<<Self::Decide as Evolver>::Evt>,
        LoadDecideAppendError<<Self::Decide as DeciderWithContext>::Err, RepoErr>,
    >
    where
        RepoErr: Debug + Send + Sync,
        StreamId: Send
            + Sync
            + Clone
            + StreamIdFromEvent<<<Self as LoadDecideAppend>::Decide as Evolver>::Evt>,
    {
        let (mut decider_evts, mut version) = match stream_id {
            StreamState::New => (vec![], RepositoryVersion::NoStream),
            StreamState::Existing(sid) => event_repository
                .load(Some(sid))
                .await
                .map_err(Self::to_lda_error)?,
        };

        let mut state = <Self::Decide as Evolver>::init();

        for r in 1..retrys.unwrap_or(20) {
            state = decider_evts
                .iter()
                .fold(state, <Self::Decide as Evolver>::evolve);

            let new_evts = <Self::Decide as DeciderWithContext>::decide(ctx, &state, cmd)
                .map_err(LoadDecideAppendError::DecideErr)?;

            let stream = match stream_id {
                StreamState::New => match new_evts.first() {
                    None => {
                        return Ok(vec![]);
                    }
                    Some(evt) => StreamId::from(evt.clone()),
                },
                StreamState::Existing(sid) => sid.clone(),
            };

            match event_repository.append(&version, &stream, &new_evts).await {
                Ok((appended_evts, _)) => return Ok(appended_evts),
                Err(VersionedRepositoryError::RepoErr(e)) => {
                    println!("Max Retries for {:?}!!", &cmd);
                    return Err(LoadDecideAppendError::RepositoryErr(e));
                }
                Err(VersionedRepositoryError::VersionConflict(_)) => {
                    println!("RETRY #{} for {:?}!!", &r, &cmd);
                    thread::sleep(time::Duration::new(0, 100000000 * r));
                    let (mut catchup_evts, new_version) = event_repository
                        .load_from_version(&version, Some(&stream))
                        .await
                        .map_err(Self::to_lda_error)?;

                    version = new_version;
                    decider_evts.append(&mut catchup_evts);
                }
            };
        }

        Err(LoadDecideAppendError::OccMaxRetries)
    }
}

#[async_trait]
pub trait ReifyDecideSave
where
    <<Self as ReifyDecideSave>::Decide as DeciderWithContext>::Ctx: Send + Sync,
    <<Self as ReifyDecideSave>::Decide as DeciderWithContext>::Cmd: Send + Sync + Debug,
    <<Self as ReifyDecideSave>::Decide as DeciderWithContext>::Err: Send + Sync,
    <<Self as ReifyDecideSave>::Decide as Evolver>::Evt: Send + Sync,
    <<Self as ReifyDecideSave>::Decide as Evolver>::State: Send + Sync + Clone,
{
    type Decide: DeciderWithContext + Send + Sync;

    async fn execute_reify_decide<'a, RepoErr>(
        state_repository: &mut (impl VersionedStateRepository<'a, <Self::Decide as Evolver>::State, RepoErr>
                  + Send
                  + Sync),
        ctx: &<<Self as ReifyDecideSave>::Decide as DeciderWithContext>::Ctx,
        cmd: &<<Self as ReifyDecideSave>::Decide as DeciderWithContext>::Cmd,
        retrys: Option<u32>,
    ) -> Result<
        <Self::Decide as Evolver>::State,
        ReifyDecideSaveError<<Self::Decide as DeciderWithContext>::Err, RepoErr>,
    >
    where
        RepoErr: Send + Sync,
    {
        let (mut state, mut version) = state_repository
            .reify()
            .await
            .map_err(ReifyDecideSaveError::RepositoryErr)?;

        for r in 1..retrys.unwrap_or(20) {
            let local_state = state.clone();
            let evts = <Self::Decide as DeciderWithContext>::decide(ctx, &local_state, cmd)
                .map_err(ReifyDecideSaveError::DecideErr)?;

            let new_state = evts
                .iter()
                .fold(local_state, <Self::Decide as Evolver>::evolve);

            match state_repository.save(&version, &new_state).await {
                Ok(s) => return Ok(s),
                Err(VersionedRepositoryError::RepoErr(e)) => {
                    return Err(ReifyDecideSaveError::RepositoryErr(e))
                }
                Err(VersionedRepositoryError::VersionConflict(_)) => {
                    println!("Retry #{} for {:?} - Reload State", &r, &cmd);
                    (state, version) = state_repository
                        .reify()
                        .await
                        .map_err(ReifyDecideSaveError::RepositoryErr)?;
                }
            }
        }

        Err(ReifyDecideSaveError::OccMaxRetries)
    }
}

#[derive(Debug)]
pub struct CommandResponse<D: DeciderWithContext + Debug>(
    <D as DeciderWithContext>::Cmd,
    Vec<<D as Evolver>::Evt>,
    <D as Evolver>::State,
);

#[async_trait]
pub trait DecideEvolveWithCommandResponse
where
    <Self::Decide as Evolver>::State: Send + Sync + Debug + Clone,
    <Self::Decide as DeciderWithContext>::Ctx: Send + Sync + Debug,
    <Self::Decide as DeciderWithContext>::Cmd: Send + Sync + Debug,
    <Self::Decide as Evolver>::Evt: Clone + Send + Sync + Debug,
    <Self::Decide as DeciderWithContext>::Err: Send + Sync + Debug,
{
    type Decide: DeciderWithContext + Send + Sync;

    async fn response(
        cmd: <<Self as DecideEvolveWithCommandResponse>::Decide as DeciderWithContext>::Cmd,
        state: &<<Self as DecideEvolveWithCommandResponse>::Decide as Evolver>::State,
        ctx: &<<Self as DecideEvolveWithCommandResponse>::Decide as DeciderWithContext>::Ctx,
    ) -> Result<
        CommandResponse<<Self as DecideEvolveWithCommandResponse>::Decide>,
        <<Self as DecideEvolveWithCommandResponse>::Decide as DeciderWithContext>::Err,
    > {
        let evts = <Self::Decide as DeciderWithContext>::decide(ctx, state, &cmd)?;
        let state = evts
            .iter()
            .fold(state.to_owned(), <Self::Decide as Evolver>::evolve);

        Ok(CommandResponse(cmd, evts, state))
    }
}

pub enum StreamState<T> {
    New,
    Existing(T),
}

#[derive(Debug)]
pub enum LoadDecideAppendError<DecideErr: Send + Sync, RepoErr> {
    OccMaxRetries,
    VersionError,
    DecideErr(DecideErr),
    RepositoryErr(RepoErr),
}

#[derive(Debug)]
pub enum ReifyDecideSaveError<DecideErr: Send + Sync, RepoErr> {
    OccMaxRetries,
    DecideErr(DecideErr),
    RepositoryErr(RepoErr),
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use assert_matches::assert_matches;

    use crate::{
        decider::Event,
        repository::in_memory::{versioned_with_streams::InMemoryEventRepository, state::versioned::InMemoryStateRepository},
        test_helpers::{
            deciders::user::{
                User, UserCommand, UserDecider, UserDeciderCtx, UserDeciderError, UserDeciderState,
                UserEvent, UserFieldError, UserName,
            },
            ValueType,
        },
    };

    use super::*;

    #[actix_rt::test]
    async fn load_decide_append_basic_function() {
        let ctx = UserDeciderCtx::new();

        let mut event_repository = InMemoryEventRepository::<UserEvent>::new("test");

        let cmd1 = UserCommand::AddUser("Mike".to_string());

        let evts =
            UserDecider::execute(&mut event_repository, &StreamState::New, &ctx, &cmd1, None)
                .await
                .expect("command_succeeds");

        let first_id = evts.first().unwrap().get_id();

        assert_matches!(
            evts.first().expect("one event"),
            UserEvent::UserAdded(User { id, name, .. }) if (&first_id == id) && (name.value() == "Mike".to_string())
        );

        let state = UserDeciderState::load_by_id(&event_repository, &first_id.to_string())
            .await
            .expect("state is loaded");

        assert_matches!(
            state,
            UserDeciderState { users } if users == HashMap::from([(first_id.clone(),  User::new(first_id, UserName::try_from("Mike".to_string()).unwrap()))])
        );

        let cmd2 = UserCommand::AddUser("Dmitiry".to_string());
        let evts =
            UserDecider::execute(&mut event_repository, &StreamState::New, &ctx, &cmd2, None)
                .await
                .expect("command_succeeds");

        let second_id = evts.first().unwrap().get_id();

        assert_matches!(
            evts.first().expect("one event"),
            UserEvent::UserAdded(User { id, name, .. }) if (&second_id == id) && (name.value() == "Dmitiry".to_string())
        );

        let state = UserDeciderState::load_by_id(&event_repository, &second_id.to_string())
            .await
            .expect("state is loaded");

        assert_matches!(
            state,
            UserDeciderState { users } if users == HashMap::from([(second_id.clone(),  User::new(second_id, UserName::try_from("Dmitiry".to_string()).unwrap()))])
        );

        let cmd3 = UserCommand::UpdateUserName(second_id.clone(), "Dmitiry2".to_string());
        let evts = UserDecider::execute(
            &mut event_repository,
            &StreamState::Existing(second_id.to_string()),
            &ctx,
            &cmd3,
            None,
        )
        .await
        .expect("command_succeeds");

        assert_matches!(
            evts.first().expect("one event"),
            UserEvent::UserNameUpdated(id, name) if (id == &second_id) && (name == &UserName::try_from("Dmitiry2".to_string()).unwrap())
        );

        let state = UserDeciderState::load_by_id(&event_repository, &second_id.to_string())
            .await
            .expect("state is loaded");

        assert_matches!(
            state,
            UserDeciderState { users } if users == HashMap::from([(second_id.clone(),  User::new(second_id, UserName::try_from("Dmitiry2".to_string()).unwrap()))])
        );

        let cmd4 =
            UserCommand::UpdateUserName(second_id.clone(), "DmitiryWayToLongToSucceed".to_string());

        let res = UserDecider::execute(
            &mut event_repository,
            &StreamState::Existing(second_id.to_string()),
            &ctx,
            &cmd4,
            None,
        )
        .await;

        assert_matches!(
            res,
            Err(LoadDecideAppendError::DecideErr(UserDeciderError::UserField(UserFieldError::NameToLong(n)))) if n == "DmitiryWayToLongToSucceed".to_string()
        );

        let state = UserDeciderState::load_by_id(&event_repository, &second_id.to_string())
            .await
            .expect("state is loaded");

        assert_matches!(
            state,
            UserDeciderState { users } if users == HashMap::from([(second_id.clone(),  User::new(second_id, UserName::try_from("Dmitiry2".to_string()).unwrap()))])
        );

        let state = UserDeciderState::load(&event_repository)
            .await
            .expect("state is loaded");

        assert_matches!(
            state,
            UserDeciderState { users } if users == HashMap::from([
                (first_id.clone(), User::new(first_id, UserName::try_from("Mike".to_string()).unwrap())),
                (second_id.clone(),  User::new(second_id, UserName::try_from("Dmitiry2".to_string()).unwrap()))
                ])
        );
    }

    #[actix_rt::test]
    async fn decide_evolve_with_command_response() {
        let ctx = UserDeciderCtx::new();
        let state = UserDeciderState::default();

        let cmd1 = UserCommand::AddUser("Mike".to_string());
        let res = UserDecider::response(cmd1, &state, &ctx).await;

        assert_matches!(res, Ok(CommandResponse(UserCommand::AddUser(_), _, _)));
    }

    #[actix_rt::test]
    async fn reify_decide_save_basic_functionality() {
        let ctx = UserDeciderCtx::new();

        let mut state_repository = InMemoryStateRepository::<UserDeciderState>::new(UserDeciderState::default());

        let cmd1 = UserCommand::AddUser("Mike".to_string());

        let res = UserDecider::execute_reify_decide(&mut state_repository, &ctx, &cmd1, None).await.unwrap();

        assert_eq!(
            res.users.len(),
            1
        );
    }
}
