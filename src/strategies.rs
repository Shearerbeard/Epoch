use std::fmt::Debug;

use crate::{
    decider::{Decider, DeciderWithContext, Evolver},
    repository,
};
use async_trait::async_trait;
use repository::event::VersionedEventRepositoryWithStreams;

#[async_trait]
pub trait StateFromEventRepository
where
    <Self::Ev as Evolver>::Evt: Send + Sync + Debug,
    <Self::Ev as Evolver>::State: Send + Sync + Debug + Default,
{
    type Ev: Evolver + Send + Sync + Default;

    async fn load<'a, Err>(
        event_repository: &(impl VersionedEventRepositoryWithStreams<'a, <Self::Ev as Evolver>::Evt, Err>
              + Send
              + Sync),
    ) -> Result<<Self::Ev as Evolver>::State, Err>
    where
        Err: Debug + Send + Sync,
    {
        Ok(event_repository
            .load(None)
            .await?
            .0
            .iter()
            .fold(<Self::Ev as Evolver>::State::default(), Self::Ev::evolve))
    }

    async fn load_by_id<'a, Err, StreamId>(
        event_repository: &(impl VersionedEventRepositoryWithStreams<
            'a,
            <Self::Ev as Evolver>::Evt,
            Err,
            StreamId = StreamId,
        > + Send
              + Sync),
        stream_id: Option<&StreamId>,
    ) -> Result<<Self::Ev as Evolver>::State, Err>
    where
        Err: Debug + Send + Sync,
        StreamId: Send + Sync,
    {
        Ok(event_repository
            .load(stream_id)
            .await?
            .0
            .iter()
            .fold(<Self::Ev as Evolver>::State::default(), Self::Ev::evolve))
    }
}

#[async_trait]
pub trait LoadDecideAppend
where
    <Self::Decide as Evolver>::State: Send + Sync + Debug + Default,
    <Self::Decide as DeciderWithContext>::Ctx: Send + Sync + Debug + Clone,
    <Self::Decide as DeciderWithContext>::Cmd: Send + Sync + Debug + Clone,
    <Self::Decide as Evolver>::Evt: Send + Sync + Debug,
    <Self::Decide as DeciderWithContext>::Err: Send + Sync + Debug,
{
    type Decide: DeciderWithContext + Send + Sync;

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
    ) -> Result<
        Vec<<Self::Decide as Evolver>::Evt>,
        LoadDecideAppendError<<Self::Decide as DeciderWithContext>::Err, RepoErr>,
    >
    where
        RepoErr: Debug + Send + Sync,
        StreamId: Send + Sync,
    {
        let stream = Some(stream_id);
        let (decider_evts, version) = event_repository
            .load(stream)
            .await
            .map_err(LoadDecideAppendError::RepositoryErr)?;

        let state = decider_evts.iter().fold(
            <Self::Decide as Evolver>::init(),
            <Self::Decide as Evolver>::evolve,
        );

        let evts = <Self::Decide as DeciderWithContext>::decide(&ctx, &state, &cmd)
            .map_err(LoadDecideAppendError::DecideErr)?;

        let (evts, _) = event_repository
            .append(&version, stream_id, &evts)
            .await
            .map_err(LoadDecideAppendError::RepositoryErr)?;

        Ok(evts)
    }
}

#[derive(Debug)]
pub enum LoadDecideAppendError<DecideErr: Send + Sync, RepoErr> {
    DecideErr(DecideErr),
    RepositoryErr(RepoErr),
}
