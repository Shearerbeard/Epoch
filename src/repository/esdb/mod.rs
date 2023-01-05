use std::{fmt::Debug, marker::PhantomData};

use async_trait::async_trait;
use eventstore::{
    AppendToStreamOptions, Client, CurrentRevision, EventData, ExpectedRevision, ReadStreamOptions,
    ResolvedEvent, StreamPosition,
};
use serde::{de::DeserializeOwned, Serialize};
use uuid_0_8_2::Uuid;

use crate::decider::Event;

use self::error::Error;

use super::{
    event::{VersionDiff, VersionedEventRepositoryWithStreams, VersionedRepositoryError},
    RepositoryVersion,
};

pub mod error;

#[derive(Clone)]
pub struct ESDBEventRepository<E> {
    client: Client,
    stream_name: String,
    _hidden: PhantomData<E>,
}

impl<'a, E> ESDBEventRepository<E> {
    pub fn new(client: &Client, stream_name: &str) -> Self {
        Self {
            client: client.to_owned(),
            stream_name: stream_name.to_owned(),
            _hidden: PhantomData::default(),
        }
    }

    fn get_stream(&self, stream_id: Option<&String>) -> String {
        if let Some(id) = stream_id {
            format!("{}-{}", self.stream_name, id)
        } else {
            format!("$ce-{}", self.stream_name)
        }
    }

    fn version_to_esdb_position(version: &RepositoryVersion) -> StreamPosition<u64> {
        if let RepositoryVersion::Exact(u) = version {
            StreamPosition::Position(u.to_owned().try_into().unwrap())
        } else {
            StreamPosition::Start
        }
    }

    fn version_to_expected_revision(version: &RepositoryVersion) -> ExpectedRevision {
        match version {
            RepositoryVersion::Exact(u) => {
                ExpectedRevision::Exact(u.to_owned().try_into().unwrap())
            }
            _ => ExpectedRevision::Any,
        }
    }

    fn current_revision_to_version(revision: &CurrentRevision) -> RepositoryVersion {
        match revision {
            CurrentRevision::Current(val) => RepositoryVersion::Exact(*val as usize),
            CurrentRevision::NoStream => RepositoryVersion::NoStream,
        }
    }
}

#[async_trait]
impl<'a, E> VersionedEventRepositoryWithStreams<'a, E, Error> for ESDBEventRepository<E>
where
    E: Event + Sync + Send + Serialize + DeserializeOwned + Clone + Debug,
{
    type StreamId = String;

    async fn load(
        &self,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, RepositoryVersion), VersionedRepositoryError<Error>> {
        self.load_from_version(&RepositoryVersion::Any, id).await
    }

    async fn load_from_version(
        &self,
        version: &RepositoryVersion,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, RepositoryVersion), VersionedRepositoryError<Error>> {
        let mut stream = self
            .client
            .read_stream(
                self.get_stream(id),
                &ReadStreamOptions::default()
                    .resolve_link_tos()
                    .position(Self::version_to_esdb_position(&version)),
            )
            .await
            .map_err(Error::ESDBGeneral)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let mut evts: Vec<ResolvedEvent> = vec![];

        loop {
            match stream.next().await {
                Ok(Some(event)) => evts.push(event),
                Ok(None) => break,
                Err(eventstore::Error::ResourceNotFound) => {
                    return Ok((vec![], RepositoryVersion::NoStream))
                }
                Err(e) => return Err(VersionedRepositoryError::RepoErr(Error::ReadStream(e))),
            }
        }

        let mut rv = vec![];
        let mut pos = RepositoryVersion::StreamExists;

        for ev in evts {
            pos = RepositoryVersion::Exact(ev.get_original_event().revision.try_into().unwrap());

            if let Some(event_data) = ev.event {
                // Continue on deser failure - occasionally you'll get delete and other system types in the stream
                if let Ok(event) = event_data.as_json::<E>().map_err(Error::DeserializeEvent) {
                    rv.push(event);
                }
            }
        }

        Ok((rv, pos))
    }

    async fn append(
        &mut self,
        version: &RepositoryVersion,
        stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> Result<(Vec<E>, RepositoryVersion), VersionedRepositoryError<Error>>
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        let mut perpared_events = vec![];

        for e in events {
            let ed = EventData::json(e.event_type(), e)
                .map(|ed| ed.id(Uuid::new_v4()))
                .map_err(Error::SerializeEventDataPayload)
                .map_err(VersionedRepositoryError::RepoErr)?;

            perpared_events.push(ed);
        }

        let res = self
            .client
            .append_to_stream(
                self.get_stream(Some(stream)),
                &AppendToStreamOptions::default()
                    .expected_revision(Self::version_to_expected_revision(version)),
                perpared_events,
            )
            .await
            .map_err(|e| {
                if let eventstore::Error::WrongExpectedVersion { current, .. } = e {
                    VersionedRepositoryError::VersionConflict(VersionDiff::new(
                        *version,
                        Self::current_revision_to_version(&current),
                    ))
                } else {
                    VersionedRepositoryError::RepoErr(Error::WriteStream(stream.to_owned(), e))
                }
            })?;

        Ok((
            events.to_owned(),
            RepositoryVersion::Exact(res.next_expected_version.try_into().unwrap()),
        ))
    }
}

// impl Into<VersionedRepositoryError<Error>> for Error {
//     fn into(self) -> VersionedRepositoryError<Error> {
//         todo!()
//     }
// }

#[cfg(test)]
mod tests {
    use eventstore::DeleteStreamOptions;

    use crate::test_helpers::{
        deciders::user::UserEvent, repository::test_versioned_event_repository_with_streams,
    };

    use super::*;

    const BASE_STREAM: &str = "test";

    async fn store_from_environment(ids: Vec<usize>) -> eventstore::Client {
        let _ = dotenv::dotenv().expect("File .env or Env Vars not found");
        let settings = dotenv::var("ESDB_CONNECTION_STRING")
            .expect("ESDB to be set in env")
            .parse()
            .expect("ESDB connection string to parse");

        let client = eventstore::Client::new(settings).expect("Eventstore client");

        for id in ids {
            let _ = client
                .delete_stream(
                    format!("{}-{}", BASE_STREAM, id),
                    &DeleteStreamOptions::default(),
                )
                .await;
        }

        client
    }

    #[actix_rt::test]
    async fn repository_spec_test() {
        let client = store_from_environment(vec![1, 2]).await;

        let event_repository = ESDBEventRepository::<UserEvent>::new(&client, BASE_STREAM);
        let _ = test_versioned_event_repository_with_streams(event_repository).await;
    }
}
