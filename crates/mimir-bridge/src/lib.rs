//! Universal assistant connectors: MCP server, local HTTP/SSE API, and `mimir connect`.

pub mod connect;
pub mod http;
pub mod mcp;

pub use mimir_core::MimirConfig;
