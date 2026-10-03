//! Saga identity and the canonical key minted for each reaction.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::batch::StreamRef;
use super::feed::FeedPosition;

/// Non-empty saga identity, shared by the runner's group and outbox stream.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SagaId(String);

impl SagaId {
    /// Reject an empty saga id at construction.
    pub fn new(id: impl Into<String>) -> Result<Self, EmptySagaId> {
        let id = id.into();
        if id.is_empty() {
            Err(EmptySagaId)
        } else {
            Ok(Self(id))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SagaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SagaId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A saga id was constructed with the empty string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a saga id must not be empty")]
pub struct EmptySagaId;

/// A zero-based index into one `react` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ReactionIndex(u64);

impl ReactionIndex {
    pub(crate) fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Identifies one reaction by saga, source stream, committed feed
/// position, and reaction index. Feed positions are stable on replay.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct IntentKey {
    saga: SagaId,
    source: StreamRef,
    position: FeedPosition,
    index: ReactionIndex,
}

impl IntentKey {
    /// Mint the key for one reaction of one delivered source event.
    /// The runner is the only caller.
    pub(crate) fn mint(
        saga: &SagaId,
        source: StreamRef,
        position: FeedPosition,
        index: ReactionIndex,
    ) -> Self {
        Self {
            saga: saga.clone(),
            source,
            position,
            index,
        }
    }

    /// Escape `\` and `/` within components, then join them with `/`.
    /// Distinct structured keys cannot render to the same string.
    pub fn render(&self) -> RenderedIntentKey {
        RenderedIntentKey::from_rendered(format!(
            "{}/{}/{}/{}/{}",
            escape_key_component(self.saga.as_str()),
            escape_key_component(self.source.category()),
            escape_key_component(self.source.key()),
            self.position.get(),
            self.index.get(),
        ))
    }
}

fn escape_key_component(component: &str) -> String {
    component.replace('\\', "\\\\").replace('/', "\\/")
}

/// Owned, serialized identity carried across event storage and the
/// effect port. The runner mints canonical keys; stored records
/// deserialize them without parsing or validating their components.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RenderedIntentKey(String);

impl RenderedIntentKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wrap a canonical key minted by the runner. Stored keys are
    /// deserialized through the same transparent representation.
    pub(crate) fn from_rendered(rendered: String) -> Self {
        Self(rendered)
    }
}

impl fmt::Display for RenderedIntentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RenderedIntentKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
