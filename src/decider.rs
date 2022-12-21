use std::marker::PhantomData;

use async_trait::async_trait;

trait Command {
    type State: Clone;
}
pub trait Event {}

#[async_trait]
trait Decider<S, Cmd: Command, E: Event, Err> {
    fn decide(cmd: Cmd, state: S) -> Result<Vec<E>, Err>;
    fn evolve(state: S, event: E) -> S;
    fn init() -> S;
}

trait EventRepository<C: Command, E: Event, Err> {
    fn load(&self) -> Result<Vec<E>, Err>;
    fn append(&mut self, events: Vec<E>) -> Result<Vec<E>, Err>;
}

trait LockingEventRepository {}

trait StateRepository<C: Command, Err> {
    fn reify(&self) -> <C as Command>::State;
    fn save(&mut self, state: <C as Command>::State) -> Result<<C as Command>::State, Err>;
}

trait LockingStateRepository {}

#[derive(Default)]
struct InMemoryEventRepository<C: Command, E: Event + Clone> {
    events: Vec<E>,
    position: usize,
    pd: PhantomData<C>,
}

impl<C: Command, E: Event + Clone> EventRepository<C, E, InMemoryEventRepositoryError>
    for InMemoryEventRepository<C, E>
{
    fn load(&self) -> Result<Vec<E>, InMemoryEventRepositoryError> {
        Ok(self.events.clone())
    }

    fn append(&mut self, events: Vec<E>) -> Result<Vec<E>, InMemoryEventRepositoryError> {
        self.events.extend(events.clone());
        self.position += 1;

        Ok(events)
    }
}

impl<C: Command, E: Event + Clone> InMemoryEventRepository<C, E> {
    fn new() -> Self {
        let events: Vec<E> = vec![];

        Self {
            events,
            position: Default::default(),
            pd: PhantomData::<C>::default(),
        }
    }
}

#[derive(Debug)]
enum InMemoryEventRepositoryError {}

struct InMemoryStateRepository<C: Command> {
    state: <C as Command>::State,
}

impl<C> InMemoryStateRepository<C>
where
    C: Command,
    <C as Command>::State: Default,
{
    fn new() -> Self {
        Self {
            state: <C as Command>::State::default(),
        }
    }
}

impl<C: Command> StateRepository<C, InMemoryEventRepositoryError> for InMemoryStateRepository<C> {
    fn reify(&self) -> <C as Command>::State {
        self.state.clone()
    }

    fn save(
        &mut self,
        state: <C as Command>::State,
    ) -> Result<<C as Command>::State, InMemoryEventRepositoryError> {
        self.state = state.clone();
        Ok(state)
    }
}

enum InMemoryStateRepositoryError {}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use thiserror::Error;

    use self::user::UserFieldError;

    use super::*;

    trait ValueType<T> {
        fn value(&self) -> T;
    }

    mod user {
        use thiserror::Error;

        use super::ValueType;

        #[derive(Clone)]
        pub struct User {
            pub id: UserId,
            pub name: UserName,
        }

        pub type UserId = usize;

        pub type UnvalidatedUserName = String;

        #[derive(Clone)]
        pub struct UserName(String);

        impl TryFrom<String> for UserName {
            type Error = UserFieldError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                let len = value.len();
                if len < 1 {
                    Err(UserFieldError::EmptyName)
                } else if len > 10 {
                    Err(UserFieldError::NameToLong(value.to_owned()))
                } else {
                    Ok(Self(value.to_owned()))
                }
            }
        }

        impl ValueType<String> for UserName {
            fn value(&self) -> String {
                self.0.to_owned()
            }
        }

        #[derive(Debug, Error)]
        pub enum UserFieldError {
            #[error("Username cannot be empty")]
            EmptyName,
            #[error("Username {0} is to long")]
            NameToLong(String),
        }
    }

    #[derive(Clone, Default)]
    struct UserDeciderState {
        users: HashMap<user::UserId, user::User>,
    }

    enum UserCommand {
        AddUser(user::UnvalidatedUserName),
        UpdateUserName(user::UserId, user::UserName),
    }

    impl Command for UserCommand {
        type State = UserDeciderState;
    }

    #[derive(Clone)]
    enum UserEvent {
        UserAdded(user::User),
        UserNameUpdated(user::UserId, user::UserName),
    }

    impl Event for UserEvent {}

    struct UserDecider {}

    impl Decider<UserDeciderState, UserCommand, UserEvent, UserDeciderError> for UserDecider {
        fn decide(
            cmd: UserCommand,
            state: UserDeciderState,
        ) -> Result<Vec<UserEvent>, UserDeciderError> {
            match cmd {
                UserCommand::AddUser(user_name) => {
                    let name = user::UserName::try_from(user_name)
                        .map_err(|e| UserDeciderError::UserField(e))?;

                    Ok(vec![UserEvent::UserAdded(user::User {
                        id: 1,
                        name
                    })])
                }
                UserCommand::UpdateUserName(_, _) => todo!(),
            }
        }

        fn evolve(state: UserDeciderState, event: UserEvent) -> UserDeciderState {
            match event {
                UserEvent::UserAdded(_) => todo!(),
                UserEvent::UserNameUpdated(_, _) => todo!(),
            }
        }

        fn init() -> UserDeciderState {
            Default::default()
        }
    }

    #[derive(Debug, Error)]
    enum UserDeciderError {
        #[error("Invalid user field {0:?}")]
        UserField(UserFieldError),
    }

    #[test]
    fn test_in_memory() {
        let event_repository: InMemoryEventRepository<UserCommand, UserEvent> =
            InMemoryEventRepository::new();
        let state_repository: InMemoryStateRepository<UserCommand> = InMemoryStateRepository::new();

        let state = event_repository
            .load()
            .expect("Empty Events Vector")
            .into_iter()
            .fold(UserDecider::init(), UserDecider::evolve);

        let cmd = UserCommand::AddUser("Mike".to_string() as user::UnvalidatedUserName);
        let events = UserDecider::decide(cmd, state).expect("Decider Success");

        if let Some(UserEvent::UserAdded(user::User{ name, .. })) = events.first() {
            assert_eq!(name.value(), "Mike".to_string())
        } else {
            panic!("Events not produced")
        }
    }
}
