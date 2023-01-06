use std::fmt::Debug;

use crate::{
    decider::{DeciderWithContext, Evolver},
    repository::{self, event::VersionedRepositoryError},
};
use async_trait::async_trait;
use repository::event::VersionedEventRepositoryWithStreams;

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
    <Self::Decide as Evolver>::Evt: Send + Sync + Debug,
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
        stream_id: &StreamId,
        ctx: &<<Self as LoadDecideAppend>::Decide as DeciderWithContext>::Ctx,
        cmd: &<<Self as LoadDecideAppend>::Decide as DeciderWithContext>::Cmd,
        retrys: Option<u32>,
    ) -> Result<
        Vec<<Self::Decide as Evolver>::Evt>,
        LoadDecideAppendError<<Self::Decide as DeciderWithContext>::Err, RepoErr>,
    >
    where
        RepoErr: Debug + Send + Sync,
        StreamId: Send + Sync,
    {
        let stream = Some(stream_id);

        let (mut decider_evts, mut version) = event_repository
            .load(stream)
            .await
            .map_err(Self::to_lda_error)?;

        let mut state = <Self::Decide as Evolver>::init();

        for _ in 1..retrys.unwrap_or(4) {
            state = decider_evts
                .iter()
                .fold(state, <Self::Decide as Evolver>::evolve);

            let new_evts = <Self::Decide as DeciderWithContext>::decide(&ctx, &state, &cmd)
                .map_err(LoadDecideAppendError::DecideErr)?;

            match event_repository
                .append(&version, stream_id, &new_evts)
                .await
            {
                Ok((appended_evts, _)) => return Ok(appended_evts),
                Err(VersionedRepositoryError::RepoErr(e)) => {
                    return Err(LoadDecideAppendError::RepositoryErr(e));
                }
                Err(VersionedRepositoryError::VersionConflict(_)) => {
                    println!("RETRY IT!!");
                    let (mut catchup_evts, new_version) = event_repository
                        .load_from_version(&version, stream)
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

#[derive(Debug)]
pub enum LoadDecideAppendError<DecideErr: Send + Sync, RepoErr> {
    OccMaxRetries,
    VersionError,
    DecideErr(DecideErr),
    RepositoryErr(RepoErr),
}

#[cfg(test)]
mod tests {
    use std::{thread, cell::Cell};

    use crate::{
        repository::in_memory::versioned_with_streams::InMemoryEventRepository,
        test_helpers::deciders::user::{
            IdGen, UserCommand, UserDecider, UserDeciderCtx, UserDeciderState, UserEvent,
        },
    };

    use super::*;

    #[actix_rt::test]
    async fn test_occ() {
        let ctx = UserDeciderCtx::new();

        let mut event_repository = InMemoryEventRepository::<UserEvent>::new("test");

        let cmd1 = UserCommand::AddUser("Mike".to_string());

        let evts =
            UserDecider::execute(&mut event_repository, &1.to_string(), &ctx, &cmd1, None).await;

        println!("New Events: {:?}", evts);
        println!(
            "State Stream -1: {:?}",
            UserDeciderState::load_by_id(&event_repository, &1.to_string()).await
        );

        let cmd2 = UserCommand::AddUser("Dmitiry".to_string());
        let evts =
            UserDecider::execute(&mut event_repository, &2.to_string(), &ctx, &cmd2, None).await;

        println!("New Events: {:?}", evts);
        println!(
            "State Stream -2: {:?}",
            UserDeciderState::load_by_id(&event_repository, &2.to_string()).await
        );

        println!(
            "State All: {:?}",
            UserDeciderState::load(&event_repository).await
        );



        let cmd3 = UserCommand::UpdateUserName(1, "MikeUpdated1".to_string());
        let cmd4 = UserCommand::UpdateUserName(1, "MikeUpdated4".to_string());
        let cmd5 = UserCommand::UpdateUserName(1, "MikeUpdated5".to_string());
        let cmd6 = UserCommand::UpdateUserName(1, "MikeUpdated6".to_string());
        let cmd7 = UserCommand::UpdateUserName(1, "MikeUpdated7".to_string());
        let cmd8 = UserCommand::UpdateUserName(1, "MikeUpdated8".to_string());
        let cmd9 = UserCommand::UpdateUserName(1, "MikeUpdated9".to_string());
        let cmd10 = UserCommand::UpdateUserName(1, "MikeUpdated10".to_string());

        let mut event_repository2 = event_repository.clone();
        let mut ctx2 = ctx.clone();

        let handle = actix_rt::spawn(async move {
            let mut thread_repo = event_repository.clone();
            let mut thread_ctx = ctx.clone();

            let id = 2.to_string();

            for cmd in vec![cmd3, cmd4, cmd5, cmd6] {
                println!("Calling in Spawn Thread: {:?}", cmd);
                let _ = UserDecider::execute(&mut thread_repo, &id, &thread_ctx, &cmd, None).await;
            }
        });

        let id = 2.to_string();
        for cmd in vec![cmd7, cmd8, cmd9, cmd10] {
            println!("Calling in Main Thread: {:?}", cmd);
            let _ = UserDecider::execute(&mut event_repository2, &id, &ctx2, &cmd, None).await;
        }

        // let paralell_cmds = (
        //     UserDecider::execute(&mut event_repository, &2.to_string(), &ctx, &cmd3, None),
        //     UserDecider::execute(&mut event_repository, &2.to_string(), &ctx, &cmd4, None),
        //     UserDecider::execute(&mut event_repository, &2.to_string(), &ctx, &cmd5, None),
        //     UserDecider::execute(&mut event_repository, &2.to_string(), &ctx, &cmd6, None),
        //     UserDecider::execute(&mut event_repository, &2.to_string(), &ctx, &cmd7, None),
        //     UserDecider::execute(&mut event_repository, &2.to_string(), &ctx, &cmd8, None),
        //     UserDecider::execute(&mut event_repository, &2.to_string(), &ctx, &cmd9, None),
        //     UserDecider::execute(&mut event_repository, &2.to_string(), &ctx, &cmd10, None),
        // );
    }
}
