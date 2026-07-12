//! Versioned provider plugin protocol over JSON-RPC/JSONL stdio.

pub mod client;
pub mod host;
pub mod manifest;
pub mod rpc;
pub mod schema;

pub use client::*;
pub use host::*;
pub use manifest::*;
pub use rpc::*;
pub use schema::*;
