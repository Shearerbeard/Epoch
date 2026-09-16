//! The saga identity family (ADR 0010's outbox-saga section): the
//! saga id, the deterministic intent key minted per reaction, and the
//! key's canonical rendered form. Rendering is injective by the
//! documented escaping rule, so a storage-level duplicate rejection
//! can never be a false positive.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::batch::StreamRef;
use super::feed::FeedPosition;

/// A saga's identity: the runner's consumer-group name, the saga's
/// outbox stream key, and the first component of every intent key the
/// runner mints for it. Non-empty, because an anonymous saga cannot be
/// told apart from a forgotten argument.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SagaId(String);

impl SagaId {
    /// Parse a saga id; the empty string is not one.
    pub fn new(id: impl Into<String>) -> Result<Self, EmptySagaId> {
        let id = id.into();
        if id.is_empty() {
            Err(EmptySagaId)
        } else {
            Ok(Self(id))
        }
    }

    /// The id as text.
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

/// A reaction's position within one `react` output: the index that
/// distinguishes two reactions of the same source event from each
/// other. Zero-based, matching the vector the saga returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ReactionIndex(u64);

impl ReactionIndex {
    pub(crate) fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The zero-based position.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// The deterministic identity of one reaction append (the card's
/// reaction-identity pin): the saga, the source event's stream, the
/// source event's committed position, and the reaction's index in that
/// event's `react` output. A redelivered source event re-mints exactly
/// the same keys, which is what makes storage's duplicate rejection a
/// no-op signal rather than a failure.
///
/// The spec's "source sequence" is realized as the source entry's
/// committed-log position: the feed delivers positions, not per-stream
/// sequences, and the position is unique per source event and stable
/// under redelivery.
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

    /// Render the key to its canonical envelope form. The rule: inside
    /// every component `\` renders as `\\` and `/` as `\/`, then the
    /// components join with `/` as `saga/category/key/position/index`.
    /// Escaping makes the rendering injective: two distinct keys never
    /// render equal, so a duplicate rejection can never be a false
    /// positive.
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

/// Escape one intent-key component under the documented rule.
fn escape_key_component(component: &str) -> String {
    component.replace('\\', "\\\\").replace('/', "\\/")
}

/// An intent key in its canonical rendered form: the string that rides
/// the `intent` envelope key and the outbox outcome payloads. Compared
/// bytewise, never parsed - the structured form is `IntentKey`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RenderedIntentKey(String);

impl RenderedIntentKey {
    /// The rendered text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wrap an already-rendered key. `IntentKey::render` is the only
    /// production source; stored facts are trusted at the read
    /// boundary, as any stored payload is.
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
