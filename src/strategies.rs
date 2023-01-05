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
    <Self::Ev as Evolver>::State: Send + Sync + Debug + Default,
{
    type Ev: Evolver + Send + Sync + Default;

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
    ) -> Result<<Self::Ev as Evolver>::State, VersionedRepositoryError<Err>>
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
                    let (mut catchup_evts, new_version) = event_repository
                        .load_from_version(&version, stream)
                        .await
                        .map_err(Self::to_lda_error)?;

                    version = new_version;
                    decider_evts.append(&mut catchup_evts);
                }
            };
        }

        Ok(vec![])
    }
}

#[derive(Debug)]
pub enum LoadDecideAppendError<DecideErr: Send + Sync, RepoErr> {
    VersionError,
    DecideErr(DecideErr),
    RepositoryErr(RepoErr),
}
