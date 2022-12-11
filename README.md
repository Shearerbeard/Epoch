# Epoch
Event Sourcing + CQRS Framework

### Inspiration
This project is a collection of event sourcing and cqrs types to support some small personal projects heavily inluenced by [Thalo](https://github.com/thalo-rs/thalo) but borrowing (or will be borrowing in the future) ideas from Haskell [Eventful](https://github.com/jdreaver/eventful), F# [Equinox](https://github.com/jet/equinox), and Kotlin [f(model)](https://github.com/fraktalio/fmodel).


### Example
```rust
use epoch::{event_store::ESDBEventStore, EventEnvelope, EventStore, EventContext};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use async_trait::async_trait;

use example::domain::user::{UserId, User, UserName},

#[derive(Debug)]
pub struct Users {}

/// IMPL EventContext to definte relationships between Event, Cmd, CmdErr, and some sort of reified State (This will be reverbed to a Decider pattern with fn decide() and fn evolve())
#[async_trait]
impl EventContext for Users {
    type Id = String;
    type Command = UserCommand;
    type Event = UserEvent;
    type Err = UserError;
    type Services = ();
    type State = UserState;

// Event Context requires we implement string names for each context - this is used at the event persistance layer
    fn event_context() -> String {
        "Users".to_string()
    }

    async fn handle(
        state: Self::State,
        cmd: Self::Command,
        _services: Self::Services,
    ) -> Result<PreparedEvent<Self>, Self::Err> {
        match cmd {
            UserCommand::AddUser(AddUserCommand { name }) => {
                todo!()
            }
            UserCommand::UpdateUser(UpdateUserCommand { user_id, name }) => {
                todo!()
            }
        }
    }

    fn apply(mut state: UserState, event: &EventEnvelope<Self>) -> UserState {
        match event.data.clone() {
            UserEvent::UserAdded { user_id, name } => {
                todo!()
            }
            UserEvent::UserUpdated { user_id, name } => {
                todo!()
            }
        };
        state
    }
}

#[derive(Default, Debug, Clone)]
pub struct UserState {
    users: HashMap<String, User>,
}

#[derive(Error, Debug, PartialEq, Eq, Clone)]
pub enum UserError {
    // Error ADT
}

// Arbitrary service calling EventContext::execute()
pub struct UserService {
    event_store: ESDBEventStore
}

impl UserService {
    pub async fn cmd_add_user(
        &self,
        cmd: AddUserCommand,
    ) -> Result<
        (
            EventEnvelope<Users>,
            <ESDBEventStore as EventStore>::Position,
        ),
        UserServiceError,
    > {
        // Execute knows how to handle a command and apply state automatically in the context of an event store (soon to be re-verbed for a decider pattern - execute knows how to decide and evolve state)
        self.event_store
            .execute::<Users>(UserCommand::AddUser(cmd), (), None)
            .await
            .map_err(UserServiceError::EventStoreError)
    }
}

#[derive(Error, Debug)]
pub enum UserServiceError {
    // Error ADT
}

// EventSourcing types and primatives can be whatever you want - usually Event and Cmd are parameteratized enums as ADT
pub enum UserCommand {
    AddUser(AddUserCommand),
    UpdateUser(UpdateUserCommand),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AddUserCommand {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct UpdateUserCommand {
    pub user_id: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserEvent {
    UserAdded {
        user_id: UserId,
        name: UserName,
    },
    UserUpdated {
        user_id: UserId,
        name: Option<UserName>,
    },
}

// Event Context requires we implement string names for event types - this are used at the event persistance layer
impl Event for UserEvent {
    fn event_type(&self) -> String {
        match self {
            UserEvent::UserAdded { .. } => "UserAdded".to_string(),
            UserEvent::UserUpdated { .. } => "UserUpdated".to_string(),
        }
    }
}
```