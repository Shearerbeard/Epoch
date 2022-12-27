use std::marker::PhantomData;

use async_trait::async_trait;
use eventstore::{
    AppendToStreamOptions, Client, EventData, ExpectedRevision, ReadStreamOptions, ResolvedEvent,
    StreamPosition,
};
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;

use crate::decider::{Command, Event};

use self::error::Error;

use super::LockingEventStoreWithStreams;

pub mod error;

struct ESDBEventRepository<E> {
    client: Client,
    stream_name: String,
    _hidden: PhantomData<E>,
}

impl<'a, E> ESDBEventRepository<E> {
    fn new(client: &Client, stream_name: &str) -> Self {
        Self {
            client: client.to_owned(),
            stream_name: stream_name.to_owned(),
            _hidden: PhantomData::default(),
        }
    }

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
impl<'a, E> LockingEventStoreWithStreams<'a, E, Error> for ESDBEventRepository<E>
where
    E: Event + Sync + Send + Serialize + DeserializeOwned + Clone,
{
    type StreamId = String;
    type Version = ExpectedRevision;

    async fn load(&self, id: Option<Self::StreamId>) -> Result<(Vec<E>, Self::Version), Error> {
        let mut stream = self
            .client
            .read_stream(self.get_stream(id), &ReadStreamOptions::default())
            .await
            .map_err(Error::ESDBGeneral)?;

        let mut evts: Vec<ResolvedEvent> = vec![];

        loop {
            match stream.next().await {
                Ok(Some(event)) => evts.push(event),
                Ok(None) => break,
                Err(eventstore::Error::ResourceNotFound) => {
                    return Ok((vec![], ExpectedRevision::NoStream))
                }
                Err(e) => return Err(Error::ReadStream(e)),
            }
        }

        let mut rv = vec![];
        let mut pos = ExpectedRevision::StreamExists;

        for ev in evts {
            let event_data = ev.get_original_event();
            let event = event_data.as_json::<E>().map_err(Error::DeserializeEvent)?;

            pos = ExpectedRevision::Exact(event_data.revision);
            rv.push(event)
        }

        Ok((rv, pos))
    }

    async fn load_from_version(
        &self,
        version: Self::Version,
        id: Option<Self::StreamId>,
    ) -> Result<(Vec<E>, Self::Version), Error> {
        let options =
            ReadStreamOptions::default().position(Self::expected_revision_to_position(&version));

        let mut stream = self
            .client
            .read_stream(self.get_stream(id), &options)
            .await
            .map_err(Error::ESDBGeneral)?;

        let mut evts: Vec<ResolvedEvent> = vec![];

        loop {
            match stream.next().await {
                Ok(Some(event)) => evts.push(event),
                Ok(None) => break,
                Err(eventstore::Error::ResourceNotFound) => {
                    return Ok((vec![], ExpectedRevision::NoStream))
                }
                Err(e) => return Err(Error::ReadStream(e)),
            }
        }

        let mut rv = vec![];
        let mut pos = ExpectedRevision::StreamExists;

        for ev in evts {
            let event_data = ev.get_original_event();
            let event = event_data.as_json::<E>().map_err(Error::DeserializeEvent)?;

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
    ) -> Result<(Vec<E>, Self::Version), Error>
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        let mut perpared_events = vec![];

        for e in &events {
            let ed = EventData::json(e.event_type(), e.clone())
                .map(|ed| ed.id(Uuid::new_v4()))
                .map_err(Error::SerializeEventDataPayload)?;

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
            .map_err(|e| Error::WriteStream(stream, e))?;

        Ok((events, ExpectedRevision::Exact(res.next_expected_version)))
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use crate::test::deciders::user::{User, UserEvent, UserId, UserName, UserCommand};

    use super::*;

    const BASE_STREAM: &str = "decider.repository.esdb";

    fn store_from_environment() -> eventstore::Client {
        let _ = dotenv::dotenv().expect("File .env or Env Vars not found");
        let settings = dotenv::var("ESDB_CONNECTION_STRING")
            .expect("ESDB to be set in env")
            .parse()
            .expect("ESDB connection string to parse");

        eventstore::Client::new(settings).expect("Eventstore client")
    }

    #[actix_rt::test]
    async fn test_storage() {
        let client = store_from_environment();
        let event_repository = ESDBEventRepository::<UserEvent>::new(&client, BASE_STREAM);

        let events = vec![
            UserEvent::UserAdded(User {
                id: 1,
                name: UserName::try_from("Mike").expect("Name is valid"),
            }),
            UserEvent::UserNameUpdated(
                1 as UserId,
                UserName::try_from("Mike2").expect("Name is valid"),
            ),
        ];

        let res: (Vec<UserEvent>, ExpectedRevision) = event_repository.load(None).await.expect("loaded");

        println!("Result: {:?}", res);
    }
}
