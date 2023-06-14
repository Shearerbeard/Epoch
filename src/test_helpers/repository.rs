use core::time;
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    thread,
};

use assert_matches::assert_matches;
use futures::{
    future::{self, BoxFuture},
    FutureExt,
};

use crate::{
    decider::Event,
    repository::{
        event::VersionedEventRepositoryWithStreams, state::VersionedStateRepository,
        RepositoryVersion,
    },
    strategies::{LoadDecideAppend, LoadDecideAppendError, StateFromEventRepository, StreamState},
    test_helpers::{
        deciders::user::{
            Guitar, User, UserCommand, UserDecider, UserDeciderCtx, UserId, UserName,
        },
        ValueType,
    },
};

use super::deciders::user::{UserDeciderState, UserEvent};

pub(crate) async fn test_versioned_event_repository_with_streams<
    'a,
    Err: Debug + Send + Sync + Debug,
    V: Eq + PartialEq + Debug,
>(
    mut event_repository: impl VersionedEventRepositoryWithStreams<
        'a,
        UserEvent,
        Err,
        Version = V,
        StreamId = String,
    >,
) {
    println!("RUNNING UNIVERSAL SPEC TEST FOR VersionedEventRepositoryWithStreams");
    let id_1 = "1".to_string();
    let id_2 = "2".to_string();

    let res: (Vec<UserEvent>, RepositoryVersion<V>) =
        event_repository.load(None).await.expect("loaded");
    assert_matches!(res, (v, _) if v == vec![] as Vec<UserEvent>);

    let events1 = vec![
        UserEvent::UserAdded(User::new(
            1 as UserId,
            UserName::try_from("Mike").expect("Name is valid"),
        )),
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
        UserEvent::UserAdded(User::new(
            2,
            UserName::try_from("Stella").expect("Name is valid"),
        )),
        UserEvent::UserNameUpdated(
            2 as UserId,
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

    let events_combined: Vec<UserEvent> = events1.into_iter().chain(events2.into_iter()).collect();
    assert_matches!(res, Ok((v, RepositoryVersion::Exact(_))) if v == events_combined);

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

pub(crate) async fn test_versioned_state_repository<'a, Err: Debug + Send + Sync>(
    mut state_repository: impl VersionedStateRepository<'a, UserDeciderState, Err, Version = usize>,
) {
    let new_state = UserDeciderState::new(HashMap::from([(
        1,
        User::new(1, UserName::try_from("Mike").expect("valid")),
    )]));

    let version = RepositoryVersion::Exact(0);
    println!("Saving: state={:?}, version={:?}", &new_state, &version);
    let _ = state_repository
        .save(&version, &new_state)
        .await
        .expect("Success");
    println!("State saved");

    assert_eq!(
        state_repository.reify().await.expect("Success"),
        (new_state.to_owned(), RepositoryVersion::Exact(1))
    );

    let res = state_repository.save(&version, &new_state).await;
    assert_matches!(res, Err(_));
}

pub(crate) async fn test_versioned_event_repository_with_streams_occ<
    'a,
    Err: Debug + Send + Sync + Debug,
    V: Eq + PartialEq + Debug + Send + Sync,
>(
    mut event_repository: impl VersionedEventRepositoryWithStreams<'a, UserEvent, Err, Version = V, StreamId = String>
        + Send
        + Sync
        + Clone,
) {
    let ctx = UserDeciderCtx::new();

    let cmd1 = UserCommand::AddUser("Mike".to_string());

    let evts = UserDecider::execute(
        UserDeciderState::default(),
        &mut event_repository,
        &StreamState::New,
        &ctx,
        &cmd1,
        None,
    )
    .await
    .expect("command_succeeds");

    let first_id = evts.first().unwrap().get_id();

    assert_matches!(
        evts.first().expect("one event"),
        UserEvent::UserAdded(User { id, name, .. }) if (&first_id == id) && (name.value() == "Mike".to_string())
    );

    let state = UserDeciderState::load_by_id(
        UserDeciderState::default(),
        &event_repository,
        &first_id.to_string(),
    )
    .await
    .expect("state is loaded");

    assert_matches!(
        state,
        UserDeciderState { users } if users == HashMap::from([(first_id.clone(), User::new(first_id, UserName::try_from("Mike".to_string()).unwrap()))])
    );

    let guitars = vec![
        Guitar {
            brand: "Ibanez".to_string(),
        },
        Guitar {
            brand: "Gibson".to_string(),
        },
        Guitar {
            brand: "Fender".to_string(),
        },
        Guitar {
            brand: "Eastman".to_string(),
        },
        Guitar {
            brand: "Meyones".to_string(),
        },
        Guitar {
            brand: "PRS".to_string(),
        },
        Guitar {
            brand: "Yamaha".to_string(),
        },
        Guitar {
            brand: "Benedetto".to_string(),
        },
        Guitar {
            brand: "Strandberg".to_string(),
        },
    ];

    let futures = guitars
        .iter()
        .cloned()
        .map(|g| add_guitar(event_repository.clone(), first_id.clone(), g).boxed())
        .collect::<Vec<BoxFuture<()>>>();

    future::join_all(futures).await;

    thread::sleep(time::Duration::from_secs(1));

    let state = UserDeciderState::load_by_id(
        UserDeciderState::default(),
        &event_repository,
        &first_id.to_string(),
    )
    .await
    .expect("state is loaded");

    assert_eq!(
        state.users.get(&first_id).unwrap().guitars,
        HashSet::from_iter(guitars.iter().cloned())
    );
}

async fn add_guitar<
    'a,
    Err: Debug + Send + Sync + Debug,
    V: Eq + PartialEq + Debug + Send + Sync,
>(
    mut event_repository: impl VersionedEventRepositoryWithStreams<'a, UserEvent, Err, Version = V, StreamId = String>
        + Send
        + Sync,
    user_id: UserId,
    guitar: Guitar,
) {
    let ctx = UserDeciderCtx::new();

    println!("Adding Guitar {:?} for user {}", &guitar.brand, &user_id);

    let cmd = UserCommand::AddGuitar(user_id, guitar.to_owned());

    let res = UserDecider::execute(
        UserDeciderState::default(),
        &mut event_repository,
        &StreamState::Existing(user_id.to_string()),
        &ctx,
        &cmd,
        None,
    )
    .await;

    println!(
        "Result for Guitar {:?} for user {}: {:?}",
        &guitar.brand, &user_id, res
    );
}
