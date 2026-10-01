//! Shared library for busybee and bzbd: the pueue-lib client slice, the bzbd
//! protocol and client, and the pure classify/scheduler/config logic.

pub mod classify;
pub mod client;
pub mod config;
pub mod daemon;
pub mod enqueue;
pub mod errors;
pub mod exit_code;
pub mod group;
pub mod jobserver;
pub mod kill;
pub mod log;
pub mod nest;
pub mod protocol;
pub mod scheduler;
pub mod wait;
