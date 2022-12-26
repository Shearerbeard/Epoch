use async_trait::async_trait;
use eventstore::{
    AppendToStreamOptions, Client, EventData, ExpectedRevision, ReadStreamOptions, ResolvedEvent,
    StreamPosition,
};
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::decider::{Command, Event};

use super::LockingEventStoreWithStreams;

struct ESDBEventRepository {
    client: Client,
    stream_name: String,
}

impl<'a> ESDBEventRepository {
    fn get_stream(&self, stream_id: Option<String>) -> String {
        if let Some(id) = stream_id {
            format!("{}/{}", self.stream_name, id)
        } else {
            self.stream_name.clone()
        }
    }

    fn expected_revision_to_position(er: &ExpectedRevision) -> StreamPosition<u64> {
        if let ExpectedRevision::Exact(u) = er {
            StreamPosition::Position(u.to_owned())
        } else {
            StreamPosition::Start
        }
    }
}

#[async_trait]
impl<'a, C, E> LockingEventStoreWithStreams<'a, C, E, ESEBEventRepositoryError>
    for ESDBEventRepository
where
    C: Command,
    E: Event + Sync + Send + Serialize + DeserializeOwned + Clone,
{
    type StreamId = String;
    type Version = ExpectedRevision;

    async fn load(
        &self,
        id: Option<Self::StreamId>,
    ) -> Result<(Vec<E>, Self::Version), ESEBEventRepositoryError> {
        let mut stream = self
            .client
            .read_stream(self.get_stream(id), &ReadStreamOptions::default())
            .await
            .map_err(ESEBEventRepositoryError::ESDBGeneral)?;

        let mut evts: Vec<ResolvedEvent> = vec![];

        loop {
            match stream.next().await {
                Ok(Some(event)) => evts.push(event),
                Ok(None) => break,
                Err(eventstore::Error::ResourceNotFound) => {
                    return Ok((vec![], ExpectedRevision::NoStream))
                }
                Err(e) => return Err(ESEBEventRepositoryError::ReadStream(e)),
            }
        }

        let mut rv = vec![];
        let mut pos = ExpectedRevision::StreamExists;

        for ev in evts {
            let event_data = ev.get_original_event();
            let event = event_data
                .as_json::<E>()
                .map_err(ESEBEventRepositoryError::DeserializeEvent)?;

            pos = ExpectedRevision::Exact(event_data.revision);
            rv.push(event)
        }

        Ok((rv, pos))
    }

    async fn load_from_version(
        &self,
        version: Self::Version,
        id: Option<Self::StreamId>,
    ) -> Result<(Vec<E>, Self::Version), ESEBEventRepositoryError> {
        let options =
            ReadStreamOptions::default().position(Self::expected_revision_to_position(&version));

        let mut stream = self
            .client
            .read_stream(self.get_stream(id), &options)
            .await
            .map_err(ESEBEventRepositoryError::ESDBGeneral)?;

        let mut evts: Vec<ResolvedEvent> = vec![];

        loop {
            match stream.next().await {
                Ok(Some(event)) => evts.push(event),
                Ok(None) => break,
                Err(eventstore::Error::ResourceNotFound) => {
                    return Ok((vec![], ExpectedRevision::NoStream))
                }
                Err(e) => return Err(ESEBEventRepositoryError::ReadStream(e)),
            }
        }

        let mut rv = vec![];
        let mut pos = ExpectedRevision::StreamExists;

        for ev in evts {
            let event_data = ev.get_original_event();
            let event = event_data
                .as_json::<E>()
                .map_err(ESEBEventRepositoryError::DeserializeEvent)?;

            pos = ExpectedRevision::Exact(event_data.revision);
            rv.push(event)
        }

        Ok((rv, pos))
    }

    async fn append(
        &mut self,
        version: Self::Version,
        stream: Self::StreamId,
        events: Vec<E>,
    ) -> Result<(Vec<E>, Self::Version), ESEBEventRepositoryError>
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        let mut perpared_events = vec![];

        for e in &events {
            let ed = EventData::json(e.event_type(), e.clone())
                .map(|ed| ed.id(Uuid::new_v4()))
                .map_err(ESEBEventRepositoryError::SerializeEventDataPayload)?;

            perpared_events.push(ed);
        }

        let res = self
            .client
            .append_to_stream(
                self.get_stream(Some(stream.to_owned())),
                &AppendToStreamOptions::default().expected_revision(version),
                perpared_events,
            )
            .await
            .map_err(|e| ESEBEventRepositoryError::WriteStream(stream, e))?;

        Ok((events, ExpectedRevision::Exact(res.next_expected_version)))
    }
}

#[derive(Debug, Error)]
enum ESEBEventRepositoryError {
    #[error("ESDB Error {0}")]
    ESDBGeneral(eventstore::Error),
    #[error("Error reading stream: {0}")]
    ReadStream(eventstore::Error),
    #[error("Could not deserialize event {0}")]
    DeserializeEvent(serde_json::Error),
    #[error("Could not parse event meta {0}")]
    ParseMetadata(serde_json::Error),
    #[error("Could not serialize event {0}")]
    SerializeEventDataPayload(serde_json::Error),
    #[error("Could not write to stream {0}: {1}")]
    WriteStream(String, eventstore::Error),
}
