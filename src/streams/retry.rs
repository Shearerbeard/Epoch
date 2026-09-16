//! The retry vocabulary the saga runner and the outbox executor
//! share (ADR 0010's outbox-saga section): a backoff schedule, a
//! nonzero attempt budget, and the policy that joins them. The zero
//! case fails at `RetryBudget::new`, so a policy always allows at
//! least one attempt.

use std::num::NonZeroU32;
use std::time::Duration;

use thiserror::Error;

/// A bounded exponential backoff schedule: each wait doubles from
/// `base`, never exceeding `cap`. Carries no attempt count - the
/// policy owns the count, so schedule and count can never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffSchedule {
    base: Duration,
    cap: Duration,
}

impl BackoffSchedule {
    /// A schedule doubling from `base`, capped at `cap`. A base above
    /// its cap is not a schedule.
    pub fn new(base: Duration, cap: Duration) -> Result<Self, BaseExceedsCap> {
        if base > cap {
            Err(BaseExceedsCap)
        } else {
            Ok(Self { base, cap })
        }
    }

    /// The first wait.
    pub fn base(&self) -> Duration {
        self.base
    }

    /// No wait exceeds this.
    pub fn cap(&self) -> Duration {
        self.cap
    }

    /// The waits, doubling from the base and capped: an unbounded
    /// stream; the caller's own count bounds it.
    pub fn delays(&self) -> impl Iterator<Item = Duration> {
        let (mut delay, cap) = (self.base, self.cap);
        std::iter::from_fn(move || {
            let current = delay;
            delay = delay.saturating_mul(2).min(cap);
            Some(current)
        })
    }
}

impl Default for BackoffSchedule {
    /// 50ms base, 2s cap - the shipped values gate A accepted.
    fn default() -> Self {
        Self {
            base: Duration::from_millis(50),
            cap: Duration::from_secs(2),
        }
    }
}

/// A backoff schedule whose base exceeds its cap was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("the backoff base exceeds its cap")]
pub struct BaseExceedsCap;

/// The nonzero attempt bound the policy is built on: the runner's
/// total tries for retryable aborts, and the executor's perform
/// attempts per intent before it parks TERMINAL-FAILED (derived
/// durably from the stream's own `Failed` records for the intent key,
/// so it survives executor crashes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryBudget(NonZeroU32);

impl RetryBudget {
    /// Parse a budget; zero is not a budget.
    pub fn new(raw: u32) -> Result<Self, ZeroBudget> {
        NonZeroU32::new(raw).map(Self).ok_or(ZeroBudget)
    }

    /// The attempt count.
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl Default for RetryBudget {
    /// The card's pinned default: 5 attempts.
    fn default() -> Self {
        Self(NonZeroU32::new(5).expect("5 is nonzero"))
    }
}

/// A retry budget of zero was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a retry budget must allow at least one attempt")]
pub struct ZeroBudget;

/// The in-memory retry policy the runner and the executor share: a
/// pre-validated budget of tries under a backoff schedule. The runner
/// applies it to retryable aborts (`TransactError::LockTimeout`) and
/// indeterminate failures (connection loss, deadlock), then
/// propagates; classification never happens inside the retry window,
/// the redelivery rule applies only after retryable aborts are
/// retried. The executor applies it per intent: under budget the
/// failing intent's `Failed` record lands and the cursor holds for
/// the backoff; at the budget the intent parks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    budget: RetryBudget,
    schedule: BackoffSchedule,
}

impl RetryPolicy {
    /// A policy of `budget` total tries under `schedule`. Infallible:
    /// the budget arrives pre-validated, the zero case having failed
    /// at [`RetryBudget::new`].
    pub fn new(budget: RetryBudget, schedule: BackoffSchedule) -> Self {
        Self { budget, schedule }
    }

    /// The attempt budget.
    pub fn budget(&self) -> RetryBudget {
        self.budget
    }

    /// Total tries, including the first.
    pub fn attempts(&self) -> u32 {
        self.budget.get()
    }

    /// The backoff schedule between tries.
    pub fn schedule(&self) -> BackoffSchedule {
        self.schedule
    }

    /// The waits between tries: `budget - 1` of them, doubling from
    /// the schedule's base and capped.
    pub fn delays(&self) -> impl Iterator<Item = Duration> {
        self.schedule
            .delays()
            .take((self.budget.get() - 1) as usize)
    }
}

impl Default for RetryPolicy {
    /// The default policy: 5 attempts under the default schedule -
    /// the shipped values gate A accepted.
    fn default() -> Self {
        Self {
            budget: RetryBudget::default(),
            schedule: BackoffSchedule::default(),
        }
    }
}
