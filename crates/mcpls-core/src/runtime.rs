//! Process-level runtime of the server: everything `serve_with` drives after
//! its configuration is resolved.
//!
//! - [`pump`] drains each LSP server's notifications into the shared cache and
//!   the subscribed MCP sessions.
//! - [`startup`] starts the configured servers in the background, registers
//!   each as it settles, and supervises the pumps.
//! - [`untrusted`] plans which servers start, refusing and hardening them in
//!   an untrusted workspace.
//! - [`shutdown`](mod@shutdown) runs the post-transport cleanup sequence.
//!
//! Lower layers (`bridge`, `lsp`, `mcp`) must not depend on this module except
//! through the narrow wiring traits they define themselves, such as
//! [`NotificationWiring`](crate::bridge::NotificationWiring).

pub mod pump;
pub mod shutdown;
pub mod startup;
pub mod untrusted;

pub use shutdown::shutdown;
pub use startup::spawn_lsp_servers_background;
pub use untrusted::{StartPlan, plan_server_starts};
