use epoch::decider::{Decider, Event, Evolver};

#[derive(Debug)]
enum CounterEvent {
    Incremented,
}

impl Event for CounterEvent {
    type EntityId = ();

    fn event_type(&self) -> String {
        "Incremented".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

struct Counter;

impl Evolver for Counter {
    type State = u64;
    type Evt = CounterEvent;

    fn evolve(state: u64, event: &CounterEvent) -> u64 {
        match event {
            CounterEvent::Incremented => state + 1,
        }
    }
}

impl Decider for Counter {
    type Cmd = ();
    type Err = std::convert::Infallible;

    fn decide(_state: &u64, _cmd: &()) -> Result<Vec<CounterEvent>, Self::Err> {
        Ok(vec![CounterEvent::Incremented])
    }
}

fn main() {
    let state = 0;
    let events = Counter::decide(&state, &()).unwrap();
    let next = events.iter().fold(state, Counter::evolve);
    assert_eq!(next, 1);
    println!("{next}");
}
