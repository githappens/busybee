use thiserror::Error;

#[derive(Debug, Error)]
pub enum BusybeeError {
    #[error("pueued is not running and auto-start failed: {context}")]
    DaemonUnreachable { context: String },

    #[error("pueued rejected our request: {0}")]
    EnqueueRejected(String),

    #[error("pueue-lib I/O error: {0}")]
    Network(#[from] std::io::Error),

    #[error("task {task_id} finished with error: {message}")]
    TaskErrored { task_id: usize, message: String },

    #[error("unexpected response from pueued: {0}")]
    UnexpectedResponse(String),

    #[error("{0}")]
    Other(String),
}

/// Recommended process exit code for a given error.
pub fn exit_code_for(err: &BusybeeError) -> i32 {
    match err {
        BusybeeError::DaemonUnreachable { .. } | BusybeeError::EnqueueRejected(_) => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_unreachable_maps_to_exit_2() {
        let e = BusybeeError::DaemonUnreachable { context: "no socket".into() };
        assert_eq!(exit_code_for(&e), 2);
    }

    #[test]
    fn other_maps_to_exit_1() {
        let e = BusybeeError::Other("oops".into());
        assert_eq!(exit_code_for(&e), 1);
    }
}
