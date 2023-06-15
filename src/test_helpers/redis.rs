use std::collections::HashSet;

use redis_om::{redis, RedisTransportValue, StreamModel};

use crate::repository::redis::versioned_event::{StreamModelDTO, WithSubStreamId};

use super::{
    deciders::user::{Guitar, User, UserEvent, UserId, UserName},
    ValueType,
};

#[derive(StreamModel, Debug, Clone)]
pub struct TestUserEventDTO {
    user_id: UserId,
    event_type: UserEventTypeDTO,
    user: Option<UserDTO>,
    user_name: Option<String>,
    guitar: Option<GuitarDTO>,
}

#[derive(RedisTransportValue, Debug, Clone, Copy)]
pub(crate) enum UserEventTypeDTO {
    UserAdded,
    UserNameUpdated,
    UserGuitarAdded,
}

#[derive(RedisTransportValue, Debug, Clone)]
pub struct UserDTO {
    pub id: usize,
    pub name: String,
    pub guitars: HashSet<GuitarDTO>,
}

#[derive(RedisTransportValue, PartialEq, Eq, Hash, Debug, Clone)]
pub struct GuitarDTO {
    brand: String,
}

impl WithSubStreamId for TestUserEventDTO {
    fn to_sub_stream_id(&self) -> String {
        self.user_id.clone().to_string()
    }
}

impl From<User> for UserDTO {
    fn from(value: User) -> Self {
        Self {
            id: value.id,
            name: value.name.value(),
            guitars: value.guitars.into_iter().map(|g| g.into()).collect(),
        }
    }
}

impl From<UserDTO> for User {
    fn from(value: UserDTO) -> Self {
        Self {
            id: value.id,
            name: value.name.try_into().unwrap(),
            guitars: value.guitars.into_iter().map(|g| g.into()).collect(),
        }
    }
}

impl From<Guitar> for GuitarDTO {
    fn from(value: Guitar) -> Self {
        Self { brand: value.brand }
    }
}

impl From<GuitarDTO> for Guitar {
    fn from(value: GuitarDTO) -> Self {
        Self { brand: value.brand }
    }
}

impl StreamModelDTO<TestUserEventDTOManager> for UserEvent {
    fn into_dto(self) -> <TestUserEventDTOManager as StreamModel>::Data {
        match self {
            UserEvent::UserAdded(user) => TestUserEventDTO {
                user_id: user.id.into(),
                event_type: UserEventTypeDTO::UserAdded,
                user: Some(user.into()),
                guitar: None,
                user_name: None,
            },
            UserEvent::UserNameUpdated(user_id, user_name) => TestUserEventDTO {
                user_id: user_id.into(),
                event_type: UserEventTypeDTO::UserNameUpdated,
                user: None,
                user_name: Some(user_name.value()),
                guitar: None,
            },
            UserEvent::UserGuitarAdded(user_id, guitar) => TestUserEventDTO {
                user_id: user_id.into(),
                event_type: UserEventTypeDTO::UserGuitarAdded,
                user: None,
                user_name: None,
                guitar: Some(guitar.into()),
            },
        }
    }

    fn try_from_dto(
        model: <TestUserEventDTOManager as StreamModel>::Data,
    ) -> Result<Self, crate::repository::redis::versioned_event::Error>
    where
        Self: Sized,
    {
        match model.event_type {
            UserEventTypeDTO::UserAdded => match model.user {
                Some(user) => Ok(UserEvent::UserAdded(user.into())),
                None => Err(crate::repository::redis::versioned_event::Error::FromDTO(
                    format!(
                        "Redis UserEventDTO invalid: missing Some(User), {:?}",
                        model
                    ),
                )),
            },
            UserEventTypeDTO::UserNameUpdated => match model.user_name {
                Some(user_name) => Ok(UserEvent::UserNameUpdated(
                    model.user_id,
                    UserName::try_from(user_name).map_err(|e| {
                        crate::repository::redis::versioned_event::Error::FromDTO(format!(
                            "Redis UserEventDTO invalid: {:?}",
                            e
                        ))
                    })?,
                )),
                None => Err(crate::repository::redis::versioned_event::Error::FromDTO(
                    format!(
                        "Redis UserEventDTO invalid: missing Some(UserName), {:?}",
                        model
                    ),
                )),
            },
            UserEventTypeDTO::UserGuitarAdded => match model.guitar {
                Some(guitar) => Ok(UserEvent::UserGuitarAdded(model.user_id, guitar.into())),
                None => Err(crate::repository::redis::versioned_event::Error::FromDTO(
                    format!(
                        "Redis UserEventDTO invalid: missing Some(Guitar), {:?}",
                        model
                    ),
                )),
            },
        }
    }
}
