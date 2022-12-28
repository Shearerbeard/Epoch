use core::time;
use std::{fmt::Debug, thread};

use assert_matches::assert_matches;

use crate::{
    repository::{event::VersionedEventRepositoryWithStreams, RepositoryVersion},
    test_helpers::deciders::user::{User, UserId, UserName},
};

use super::deciders::user::UserEvent;

pub(crate) async fn test_versioned_event_repository_with_streams<'a, Err: Debug>(
    mut event_repository: impl VersionedEventRepositoryWithStreams<
        'a,
        UserEvent,
        Err,
        StreamId = String,
    >,
) {
    println!("RUNNING UNIVERSAL SPEC TEST FOR VersionedEventRepositoryWithStreams");
    let id_1 = "1".to_string();
    let id_2 = "2".to_string();

    let res: (Vec<UserEvent>, RepositoryVersion) =
        event_repository.load(None).await.expect("loaded");
    assert_matches!(res, (v, _) if v == vec![] as Vec<UserEvent>);

    let events1 = vec![
        UserEvent::UserAdded(User {
            id: 1,
            name: UserName::try_from("Mike").expect("Name is valid"),
        }),
        UserEvent::UserNameUpdated(
            1 as UserId,
            UserName::try_from("Mike2").expect("Name is valid"),
        ),
    ];

    let _ = event_repository
        .append(&RepositoryVersion::Any, &id_1, &events1)
        .await
        .expect("Successful append");

    let events2 = vec![
        UserEvent::UserAdded(User {
            id: 2,
            name: UserName::try_from("Stella").expect("Name is valid"),
        }),
        UserEvent::UserNameUpdated(
            1 as UserId,
            UserName::try_from("Stella2").expect("Name is valid"),
        ),
    ];

    let _ = event_repository
        .append(&RepositoryVersion::Any, &id_2, &events2)
        .await
        .expect("Successful append");

    // Crude but we need to wait for ESDB to catch up its "Categories" auto projection
    thread::sleep(time::Duration::from_secs(1));

    let res = event_repository.load(Some(&id_1)).await;
    assert_matches!(res, Ok((v, RepositoryVersion::Exact(_))) if v == events1);

    let res = event_repository.load(Some(&id_2)).await;
    assert_matches!(res, Ok((v, RepositoryVersion::Exact(_))) if v == events2);

    let res = event_repository.load(None).await;

    // let events_combined: Vec<UserEvent> = events1.into_iter().chain(events2.into_iter()).collect();
    // assert_matches!(res, Ok((v, RepositoryVersion::Exact(_))) if v == events_combined);

    let res = event_repository.load(Some(&id_1)).await;
    let version = res.unwrap().1;

    let new_events = vec![UserEvent::UserNameUpdated(
        1,
        UserName::try_from("Mike").expect("Name is valid"),
    )];

    let res = event_repository
        .append(&version, &id_1, &new_events)
        .await
        .expect("Success");

    let version = res.1;

    let (latest_events, _) = event_repository
        .load_from_version(&version, Some(&id_1))
        .await
        .expect("load success");

    assert_eq!(latest_events.first().unwrap(), new_events.first().unwrap());
}
