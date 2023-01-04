pub(crate) mod user {
    use std::collections::HashMap;

    use serde::{Deserialize, Serialize};
    use thiserror::Error;

    use crate::{
        decider::{Decider, Event},
        test_helpers::ValueType,
    };

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(crate) struct User {
        pub id: UserId,
        pub name: UserName,
    }

    pub(crate) type UserId = usize;

    pub(crate) type UnvalidatedUserName = String;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(crate) struct UserName(String);

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

    impl TryFrom<&String> for UserName {
        type Error = UserFieldError;

        fn try_from(value: &String) -> Result<Self, Self::Error> {
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
    pub(crate) enum UserFieldError {
        #[error("Username cannot be empty")]
        EmptyName,
        #[error("Username {0} is to long")]
        NameToLong(String),
    }

    pub(crate) struct UserDecider {}

    impl Decider for UserDecider {
        type State = UserDeciderState;

        type Cmd = UserCommand;

        type Evt = UserEvent;

        type Err = UserDeciderError;

        fn decide(
            _state: &UserDeciderState,
            cmd: &UserCommand,
        ) -> Result<Vec<UserEvent>, UserDeciderError> {
            match cmd {
                UserCommand::AddUser(user_name) => {
                    let name = UserName::try_from(user_name)
                        .map_err(|e| UserDeciderError::UserField(e))?;

                    Ok(vec![UserEvent::UserAdded(User { id: 1, name })])
                }
                UserCommand::UpdateUserName(user_id, user_name) => {
                    let name = UserName::try_from(user_name)
                        .map_err(|e| UserDeciderError::UserField(e))?;

                    Ok(vec![UserEvent::UserNameUpdated(user_id.to_owned(), name)])
                }
            }
        }

        fn evolve(mut state: UserDeciderState, event: &UserEvent) -> UserDeciderState {
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

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub(crate) struct UserDeciderState {
        pub(crate) users: HashMap<UserId, User>,
    }

    #[derive(Debug)]
    pub(crate) enum UserCommand {
        AddUser(UnvalidatedUserName),
        UpdateUserName(UserId, UnvalidatedUserName),
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub(crate) enum UserEvent {
        UserAdded(User),
        UserNameUpdated(UserId, UserName),
    }

    impl Event for UserEvent {
        fn event_type(&self) -> String {
            match self {
                UserEvent::UserAdded(_) => "UserAdded".to_string(),
                UserEvent::UserNameUpdated(_, _) => "UserNameUpdated".to_string(),
            }
        }
    }

    #[derive(Debug, Error)]
    pub(crate) enum UserDeciderError {
        #[error("Invalid user field {0:?}")]
        UserField(UserFieldError),
    }
}
