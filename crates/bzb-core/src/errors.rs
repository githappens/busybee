use thiserror::Error;

#[derive(Debug, Error)]
pub enum BusybeeError {
    /// Raised for both daemons, so `context` names the one that failed.
    #[error("{context}")]
    DaemonUnreachable { context: String },

    #[error("pueued rejected our request: {0}")]
    EnqueueRejected(String),

    #[error("bzbd rejected our request: {0}")]
    Rejected(String),

    #[error("unexpected response from pueued: {0}")]
    UnexpectedResponse(String),

    #[error("bzbd protocol error: {0}")]
    Protocol(String),

    #[error("{0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variant covers both daemons, so the message must not name one.
    #[test]
    fn daemon_unreachable_shows_the_context_verbatim() {
        let e = BusybeeError::DaemonUnreachable {
            context: "cannot connect to bzbd at /tmp/bzbd.sock".into(),
        };
        assert_eq!(e.to_string(), "cannot connect to bzbd at /tmp/bzbd.sock");
    }
}
