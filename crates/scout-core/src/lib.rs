//! scout-core: everything shared by every scout front-end (CLI today, MCP later).
//!
//! The crate owns the domain model and the *ports* (traits) that make storage
//! swappable. It never decides where repos or index files physically live; concrete
//! adapters (`scout-source-fs`, `scout-index-native`, ...) implement the ports and a
//! front-end wires them together.

pub mod catalog;
pub mod error;
pub mod indexer;
pub mod ports;
pub mod query;
pub mod search;
pub mod snapshots;
pub mod trigram;

pub use error::{Error, Result};
