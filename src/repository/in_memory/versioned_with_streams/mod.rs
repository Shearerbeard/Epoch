use async_trait::async_trait;

use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, Mutex},
};

use crate::{
    decider::Event,
    repository::{event::VersionedEventRepositoryWithStreams, RepositoryVersion},
};

use super::InMemoryEventRepositoryState;
use error::Error;

pub mod error;

pub struct InMemoryEventRepository<E>
where
    E: Event + Sync + Send + Debug,
{
    stream_name: String,
    state: HashMap<String, Arc<Mutex<InMemoryEventRepositoryState<E>>>>,
}

impl<'a, E> InMemoryEventRepository<E>
where
    E: Event + Sync + Send + Debug,
{
    pub fn new(stream_name: &str) -> Self {
        Self {
            stream_name: stream_name.to_owned(),
            state: HashMap::default(),
        }
    }

    fn get_base_stream_key(&self) -> String {
        self.stream_name.to_owned()
    }

    fn get_stream_key(&self, stream_id: Option<&String>) -> String {
        if let Some(id) = stream_id {
            format!("{}/{}", self.stream_name, id)
        } else {
            format!("{}", self.stream_name)
        }
    }

    fn get_stream_or_new(&mut self, key: &str) -> &Arc<Mutex<InMemoryEventRepositoryState<E>>> {
        if let None = self.state.get(key) {
            self.state.insert(
                key.to_owned(),
                Arc::new(Mutex::new(InMemoryEventRepositoryState::new())),
            );
        }

        self.state.get(key).unwrap()
    }

    fn index_from_version(version: &RepositoryVersion) -> usize {
        match version {
            RepositoryVersion::Exact(v) => *v,
            _ => 0,
        }
    }
}

#[async_trait]
impl<'a, E> VersionedEventRepositoryWithStreams<'a, E, Error> for InMemoryEventRepository<E>
where
    E: Event + Sync + Send + Clone + Debug,
{
    type StreamId = String;

    async fn load(
        &self,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, RepositoryVersion), Error> {
        self.load_from_version(&RepositoryVersion::Any, id).await
    }
    async fn load_from_version(
        &self,
        version: &RepositoryVersion,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, RepositoryVersion), Error> {
        let stream_key = self.get_stream_key(id);
        println!("Calling Stream {}", &stream_key);

        if let Some(m) = self.state.get(&stream_key) {
            let stream_state = m.lock().unwrap();

            let start = Self::index_from_version(version);
            let end = stream_state.position + 1;

            return Ok((
                stream_state.events[start..end].to_vec(),
                RepositoryVersion::Exact(stream_state.position),
            ));
        } else {
            return Ok((vec![], RepositoryVersion::Exact(0)));
        }
    }
    async fn append(
        &mut self,
        version: &RepositoryVersion,
        stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> Result<(Vec<E>, RepositoryVersion), Error>
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        let stream_key = self.get_stream_key(Some(stream));

        println!("Calling Stream {}", &stream_key);

        let mut stream = self.get_stream_or_new(&stream_key).lock().unwrap();

        if stream.position == Self::index_from_version(version) {
            stream.events.extend(events.clone());
            let position = stream.events.len() - 1;
            stream.position = position.clone();

            drop(stream); // Drop mutable reference so we can pull another and write to sub_stream

            let mut sub_stream = self
                .get_stream_or_new(&self.get_base_stream_key())
                .lock()
                .unwrap();
            sub_stream.events.extend(events.clone());
            let sub_position = sub_stream.events.len() - 1;
            sub_stream.position = sub_position;

            Ok((events.to_owned(), RepositoryVersion::Exact(position)))
        } else {
            Err(Error::VersionConflict)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_helpers::{
        deciders::user::UserEvent, repository::test_versioned_event_repository_with_streams,
    };

    use super::InMemoryEventRepository;

    const BASE_STREAM: &str = "test";

    #[actix_rt::test]
    async fn repository_spec_test() {
        let event_repository = InMemoryEventRepository::<UserEvent>::new(BASE_STREAM);
        let _ = test_versioned_event_repository_with_streams(event_repository).await;
    }
}
