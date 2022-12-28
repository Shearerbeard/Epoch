use std::{fmt::Debug, marker::PhantomData};

use async_trait::async_trait;
use eventstore::{
    AppendToStreamOptions, Client, EventData, ExpectedRevision, ReadStreamOptions, ResolvedEvent,
    StreamPosition,
};
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;

use crate::decider::Event;

use self::error::Error;

use super::event::VersionedEventRepositoryWithStreams;

pub mod error;

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

    fn expected_revision_to_position(er: &ExpectedRevision) -> StreamPosition<u64> {
        if let ExpectedRevision::Exact(u) = er {
            StreamPosition::Position(u.to_owned())
        } else {
            StreamPosition::Start
        }
    }
}

#[async_trait]
impl<'a, E> VersionedEventRepositoryWithStreams<'a, E, Error> for ESDBEventRepository<E>
where
    E: Event + Sync + Send + Serialize + DeserializeOwned + Clone + Debug,
{
    type StreamId = String;
    type Version = ExpectedRevision;

    async fn load(&self, id: Option<&Self::StreamId>) -> Result<(Vec<E>, Self::Version), Error> {
        self.load_from_version(&ExpectedRevision::Any, id).await
    }

    async fn load_from_version(
        &self,
        version: &Self::Version,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, Self::Version), Error> {
        println!("Calling Stream {}", self.get_stream(id));
        let mut stream = self
            .client
            .read_stream(
                self.get_stream(id),
                &ReadStreamOptions::default()
                    .resolve_link_tos()
                    .position(Self::expected_revision_to_position(&version)),
            )
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
            pos = ExpectedRevision::Exact(ev.get_original_event().revision);

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
        version: &Self::Version,
        stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> Result<(Vec<E>, Self::Version), Error>
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        let mut perpared_events = vec![];

        for e in events {
            let ed = EventData::json(e.event_type(), e)
                .map(|ed| ed.id(Uuid::new_v4()))
                .map_err(Error::SerializeEventDataPayload)?;

            perpared_events.push(ed);
        }

        let res = self
            .client
            .append_to_stream(
                self.get_stream(Some(stream)),
                &AppendToStreamOptions::default().expected_revision(*version),
                perpared_events,
            )
            .await
            .map_err(|e| Error::WriteStream(stream.to_owned(), e))?;

        Ok((
            events.to_owned(),
            ExpectedRevision::Exact(res.next_expected_version),
        ))
    }
}

#[cfg(test)]
mod tests {
    use core::time;
    use std::thread;

    use assert_matches::assert_matches;
    use eventstore::DeleteStreamOptions;

    use crate::test_helpers::deciders::user::{User, UserEvent, UserId, UserName};

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
    async fn test_storage() {
        let client = store_from_environment(vec![1, 2]).await;

        let id_1 = "1".to_string();
        let id_2 = "2".to_string();

        let mut event_repository = ESDBEventRepository::<UserEvent>::new(&client, BASE_STREAM);

        let res: (Vec<UserEvent>, ExpectedRevision) =
            event_repository.load(None).await.expect("loaded");
        assert_matches!(res, (v, _) if v == vec![] as Vec<UserEvent>);

        let events1 = vec![
            UserEvent::UserAdded(User {
                id: 1,
                name: UserName::try_from("Mike").expect("Name is valid"),
            }),
            UserEvent::UserNameUpdated(
                1 as UserId,
                UserName::try_from("Mike2").expect("Name is valid"),
            ),
        ];

        let _ = event_repository
            .append(&ExpectedRevision::Any, &id_1, &events1)
            .await;

        let events2 = vec![
            UserEvent::UserAdded(User {
                id: 2,
                name: UserName::try_from("Stella").expect("Name is valid"),
            }),
            UserEvent::UserNameUpdated(
                1 as UserId,
                UserName::try_from("Stella2").expect("Name is valid"),
            ),
        ];

        let _ = event_repository
            .append(&ExpectedRevision::Any, &id_2, &events2)
            .await;

        // Crude but we need to wait for ESDB to catch up its "Categories" auto projection
        thread::sleep(time::Duration::from_secs(1));

        let res = event_repository.load(Some(&id_1)).await;
        assert_matches!(res, Ok((v, ExpectedRevision::Exact(_))) if v == events1);

        let res = event_repository.load(Some(&id_2)).await;
        assert_matches!(res, Ok((v, ExpectedRevision::Exact(_))) if v == events2);

        let res = event_repository.load(None).await;

        let events_combined: Vec<UserEvent> =
            events1.into_iter().chain(events2.into_iter()).collect();
        assert_matches!(res, Ok((v, ExpectedRevision::Exact(_))) if v == events_combined);

        let res = event_repository.load(Some(&id_1)).await;
        let version = res.unwrap().1;

        let new_events = vec![UserEvent::UserNameUpdated(
            1,
            UserName::try_from("Mike").expect("Name is valid"),
        )];

        let res = event_repository
            .append(&version, &id_1, &new_events)
            .await
            .expect("Success");

        let version = res.1;

        let (latest_events, _) = event_repository
            .load_from_version(&version, Some(&id_1))
            .await
            .expect("load success");

        assert_eq!(latest_events.first().unwrap(), new_events.first().unwrap());
    }
}
