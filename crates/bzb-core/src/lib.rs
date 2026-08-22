//! Shared library for the busybee CLI and monitor.
//!
//! Wraps pueue-lib with the narrow slice of functionality busybee needs:
//! connecting to the daemon, ensuring the `busybee` group exists, building
//! add-requests, and pure helpers (color envs, exit-code mapping, queue
//! snapshots).

pub mod classify;
pub mod client;
pub mod config;
pub mod daemon;
pub mod enqueue;
pub mod env;
pub mod errors;
pub mod exit_code;
pub mod group;
pub mod jobserver;
pub mod kill;
pub mod log;
pub mod protocol;
pub mod scheduler;
pub mod status;
pub mod wait;
