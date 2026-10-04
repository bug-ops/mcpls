//! Latency benchmark harness for mcpls.
//!
//! Drives a real `mcpls` process over MCP stdio against a pinned repository and
//! a real language server, and records per-call latency with correctness
//! checks. See `docs/benchmarks.md` for methodology.

pub mod pin;
pub mod prepare;
pub mod probe;
pub mod process_tree;
pub mod report;
pub mod run;
pub mod scenario;
pub mod signals;
