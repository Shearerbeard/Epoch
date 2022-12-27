use async_trait::async_trait;

pub mod repository;

pub trait Command {
    type State: Clone + Send + Sync;
}
pub trait Event {
    fn event_type(&self) -> String;
}

#[async_trait]
pub trait Decider<S, Cmd: Command, E: Event, Err> {
    fn decide(cmd: &Cmd, state: &S) -> Result<Vec<E>, Err>;
    fn evolve(state: S, event: &E) -> S;
    fn init() -> S;
}

#[cfg(test)]
mod tests {

    use assert_matches::assert_matches;

    use crate::{
        decider::repository::StateRepository,
        test::{
            deciders::user::{self, UserCommand, UserDecider, UserEvent},
            ValueType,
        },
    };

    use super::{
        repository::{
            in_memory::{SimpleInMemoryEventRepository, InMemoryStateRepository},
            EventRepository,
        },
        *,
    };

    #[actix_rt::test]
    async fn test_raw_decider() {
        let event_repository: SimpleInMemoryEventRepository<UserEvent> = SimpleInMemoryEventRepository::new();
        let mut state_repository: InMemoryStateRepository<UserCommand> =
            InMemoryStateRepository::new();

        let state = event_repository
            .load()
            .await
            .expect("Empty Events Vector")
            .iter()
            .fold(UserDecider::init(), UserDecider::evolve);

        let cmd = UserCommand::AddUser("Mike".to_string() as user::UnvalidatedUserName);
        let events = UserDecider::decide(&cmd, &state).expect("Decider Success");

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
