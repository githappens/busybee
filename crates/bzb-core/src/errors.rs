use thiserror::Error;

#[derive(Debug, Error)]
pub enum BusybeeError {
    /// Raised for both daemons, so `context` names the one that failed.
    #[error("{context}")]
    DaemonUnreachable { context: String },

    #[error("pueued rejected our request: {0}")]
    EnqueueRejected(String),

    /// The bzbd counterpart: same shape, different daemon in the message.
    #[error("bzbd rejected our request: {0}")]
    Rejected(String),

    #[error("pueue-lib I/O error: {0}")]
    Network(#[from] std::io::Error),

    #[error("task {task_id} finished with error: {message}")]
    TaskErrored { task_id: usize, message: String },

    #[error("unexpected response from pueued: {0}")]
    UnexpectedResponse(String),

    #[error("bzbd protocol error: {0}")]
    Protocol(String),

    #[error("{0}")]
    Other(String),
}

/// Recommended process exit code for a given error.
pub fn exit_code_for(err: &BusybeeError) -> i32 {
    match err {
        BusybeeError::DaemonUnreachable { .. }
        | BusybeeError::EnqueueRejected(_)
        | BusybeeError::Rejected(_) => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_unreachable_maps_to_exit_2() {
        let e = BusybeeError::DaemonUnreachable {
            context: "no socket".into(),
        };
        assert_eq!(exit_code_for(&e), 2);
    }

    /// The variant covers both daemons, so the context names which one failed
    /// and the wording must not contradict it.
    #[test]
    fn daemon_unreachable_shows_the_context_verbatim() {
        let e = BusybeeError::DaemonUnreachable {
            context: "cannot connect to bzbd at /tmp/bzbd.sock".into(),
        };
        assert_eq!(e.to_string(), "cannot connect to bzbd at /tmp/bzbd.sock");
    }

    #[test]
    fn other_maps_to_exit_1() {
        let e = BusybeeError::Other("oops".into());
        assert_eq!(exit_code_for(&e), 1);
    }
}
