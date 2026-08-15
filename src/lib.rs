//! Core contracts for the `codex-mux` terminal application.
//!
//! tmux remains the discovery and control boundary. Concrete tmux, process,
//! terminal, and configuration adapters are implemented by later feature
//! modules behind the ports defined here.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod cli;
pub mod domain;
pub mod error;
pub mod launch;
pub mod linux_process;
pub mod tmux;

pub use error::{MuxError, Result};
