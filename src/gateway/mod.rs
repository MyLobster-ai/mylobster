mod auth;
mod chat;
mod client;
pub mod connect_policy;
mod protocol;
pub mod routes;
mod server;
mod websocket;

pub use auth::*;
pub use client::*;
pub use protocol::*;
pub use server::*;

/// Re-export model listing helper for HTTP routes.
pub fn websocket_models(provider: &str) -> Vec<serde_json::Value> {
    websocket::get_provider_models(provider)
}
