use async_trait::async_trait;

use std::{
    collections::HashMap,
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
    E: Event + Sync + Send,
{
    stream_name: String,
    state: HashMap<String, Arc<Mutex<InMemoryEventRepositoryState<E>>>>,
}

impl<'a, E> InMemoryEventRepository<E>
where
    E: Event + Sync + Send,
{
    pub fn new(stream_name: &str) -> Self {
        Self {
            stream_name: stream_name.to_owned(),
            state: HashMap::default(),
        }
    }

    fn get_stream(&self, stream_id: Option<&String>) -> String {
        if let Some(id) = stream_id {
            format!("{}/{}", self.stream_name, id)
        } else {
            format!("{}", self.stream_name)
        }
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
    E: Event + Sync + Send + Clone,
{
    type StreamId = String;

    async fn load(&self, id: Option<&Self::StreamId>) -> Result<(Vec<E>, RepositoryVersion), Error> {
        self.load_from_version(&RepositoryVersion::Any, id).await
    }
    async fn load_from_version(
        &self,
        version: &RepositoryVersion,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, RepositoryVersion), Error> {
        let stream_key = self.get_stream(id);
        println!("Calling Stream {}", &stream_key);

        if let Some(m) = self.state.get(&stream_key) {
            let stream_state = m.lock().unwrap();

            let start = Self::index_from_version(version);
            let end = stream_state.position;

            return Ok((
                stream_state.events[start..end].to_vec(),
                RepositoryVersion::Exact(stream_state.position),
            ));
        } else {
            return Ok((vec![], RepositoryVersion::NoStream));
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
        let stream_key = self.get_stream(Some(stream));
        println!("Calling Stream {}", &stream_key);

        let stream_state = if let Some(s) = self.state.get(&stream_key) {
            s.to_owned()
        } else {
            let n = Arc::new(Mutex::new(InMemoryEventRepositoryState::new()));
            n
        };

        let mut stream_state = stream_state.lock().unwrap();

        if stream_state.position == Self::index_from_version(version) {
            stream_state.events.extend(events.clone());
            stream_state.position = stream_state.events.len();

            Ok((
                events.to_owned(),
                RepositoryVersion::Exact(stream_state.events.len()),
            ))
        } else {
            Err(Error::VersionConflict)
        }
    }
}
