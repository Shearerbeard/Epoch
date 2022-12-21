use std::marker::PhantomData;

use async_trait::async_trait;

trait Command {
    type State: Default + Clone;
}
trait Event {}

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

enum InMemoryEventRepositoryError {}

#[derive(Default)]
struct InMemoryStateRepository<C: Command> {
    state: <C as Command>::State,
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

    use super::*;

    trait ValueType<T> {
        fn value(&self) -> T;
    }

    mod user {
        use super::ValueType;

        #[derive(Clone)]
        pub struct User {
            pub id: UserId,
            pub name: String,
        }

        pub type UserId = usize;
        pub struct UserName(String);

        impl TryFrom<&str> for UserName {
            type Error = UserFieldError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
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

        pub enum UserFieldError {
            EmptyName,
            NameToLong(String),
        }
    }

    #[derive(Default, Clone)]
    struct UserDeciderState {
        users: HashMap<user::UserId, user::User>,
    }

    enum UserCommand {
        AddUser(user::UserName),
        UpdateUserName(user::UserId, user::UserName),
    }

    impl Command for UserCommand {
        type State = UserDeciderState;
    }

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
            match cmd  {
                UserCommand::AddUser(_) => todo!(),
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

    enum UserDeciderError {}

    #[test]
    fn test_in_memory() {}
}
