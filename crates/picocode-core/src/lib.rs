//! picocode-core: the UI-independent heart of picocode.
//!
//! Everything a front end needs to run the agent lives here — configuration
//! and permission rules, the rig agent worker, the built-in tools, the
//! approval hook, and session/state persistence. Front ends (the TUI, or a
//! future GUI) talk to it exclusively through [`event::AgentEvent`] /
//! [`event::WorkerCmd`] channels and the plain data types in [`transcript`];
//! nothing in this crate depends on a rendering library.

pub mod agent;
pub mod approval;
pub mod attachment;
pub mod backend;
pub mod clipboard;
pub mod command;
pub mod config;
pub mod context;
pub mod event;
pub mod git;
pub mod history;
pub mod mcp;
pub mod models;
pub mod report;
pub mod sandbox;
pub mod session;
pub mod speed;
pub mod state;
pub mod steer;
pub mod tools;
pub mod transcript;
pub mod undo;
