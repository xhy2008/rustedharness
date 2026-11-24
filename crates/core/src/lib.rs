//! RustedHarness core: provider, agent loop, tools and code intelligence.

pub mod agent;
pub mod cache;
pub mod compact;
pub mod config;
pub mod discovery;
pub mod error;
pub mod hashline;
pub mod intel;
pub mod lsp;
pub mod mcp;
pub mod paths;
pub mod proc;
pub mod protocol;
pub mod provider;
mod responses;
pub mod session;
pub mod shell;
pub mod syntax;
pub mod token;
pub mod tools;
pub mod web;

pub use config::Config;
pub use error::{HarnessError, Result};
pub use protocol::{HarnessCommand, HarnessEvent};

/// Version reported by the CLI and the protocol handshake.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
