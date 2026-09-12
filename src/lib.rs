//! FoxIR library target.
//!
//! Declares the same module tree as the binary (`src/main.rs`) so that
//! external integration tests under `tests/` can link the crate's internals
//! (T6.8: gate aggregation). The binary keeps its own copy of these modules
//! and is left untouched; both compile the identical source.
#![allow(dead_code)]

pub mod agent;
pub mod callbacks;
pub mod checkpoint;
pub mod config;
pub mod crypto;
pub mod forensics;
pub mod context;
pub mod error;
pub mod deep_memory;
pub mod memory_migrate;
pub mod shallow_memory;
pub mod context_arbiter;
pub mod turn_decision;
pub mod event_log;
pub mod external_tools;
pub mod heartbeat;
pub mod knowledge;
pub mod interject;
pub mod log;
pub mod managed;
pub mod memory;
pub mod orch_selftest;
pub mod model;
pub mod model_store;
pub mod permission;
pub mod policy;
pub mod runner;
pub mod scheduler;
pub mod server;
pub mod session;
pub mod skill;
pub mod sop;
pub mod security;
pub mod tool;
pub mod value;
pub mod web;

