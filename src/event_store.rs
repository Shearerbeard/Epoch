use std::{
    fmt::{Debug, Display},
    str::FromStr,
};

use chrono::{DateTime, Utc};
use eventstore::{
    AppendToStreamOptions, EventData, ExpectedRevision, ReadStreamOptions, ResolvedEvent,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use async_trait::async_trait;
use uuid::Uuid;

use crate::Event;

use super::{
    CommmandInvariantError, EventContext, EventEnvelope, EventStore, FromCommandInvariant,
    PreparedEvent, ToCommandInvariantError,
};

#[derive(Clone)]
pub struct ESDBEventStore {
    is_test: bool,
    client: eventstore::Client,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("ESDB ERROR {0}")]
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
    #[error("Could not convert EventContext::Id to String {0}")]
    ContextIdFromStr(String),
    #[error("Command invariant bounds prevented a committed event {0}")]
    CommandInvariant(String),
}

impl FromCommandInvariant for Error {
    fn from_command_invariant<Ctx>(cmd_err: Ctx::Err) -> Self
    where
        Ctx: EventContext,
        <Ctx as EventContext>::Err: ToCommandInvariantError,
    {
        // Self::CommandInvariant(cmd_err.to_command_invariant_error())
        Self::CommandInvariant(cmd_err.to_string())
    }
}

impl ESDBEventStore {
    pub fn new(client: eventstore::Client) -> ESDBEventStore {
        Self {
            client,
            is_test: false,
        }
    }

    // #[cfg(test)]
    pub fn new_for_test(client: eventstore::Client) -> ESDBEventStore {
        Self {
            client,
            is_test: true,
        }
    }

    pub fn stream_id<Ctx: EventContext>(&self, id: Option<Ctx::Id>) -> String
    where
        <Ctx as EventContext>::Id: Display,
    {
        let prefix = if self.is_test { "test_" } else { "" };

        let ctx = Ctx::event_context();

        match id {
            Some(id_str) => format!("{}/{}", ctx, id_str),
            None => format!("{}{}", prefix, ctx),
        }
    }

    fn resolved_to_event_envelope<Ctx>(
        resolved_event: &ResolvedEvent,
    ) -> Result<(EventEnvelope<Ctx>, ExpectedRevision), Error>
    where
        Ctx: EventContext,
        <Ctx as EventContext>::Id: FromStr,
    {
        let event_data = resolved_event.get_original_event();
        let event = event_data
            .as_json::<<Ctx as EventContext>::Event>()
            .map_err(Error::DeserializeEvent)?;

        let EventMetadata {
            time,
            event_context,
            event_context_id,
        }: EventMetadata =
            serde_json::from_slice(&event_data.custom_metadata).map_err(Error::ParseMetadata)?;

        let event_context_id = match event_context_id {
            Some(str) => {
                Some(Ctx::Id::from_str(&str.clone()).map_err(|_| Error::ContextIdFromStr(str))?)
            }
            None => None,
        };

        Ok((
            EventEnvelope {
                id: event_data.id,
                time,
                event_context,
                event_context_id,
                data: event,
            },
            ExpectedRevision::Exact(event_data.revision),
        ))
    }
}

#[async_trait]
impl EventStore for ESDBEventStore {
    type Error = Error;
    type Position = ExpectedRevision;

    async fn load<Ctx: EventContext>(
        &self,
        id: Option<Ctx::Id>,
    ) -> Result<(Vec<EventEnvelope<Ctx>>, Self::Position), Self::Error>
    where
        <Ctx as EventContext>::Id: Display + FromStr,
    {
        let mut stream = self
            .client
            .read_stream(self.stream_id::<Ctx>(id), &ReadStreamOptions::default())
            .await
            .map_err(Self::Error::ESDBGeneral)?;

        let mut evts: Vec<ResolvedEvent> = vec![];
        loop {
            match stream.next().await {
                Ok(Some(event)) => evts.push(event),
                Ok(None) => break,
                Err(eventstore::Error::ResourceNotFound) => {
                    return Ok((vec![], ExpectedRevision::NoStream))
                }
                Err(e) => return Err(Self::Error::ReadStream(e)),
            }
        }

        let mut rv = vec![];
        let mut pos = ExpectedRevision::StreamExists;

        for ev in evts {
            let (ee, revision) = ESDBEventStore::resolved_to_event_envelope(&ev)?;

            rv.push(ee);
            pos = revision;
        }

        Ok((rv, pos))
    }

    async fn append<Ctx>(
        &self,
        position: Self::Position,
        event: PreparedEvent<Ctx>,
        id: Option<<Ctx as EventContext>::Id>,
    ) -> Result<(EventEnvelope<Ctx>, Self::Position), Self::Error>
    where
        Ctx: EventContext + Send + Sync,
        <Ctx as EventContext>::Id: Clone + Display + ToString,
        <Ctx as EventContext>::Event: Serialize + Event,
    {
        let event_meta = EventMetadata {
            time: Utc::now(),
            event_context: event.event_context,
            event_context_id: id.clone().map(|x| x.to_string()),
        };

        let event_id = Uuid::new_v4();
        let event_data = EventData::json(event.data.event_type(), &event.data.clone())
            .map_err(Error::SerializeEventDataPayload)?
            .id(event_id.clone())
            .metadata_as_json(event_meta.clone())
            .map_err(Error::SerializeEventDataPayload)?;

        let options = AppendToStreamOptions::default().expected_revision(position);

        let stream_id = self.stream_id::<Ctx>(id.clone());

        let res = self
            .client
            .append_to_stream(stream_id.clone(), &options, event_data.clone())
            .await
            .map_err(|e| Error::WriteStream(stream_id, e))?;

        let EventMetadata {
            time,
            event_context,
            ..
        } = event_meta;

        Ok((
            EventEnvelope {
                id: event_id,
                time,
                event_context,
                event_context_id: id,
                data: event.data,
            },
            ExpectedRevision::Exact(res.next_expected_version),
        ))
    }
}

#[derive(Deserialize, Serialize, Clone)]
pub struct EventMetadata {
    time: DateTime<Utc>,
    event_context: String,
    event_context_id: Option<String>,
}
