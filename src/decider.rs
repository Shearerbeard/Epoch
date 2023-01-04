pub trait Command {
    type State: Clone + Send + Sync;
}
pub trait Event {
    fn event_type(&self) -> String;
}

pub trait Decider {
    type State;
    type Cmd: Send + Sync;
    type Evt: Event;
    type Err;

    fn decide(state: &Self::State, cmd: &Self::Cmd) -> Result<Vec<Self::Evt>, Self::Err>;
    fn evolve(state: Self::State, event: &Self::Evt) -> Self::State;
    fn init() -> Self::State;
}

pub trait DeciderWithContext {
    type Ctx;
    type State;
    type Cmd: Send + Sync;
    type Evt: Event;
    type Err;

    fn decide(
        ctx: &Self::Ctx,
        state: &Self::State,
        cmd: &Self::Cmd,
    ) -> Result<Vec<Self::Evt>, Self::Err>;
    fn evolve(state: Self::State, event: &Self::Evt) -> Self::State;
    fn init() -> Self::State;
}

pub trait Evolver<State, Evt: Event> {
    fn evolve(state: State, event: &Evt) -> State;
}

#[cfg(test)]
mod tests {

    use assert_matches::assert_matches;

    use crate::{
        repository::{
            event::EventRepository,
            in_memory::simple::{InMemoryEventRepository, InMemoryStateRepository},
            state::StateRepository,
        },
        test_helpers::{
            deciders::user::{self, UserCommand, UserDecider, UserDeciderState, UserEvent},
            ValueType,
        },
    };

    use super::*;

    #[actix_rt::test]
    async fn test_raw_decider() {
        let event_repository: InMemoryEventRepository<UserEvent> = InMemoryEventRepository::new();
        let mut state_repository: InMemoryStateRepository<UserDeciderState> =
            InMemoryStateRepository::new();

        let state = event_repository
            .load()
            .await
            .expect("Empty Events Vector")
            .iter()
            .fold(UserDecider::init(), UserDecider::evolve);

        let cmd = UserCommand::AddUser("Mike".to_string() as user::UnvalidatedUserName);
        let events = UserDecider::decide(&state, &cmd).expect("Decider Success");

        if let Some(UserEvent::UserAdded(user::User { name, id })) = events.clone().first() {
            let user_id = id.clone();
            let user_name = name.clone();

            assert_eq!(name.value(), "Mike".to_string());

            let state = events.iter().fold(state.clone(), UserDecider::evolve);

            let _ = state_repository.save(&state).await;
            assert_eq!(state_repository.reify().await, state.clone());

            assert_matches!(state.users.get(&id).expect("User exists"), user::User {
                id,
                name
            } if (id == &user_id && name == &user_name));
        } else {
            panic!("Events not produced")
        }
    }
}
