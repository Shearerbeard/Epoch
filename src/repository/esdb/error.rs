use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error{
    #[error("ESDB Error {0}")]
    ESDBGeneral(eventstore::Error),
    #[error("Error reading stream: {0}")]
    ReadStream(eventstore::Error),
    #[error("Could not deserialize event {0}")]
    DeserializeEvent(serde_json::Error),
    #[error("Could not serialize event {0}")]
    SerializeEventDataPayload(serde_json::Error),
    #[error("Could not write to stream {0}: {1}")]
    WriteStream(String, eventstore::Error),
}