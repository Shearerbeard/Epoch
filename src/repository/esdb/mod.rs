use std::{fmt::Debug, marker::PhantomData};

use async_trait::async_trait;
use eventstore::{
    AppendToStreamOptions, Client, CurrentRevision, EventData, ExpectedRevision, ReadStreamOptions,
    ResolvedEvent, StreamPosition,
};
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;

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

impl<E> ESDBEventRepository<E> {
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
                    .position(Self::version_to_esdb_position(version)),
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
        E: 'async_trait + Send + Sync,
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

#[cfg(test)]
mod tests {
    use const_random::const_random;
    use core::time;
    use futures::{
        future::{self, BoxFuture},
        FutureExt,
    };
    use std::{
        collections::{HashMap, HashSet},
        thread,
    };

    use assert_matches::assert_matches;
    use eventstore::DeleteStreamOptions;

    use crate::strategies::{LoadDecideAppend, StateFromEventRepository, StreamState};

    use crate::test_helpers::{
        deciders::user::{
            Guitar, User, UserCommand, UserDecider, UserDeciderCtx, UserDeciderState, UserEvent,
            UserId, UserName,
        },
        repository::test_versioned_event_repository_with_streams,
        ValueType,
    };

    use super::*;

    const BASE_STREAM: u32 = const_random!(u32);

    async fn store_from_environment(base_stream: &str, ids: Vec<usize>) -> eventstore::Client {
        let _ = dotenv::dotenv().expect("File .env or Env Vars not found");
        let settings = dotenv::var("ESDB_CONNECTION_STRING")
            .expect("ESDB to be set in env")
            .parse()
            .expect("ESDB connection string to parse");

        let client = eventstore::Client::new(settings).expect("Eventstore client");

        for id in ids {
            let _ = client
                .delete_stream(
                    format!("{}-{}", base_stream, id),
                    &DeleteStreamOptions::default(),
                )
                .await;
        }

        client
    }

    async fn add_guitar(base_stream: String, user_id: UserId, guitar: Guitar) {
        let ctx = UserDeciderCtx::new();
        let client = store_from_environment(&base_stream.to_string(), vec![]).await;
        let mut event_repository =
            ESDBEventRepository::<UserEvent>::new(&client, &base_stream.to_string());

        println!("Adding Guitar {:?} for user {}", &guitar.brand, &user_id);

        let cmd = UserCommand::AddGuitar(user_id, guitar.to_owned());

        let res = UserDecider::execute(
            UserDeciderState::default(),
            &mut event_repository,
            &StreamState::Existing(user_id.to_string()),
            &ctx,
            &cmd,
            None,
        )
        .await;

        println!(
            "Result for Guitar {:?} for user {}: {:?}",
            &guitar.brand, &user_id, res
        );
    }

    #[actix_rt::test]
    async fn repository_spec_test() {
        let base_stream = BASE_STREAM;
        let client = store_from_environment(&base_stream.to_string(), vec![1, 2]).await;
        let event_repository =
            ESDBEventRepository::<UserEvent>::new(&client, &base_stream.to_string());

        let _ = test_versioned_event_repository_with_streams(event_repository).await;
    }

    #[actix_rt::test]
    async fn test_occ() {
        let base_stream = format!("{}_with_occ", BASE_STREAM);
        let client = store_from_environment(&base_stream.to_string(), vec![1]).await;
        let mut event_repository =
            ESDBEventRepository::<UserEvent>::new(&client, &base_stream.to_string());
        let ctx = UserDeciderCtx::new();

        let cmd1 = UserCommand::AddUser("Mike".to_string());

        let evts = UserDecider::execute(
            UserDeciderState::default(),
            &mut event_repository,
            &StreamState::New,
            &ctx,
            &cmd1,
            None,
        )
        .await
        .expect("command_succeeds");

        let first_id = evts.first().unwrap().get_id();

        assert_matches!(
            evts.first().expect("one event"),
            UserEvent::UserAdded(User { id, name, .. }) if (&first_id == id) && (name.value() == "Mike".to_string())
        );

        let state = UserDeciderState::load_by_id(
            UserDeciderState::default(),
            &event_repository,
            &first_id.to_string(),
        )
        .await
        .expect("state is loaded");

        assert_matches!(
            state,
            UserDeciderState { users } if users == HashMap::from([(first_id.clone(), User::new(first_id, UserName::try_from("Mike".to_string()).unwrap()))])
        );

        let guitars = vec![
            Guitar {
                brand: "Ibanez".to_string(),
            },
            Guitar {
                brand: "Gibson".to_string(),
            },
            Guitar {
                brand: "Fender".to_string(),
            },
            Guitar {
                brand: "Eastman".to_string(),
            },
            Guitar {
                brand: "Meyones".to_string(),
            },
            Guitar {
                brand: "PRS".to_string(),
            },
            Guitar {
                brand: "Yamaha".to_string(),
            },
            Guitar {
                brand: "Benedetto".to_string(),
            },
            Guitar {
                brand: "Strandberg".to_string(),
            },
        ];

        let futures = guitars
            .iter()
            .cloned()
            .map(|g| add_guitar(base_stream.clone(), first_id.clone(), g).boxed())
            .collect::<Vec<BoxFuture<()>>>();

        future::join_all(futures).await;

        thread::sleep(time::Duration::from_secs(1));

        let state = UserDeciderState::load_by_id(
            UserDeciderState::default(),
            &event_repository,
            &first_id.to_string(),
        )
        .await
        .expect("state is loaded");

        assert_eq!(
            state.users.get(&first_id).unwrap().guitars,
            HashSet::from_iter(guitars.iter().cloned())
        );
    }
}
