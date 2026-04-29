//! Test library for mcpls integration tests.
//!
//! This workspace contains intentional patterns for testing:
//! - Hover information on standard types
//! - Go-to-definition on custom types
//! - Find references on functions
//! - Diagnostics (intentional errors)

pub mod types;
pub mod functions;

use serde::{Deserialize, Serialize};

/// A sample struct for testing hover and definition.
///
/// This struct represents a user in the system.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct User {
    pub id: u64,
    pub name: String,
    pub email: String,
}

impl User {
    /// Creates a new user with the given ID, name, and email.
    pub fn new(id: u64, name: String, email: String) -> Self {
        Self { id, name, email }
    }
}

/// Intentional error for diagnostics testing.
///
/// This function contains an undefined variable to test
/// diagnostic reporting.
#[allow(dead_code)]
pub fn has_error() {
    let _x = undefined_variable;
}

/// Function with unused variable for warning testing.
#[allow(dead_code)]
pub fn has_warning() {
    let unused = 42;
    println!("Hello");
}

// --- e2e test surface: stable symbols used by ra_e2e test suite ---
use std::fmt;

/// Adds two integers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

/// Calls `add` — used for call hierarchy and reference tests.
pub fn caller() -> i32 {
    add(1, 2)
}

/// A simple point with two coordinates.
pub struct Point {
    /// X coordinate.
    pub x: f64,
    /// Y coordinate.
    pub y: f64,
}

impl fmt::Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.x, self.y)
    }
}

/// Target function for code-action e2e testing.
///
/// The assignment `let ca_var = 1` is missing a semicolon so that
/// rust-analyzer reliably offers an "add semicolon" quickfix here.
#[allow(dead_code)]
pub fn code_action_target() {
    let ca_var = 1
}
