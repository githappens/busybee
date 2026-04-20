//! Shared library for the busybee CLI and monitor.
//!
//! Wraps pueue-lib with the narrow slice of functionality busybee needs:
//! connecting to the daemon, ensuring the `busybee` group exists, building
//! add-requests, and pure helpers (color envs, exit-code mapping, queue
//! snapshots).

pub mod env;
pub mod exit_code;
pub mod status;
pub mod errors;
pub mod client;
pub mod group;
pub mod enqueue;
pub mod wait;
pub mod log;
