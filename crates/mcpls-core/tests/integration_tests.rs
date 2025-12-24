//! Integration tests for mcpls-core.

mod common;
mod e2e;
mod integration;

// Re-export the macro for tests
pub use crate::common::test_utils::rust_analyzer_available;
