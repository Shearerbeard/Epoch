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
    fn decide(cmd: Cmd, state: S) -> Result<Vec<E>, Err>;
    fn evolve(state: S, event: E) -> S;
    fn init() -> S;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use assert_matches::assert_matches;
    use thiserror::Error;

    use crate::decider::repository::StateRepository;

    use self::user::UserFieldError;

    use super::{
        repository::{
            in_memory::{InMemoryEventRepository, InMemoryStateRepository},
            EventRepository,
        },
        *,
    };

    trait ValueType<T> {
        fn value(&self) -> T;
    }

    mod user {
        use thiserror::Error;

        use super::ValueType;

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct User {
            pub id: UserId,
            pub name: UserName,
        }

        pub type UserId = usize;

        pub type UnvalidatedUserName = String;

        #[derive(Debug, Clone, PartialEq, Eq)]
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

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    struct UserDeciderState {
        users: HashMap<user::UserId, user::User>,
    }

    enum UserCommand {
        AddUser(user::UnvalidatedUserName),
        UpdateUserName(user::UserId, user::UnvalidatedUserName),
    }

    impl Command for UserCommand {
        type State = UserDeciderState;
    }

    #[derive(Clone)]
    enum UserEvent {
        UserAdded(user::User),
        UserNameUpdated(user::UserId, user::UserName),
    }

    impl Event for UserEvent {
        fn event_type(&self) -> String {
            match self {
                UserEvent::UserAdded(_) => "UserAdded".to_string(),
                UserEvent::UserNameUpdated(_, _) => "UserNameUpdated".to_string(),
            }
        }
    }

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

                    Ok(vec![UserEvent::UserAdded(user::User { id: 1, name })])
                }
                UserCommand::UpdateUserName(user_id, user_name) => {
                    let name = user::UserName::try_from(user_name)
                        .map_err(|e| UserDeciderError::UserField(e))?;

                    Ok(vec![UserEvent::UserNameUpdated(user_id, name)])
                }
            }
        }

        fn evolve(mut state: UserDeciderState, event: UserEvent) -> UserDeciderState {
            match event {
                UserEvent::UserAdded(user) => {
                    state.users.insert(user.id.to_owned(), user.to_owned());
                    state
                }
                UserEvent::UserNameUpdated(user_id, user_name) => {
                    state.users.get_mut(&user_id).unwrap().name = user_name.to_owned();
                    state
                }
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

    #[actix_rt::test]
    async fn test_raw_decider() {
        let event_repository: InMemoryEventRepository<UserCommand, UserEvent> =
            InMemoryEventRepository::new();
        let mut state_repository: InMemoryStateRepository<UserCommand> =
            InMemoryStateRepository::new();

        let state = event_repository
            .load()
            .await
            .expect("Empty Events Vector")
            .into_iter()
            .fold(UserDecider::init(), UserDecider::evolve);

        let cmd = UserCommand::AddUser("Mike".to_string() as user::UnvalidatedUserName);
        let events = UserDecider::decide(cmd, state.clone()).expect("Decider Success");

        if let Some(UserEvent::UserAdded(user::User { name, id })) = events.clone().first() {
            let user_id = id.clone();
            let user_name = name.clone();

            assert_eq!(name.value(), "Mike".to_string());

            let state = events.into_iter().fold(state.clone(), UserDecider::evolve);

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
