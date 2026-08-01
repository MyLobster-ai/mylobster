mod auth;
mod chat;
mod client;
pub mod connect_policy;
mod protocol;
pub mod routes;
mod server;
mod websocket;

pub mod artifacts;
pub mod boot_ledger;
pub mod channels_rpc;
pub mod config_rpc;
pub mod delivery_recovery;
pub mod diagnostics;
pub mod dispatch;
pub mod health;
pub mod method_registry;
pub mod restart;
pub mod sessions_rpc;
pub mod startup;
pub mod stream_frames;
pub mod system_rpc;
pub mod tools_invoke;
pub mod transcript_api;
pub mod trust;

pub use auth::*;
pub use client::*;
pub use protocol::*;
pub use server::*;

/// Re-export model listing helper for HTTP routes.
pub fn websocket_models(provider: &str) -> Vec<serde_json::Value> {
    websocket::get_provider_models(provider)
}
