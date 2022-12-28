use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::decider::{Command, Event};

use super::{EventRepository, StateRepository, VersionedEventRepositoryWithStreams};

/// Events ///
#[derive(Default)]
pub struct SimpleInMemoryEventRepository<E>
where
    E: Event + Clone + Send + Sync,
{
    state: Arc<Mutex<InMemoryEventRepositoryState<E>>>,
}

#[async_trait]
impl<E> EventRepository<E, SimpleInMemoryError> for SimpleInMemoryEventRepository<E>
where
    E: Event + Clone + Send + Sync,
{
    async fn load(&self) -> Result<Vec<E>, SimpleInMemoryError> {
        let lock = self.state.lock().unwrap();
        Ok(lock.events.clone())
    }

    async fn append(&mut self, events: &Vec<E>) -> Result<Vec<E>, SimpleInMemoryError> {
        let mut lock = self.state.lock().unwrap();
        lock.events.extend(events.to_owned());
        lock.position = lock.events.len();

        Ok(events.clone())
    }
}

impl<E> SimpleInMemoryEventRepository<E>
where
    E: Event + Clone + Send + Sync,
{
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(InMemoryEventRepositoryState::new())),
        }
    }
}

#[derive(Debug, Default)]
struct InMemoryEventRepositoryState<E> {
    events: Vec<E>,
    position: usize,
}

impl<E> InMemoryEventRepositoryState<E> {
    pub fn new() -> Self {
        InMemoryEventRepositoryState {
            events: vec![],
            position: 0,
        }
    }
}

#[derive(Debug)]
pub enum SimpleInMemoryError {}

pub struct StreamedVersionedInMemoryEventRepository<E>
where
    E: Event + Sync + Send,
{
    stream_name: String,
    state: HashMap<String, Arc<Mutex<InMemoryEventRepositoryState<E>>>>,
}

impl<'a, E> StreamedVersionedInMemoryEventRepository<E>
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

    fn index_from_version(version: &InMemoryStreamVersion) -> usize {
        match version {
            InMemoryStreamVersion::Any => 0,
            InMemoryStreamVersion::Exact(v) => *v,
        }
    }
}

#[async_trait]
impl<'a, E>
    VersionedEventRepositoryWithStreams<'a, E, StreamedVersionedInMemoryEventRepositoryError>
    for StreamedVersionedInMemoryEventRepository<E>
where
    E: Event + Sync + Send + Clone,
{
    type StreamId = String;
    type Version = InMemoryStreamVersion;

    async fn load(
        &self,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, Self::Version), StreamedVersionedInMemoryEventRepositoryError> {
        self.load_from_version(&InMemoryStreamVersion::Any, id)
            .await
    }
    async fn load_from_version(
        &self,
        version: &Self::Version,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, Self::Version), StreamedVersionedInMemoryEventRepositoryError> {
        let stream_key = self.get_stream(id);
        println!("Calling Stream {}", &stream_key);

        let stream_state = self.state.get(&stream_key);

        if let Some(m) = stream_state {
            let stream_state = m.lock().unwrap();

            let start = Self::index_from_version(version);
            let end = stream_state.position;

            return Ok((
                stream_state.events[start..end].to_vec(),
                Self::Version::Exact(stream_state.position),
            ));
        } else {
            return Ok((vec![], InMemoryStreamVersion::Exact(0)));
        }
    }
    async fn append(
        &mut self,
        version: &Self::Version,
        stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> Result<(Vec<E>, Self::Version), StreamedVersionedInMemoryEventRepositoryError>
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
                InMemoryStreamVersion::Exact(stream_state.events.len()),
            ))
        } else {
            Err(StreamedVersionedInMemoryEventRepositoryError::VersionConflict)
        }
    }
}

#[derive(Debug)]
pub enum StreamedVersionedInMemoryEventRepositoryError {
    VersionConflict,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InMemoryStreamVersion {
    Any,
    Exact(usize),
}

/// State ///
pub struct InMemoryStateRepository<C: Command> {
    state: <C as Command>::State,
}

impl<C> InMemoryStateRepository<C>
where
    C: Command + Send + Sync,
    <C as Command>::State: Default + Send + Sync + Debug,
{
    pub fn new() -> Self {
        Self {
            state: <C as Command>::State::default(),
        }
    }
}

#[async_trait]
impl<C> StateRepository<C, SimpleInMemoryError> for InMemoryStateRepository<C>
where
    C: Command + Send + Sync,
    <C as Command>::State: Default + Send + Sync + Debug,
{
    async fn reify(&self) -> <C as Command>::State {
        self.state.clone()
    }

    async fn save(
        &mut self,
        state: &<C as Command>::State,
    ) -> Result<<C as Command>::State, SimpleInMemoryError> {
        self.state = state.clone();
        Ok(self.state.to_owned())
    }
}

pub enum InMemoryStateRepositoryError {}
