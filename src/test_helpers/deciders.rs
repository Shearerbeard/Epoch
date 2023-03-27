pub(crate) mod user {
    use std::{
        collections::{HashMap, HashSet},
        sync::{Arc, Mutex},
    };

    use autoincrement::{AsyncIncrement, AsyncIncremental};
    use serde::{Deserialize, Serialize};
    use thiserror::Error;

    use crate::{
        decider::{Decider, DeciderWithContext, Event, Evolver},
        repository::event::StreamIdFromEvent,
        strategies::{
            DecideEvolveWithCommandResponse, LoadDecideAppend, ReifyDecideSave,
            StateFromEventRepository,
        },
        test_helpers::ValueType,
    };

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(crate) struct User {
        pub id: UserId,
        pub name: UserName,
        pub guitars: HashSet<Guitar>,
    }

    impl User {
        pub fn new(id: UserId, name: UserName) -> Self {
            Self {
                id,
                name,
                guitars: HashSet::new(),
            }
        }
    }

    pub(crate) type UserId = usize;

    pub(crate) type UnvalidatedUserName = String;

    #[derive(Debug, Clone, Hash, Serialize, Deserialize, PartialEq, Eq)]
    pub(crate) struct Guitar {
        pub brand: String,
    }

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

    #[derive(Debug)]
    pub(crate) struct UserDecider;

    impl Decider for UserDecider {
        type Cmd = UserCommand;

        type Err = UserDeciderError;

        fn decide(
            state: &UserDeciderState,
            cmd: &UserCommand,
        ) -> Result<Vec<UserEvent>, UserDeciderError> {
            match cmd {
                UserCommand::AddUser(user_name) => {
                    let name = UserName::try_from(user_name)
                        .map_err(|e| UserDeciderError::UserField(e))?;

                    Ok(vec![UserEvent::UserAdded(User {
                        id: 1,
                        name,
                        guitars: HashSet::new(),
                    })])
                }
                UserCommand::UpdateUserName(user_id, user_name) => {
                    let name = UserName::try_from(user_name)
                        .map_err(|e| UserDeciderError::UserField(e))?;

                    Ok(vec![UserEvent::UserNameUpdated(user_id.to_owned(), name)])
                }
                UserCommand::AddGuitar(user_id, guitar) => {
                    println!("ADD GUITAR STATE: {:?}", &state);
                    let user = state
                        .users
                        .get(&user_id)
                        .ok_or(UserDeciderError::NotFound(*user_id))?;
                    if user.guitars.contains(&guitar) {
                        Err(UserDeciderError::AlreadyHasGuitar(guitar.to_owned()))
                    } else {
                        Ok(vec![UserEvent::UserGuitarAdded(
                            *user_id,
                            guitar.to_owned(),
                        )])
                    }
                }
            }
        }
    }

    impl Evolver for UserDecider {
        type State = UserDeciderState;

        type Evt = UserEvent;

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
                UserEvent::UserGuitarAdded(user_id, guitar) => {
                    state
                        .users
                        .get_mut(&user_id)
                        .unwrap()
                        .guitars
                        .insert(guitar.to_owned());
                    state
                }
            }
        }
    }

    impl DeciderWithContext for UserDecider {
        type Ctx = UserDeciderCtx;

        type Cmd = UserCommand;

        type Err = UserDeciderError;

        fn decide(
            ctx: &UserDeciderCtx,
            state: &UserDeciderState,
            cmd: &UserCommand,
        ) -> Result<Vec<UserEvent>, UserDeciderError> {
            match cmd {
                UserCommand::AddUser(user_name) => {
                    let seq = ctx.id_sequence.lock().unwrap();
                    let IdGen(id) = seq.pull();

                    let name = UserName::try_from(user_name)
                        .map_err(|e| UserDeciderError::UserField(e))?;

                    Ok(vec![UserEvent::UserAdded(User {
                        id,
                        name,
                        guitars: HashSet::new(),
                    })])
                }
                UserCommand::UpdateUserName(user_id, user_name) => {
                    let name = UserName::try_from(user_name)
                        .map_err(|e| UserDeciderError::UserField(e))?;

                    Ok(vec![UserEvent::UserNameUpdated(user_id.to_owned(), name)])
                }
                UserCommand::AddGuitar(user_id, guitar) => {
                    let user = state
                        .users
                        .get(&user_id)
                        .ok_or(UserDeciderError::NotFound(*user_id))?;
                    if user.guitars.contains(&guitar) {
                        Err(UserDeciderError::AlreadyHasGuitar(guitar.to_owned()))
                    } else {
                        Ok(vec![UserEvent::UserGuitarAdded(
                            *user_id,
                            guitar.to_owned(),
                        )])
                    }
                }
            }
        }
    }

    impl LoadDecideAppend for UserDecider {
        type Decide = Self;
    }

    impl DecideEvolveWithCommandResponse for UserDecider {
        type Decide = Self;
    }

    impl ReifyDecideSave for UserDecider {
        type Decide = Self;
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct UserDeciderState {
        pub(crate) users: HashMap<UserId, User>,
    }

    impl UserDeciderState {
        pub fn new(users: HashMap<UserId, User>) -> Self {
            Self::default().set_users(users)
        }

        pub fn set_users(&self, users: HashMap<UserId, User>) -> Self {
            Self { users, ..*self }
        }
    }

    impl Default for UserDeciderState {
        fn default() -> Self {
            Self {
                users: HashMap::new().to_owned(),
            }
        }
    }

    #[derive(Clone, Debug)]
    pub(crate) struct UserDeciderCtx {
        id_sequence: Arc<Mutex<AsyncIncrement<IdGen>>>,
    }

    impl UserDeciderCtx {
        pub fn new() -> Self {
            Self {
                id_sequence: Arc::new(Mutex::new(IdGen::init())),
            }
        }

        pub fn current(&self) -> usize {
            let IdGen(id) = self.id_sequence.lock().unwrap().current();
            id
        }
    }

    impl StateFromEventRepository for UserDeciderState {
        type Ev = UserDecider;
    }

    #[derive(Debug)]
    pub(crate) enum UserCommand {
        AddUser(UnvalidatedUserName),
        UpdateUserName(UserId, UnvalidatedUserName),
        AddGuitar(UserId, Guitar),
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub(crate) enum UserEvent {
        UserAdded(User),
        UserNameUpdated(UserId, UserName),
        UserGuitarAdded(UserId, Guitar),
    }

    impl Event for UserEvent {
        type EntityId = UserId;

        fn event_type(&self) -> String {
            match self {
                UserEvent::UserAdded(_) => "UserAdded".to_string(),
                UserEvent::UserNameUpdated(_, _) => "UserNameUpdated".to_string(),
                UserEvent::UserGuitarAdded(_, _) => "UserGuitarAdded".to_string(),
            }
        }

        fn get_id(&self) -> Self::EntityId {
            match self {
                UserEvent::UserAdded(User { id, .. }) => id,
                UserEvent::UserNameUpdated(id, _) => id,
                UserEvent::UserGuitarAdded(id, _) => id,
            }
            .to_owned()
        }
    }

    impl StreamIdFromEvent<UserEvent> for String {
        fn event_entity_id_into(id: <UserEvent as Event>::EntityId) -> Self {
            id.to_string()
        }
    }

    #[derive(Debug, Error)]
    pub(crate) enum UserDeciderError {
        #[error("Invalid user field {0:?}")]
        UserField(UserFieldError),
        #[error("User id {0} not found")]
        NotFound(UserId),
        #[error("Already has guitar {0:?}")]
        AlreadyHasGuitar(Guitar),
    }

    #[derive(AsyncIncremental, PartialEq, Eq, Clone, Debug)]
    pub struct IdGen(usize);
}
