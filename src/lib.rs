use std::{
    fmt::{Debug, Display},
    str::FromStr,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub mod event_store;
pub mod decider;

#[derive(Debug, Clone)]
pub struct EventEnvelope<Ctx>
where
    Ctx: EventContext + ?Sized,
    <Ctx as EventContext>::Id: FromStr + Clone,
    <Ctx as EventContext>::Event: Event + Clone,
{
    pub id: Uuid,
    pub event_context: String,
    pub event_context_id: Option<Ctx::Id>,
    pub time: DateTime<Utc>,
    pub data: Ctx::Event,
}

#[derive(Debug, Clone)]
pub struct PreparedEvent<Ctx>
where
    Ctx: EventContext + ?Sized,
{
    pub event_context: String,
    pub event_context_id: Option<Ctx::Id>,
    pub data: Ctx::Event,
}

pub trait Event {
    fn event_type(&self) -> String;
}

pub trait FromCommandInvariant {
    fn from_command_invariant<Ctx>(cmd_err: Ctx::Err) -> Self
    where
        Ctx: EventContext,
        <Ctx as EventContext>::Err: ToCommandInvariantError;
}

#[derive(Error, Debug)]
pub enum CommmandInvariantError {
    #[error("Command Invariant: {0}")]
    CommandInvariant(String),
    // #[error("User Command Invariant Command {0}")]
    // Users(UserError)
}

pub trait ToCommandInvariantError: Display {
    fn to_command_invariant_error(&self) -> CommmandInvariantError;
}

#[async_trait]
pub trait EventContext {
    type Id: ToString + FromStr + Send + Sync + Eq + PartialEq + Clone;
    type Command;
    type Event: Send + Sync + DeserializeOwned + Event + Clone;
    type Err: Debug;
    type Services;
    type State: Default;

    fn event_context() -> String;

    fn to_prepared_event(id: Option<Self::Id>, event: Self::Event) -> PreparedEvent<Self> {
        PreparedEvent {
            event_context: Self::event_context(),
            event_context_id: id,
            data: event,
        }
    }

    async fn handle(
        state: Self::State,
        cmd: Self::Command,
        services: Self::Services,
    ) -> Result<PreparedEvent<Self>, Self::Err>;

    fn apply(state: Self::State, event: &EventEnvelope<Self>) -> Self::State;
}

#[async_trait]
pub trait EventStore {
    type Error;
    type Position: Eq + PartialEq + Clone + Send;

    async fn load<Ctx: EventContext>(
        &self,
        id: Option<Ctx::Id>,
    ) -> Result<(Vec<EventEnvelope<Ctx>>, Self::Position), Self::Error>
    where
        <Ctx as EventContext>::Id: Display + FromStr;

    async fn append<Ctx>(
        &self,
        position: Self::Position,
        event: PreparedEvent<Ctx>,
        id: Option<Ctx::Id>,
    ) -> Result<(EventEnvelope<Ctx>, Self::Position), Self::Error>
    where
        Ctx: EventContext + Send + Sync,
        <Ctx as EventContext>::Id: Clone + Display,
        <Ctx as EventContext>::Event: Serialize + Event;

    async fn execute<Ctx>(
        &self,
        cmd: Ctx::Command,
        services: Ctx::Services,
        id: Option<<Ctx as EventContext>::Id>,
    ) -> Result<(EventEnvelope<Ctx>, Self::Position), Self::Error>
    where
        Ctx: EventContext + Send + Sync + Debug,
        <Ctx as EventContext>::Id: Display + Clone,
        <Ctx as EventContext>::Event: Send + Clone + Serialize + Debug,
        <Ctx as EventContext>::Services: Send,
        <Ctx as EventContext>::Command: Send,
        <Ctx as EventContext>::State: Send,
        <Ctx as EventContext>::Err: Send + Sync + Clone + ToCommandInvariantError,
        Self::Error: FromCommandInvariant,
    {
        let (value, position) = self.get_current_state::<Ctx>(id.clone()).await?;
        let res = Ctx::handle(value, cmd, services)
            .await
            .map_err(Self::Error::from_command_invariant::<Ctx>)?;

        Ok(self.append::<Ctx>(position, res, id).await?)
    }

    async fn get_current_state<Ctx>(
        &self,
        id: Option<Ctx::Id>,
    ) -> Result<(Ctx::State, Self::Position), Self::Error>
    where
        Ctx: EventContext + Send + Sync,
        <Ctx as EventContext>::Event: Send + Clone + Debug,
        <Ctx as EventContext>::Id: Display + Clone,
    {
        let (evts, position) = self.load::<Ctx>(id).await?;

        Ok((
            evts.iter()
                .fold(Ctx::State::default(), |state, evt| Ctx::apply(state, evt)),
            position,
        ))
    }
}
