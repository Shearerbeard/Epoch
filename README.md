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

#[derive(Debug)]
pub struct Trucks {}

/// IMPL EventContext to definte relationships between Event, Cmd, CmdErr, and some sort of reified State (This will be reverbed to a Decider pattern with fn decide() and fn evolve())
#[async_trait]
impl EventContext for Trucks {
    type Id = String;
    type Command = TruckCommand;
    type Event = TruckEvent;
    type Err = TruckError;
    type Services = ();
    type State = TruckState;

// Event Context requires we implement string names for each context - this is used at the event persistance layer
    fn event_context() -> String {
        "Trucks".to_string()
    }

    async fn handle(
        state: Self::State,
        cmd: Self::Command,
        _services: Self::Services,
    ) -> Result<PreparedEvent<Self>, Self::Err> {
        match cmd {
            TruckCommand::AddTruck(AddTruckCommand { name }) => {
                todo!()
            }
            TruckCommand::UpdateTruck(UpdateTruckCommand { truck_id, name }) => {
                todo!()
            }
        }
    }

    fn apply(mut state: TruckState, event: &EventEnvelope<Self>) -> TruckState {
        match event.data.clone() {
            TruckEvent::TruckAdded { truck_id, name } => {
                todo!()
            }
            TruckEvent::TruckUpdated { truck_id, name } => {
                todo!()
            }
        };
        state
    }
}

#[derive(Error, Debug, PartialEq, Eq, Clone)]
pub enum TruckError {
    // Error ADT
}

// Arbitrary service calling EventContext::execute()
pub struct TruckService {
    event_store: ESDBEventStore
}

impl TruckService {
    pub async fn cmd_add_truck(
        &self,
        cmd: AddTruckCommand,
    ) -> Result<
        (
            EventEnvelope<Trucks>,
            <ESDBEventStore as EventStore>::Position,
        ),
        TruckServiceError,
    > {
        // Execute knows how to handle a command and apply state automatically in the context of an event store (soon to be re-verbed for a decider pattern - execute knows how to decide and evolve state)
        self.event_store
            .execute::<Trucks>(TruckCommand::AddTruck(cmd), (), None)
            .await
            .map_err(TruckServiceError::EventStoreError)
    }
}

#[derive(Error, Debug)]
pub enum TruckServiceError {
    // Error ADT
}

// EventSourcing types and primatives can be whatever you want - usually Event and Cmd are parameteratized enums as ADT
pub enum TruckCommand {
    AddTruck(AddTruckCommand),
    UpdateTruck(UpdateTruckCommand),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AddTruckCommand {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct UpdateTruckCommand {
    pub truck_id: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TruckEvent {
    TruckAdded {
        truck_id: TruckId,
        name: TruckName,
    },
    TruckUpdated {
        truck_id: TruckId,
        name: Option<TruckName>,
    },
}

// Event Context requires we implement string names for event types - this are used at the event persistance layer
impl Event for TruckEvent {
    fn event_type(&self) -> String {
        match self {
            TruckEvent::TruckAdded { .. } => "TruckAdded".to_string(),
            TruckEvent::TruckUpdated { .. } => "TruckUpdated".to_string(),
        }
    }
}
```