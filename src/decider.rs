pub trait Command {
    type State: Clone + Send + Sync;
}
pub trait Event {
    fn event_type(&self) -> String;
}

pub trait Decider<State, Cmd: Command, Evt: Event, Err> {
    fn decide(state: &State, cmd: &Cmd) -> Result<Vec<Evt>, Err>;
    fn evolve(state: State, event: &Evt) -> State;
    fn init() -> State;
}

pub trait DeciderWithContext<State, Ctx, Cmd, Evt, Err>
where
    Evt: Event,
    Cmd: Command,
{
    fn decide(ctx: &Ctx, state: &State, cmd: &Cmd) -> Result<Vec<Evt>, Err>;
    fn evolve(state: State, event: &Evt) -> State;
    fn init() -> State;
}

pub trait Evolver<State, Evt: Event> {
    fn evolve(state: State, event: &Evt) -> State;
}

pub trait CommandState<C: Command> {
    fn from(value: <C as Command>::State) -> Self;
}

pub trait DeciderVariant<S, Cmd: Command, E: Event, Err> {
    fn decide(cs: impl CommandState<Cmd>) -> Result<Vec<E>, Err>;
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
            deciders::user::{self, UserCommand, UserDecider, UserEvent},
            ValueType,
        },
    };

    use super::*;

    #[actix_rt::test]
    async fn test_raw_decider() {
        let event_repository: InMemoryEventRepository<UserEvent> = InMemoryEventRepository::new();
        let mut state_repository: InMemoryStateRepository<UserCommand> =
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
