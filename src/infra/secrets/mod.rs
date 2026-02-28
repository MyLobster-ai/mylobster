//! External secrets management (v2026.2.26).
//!
//! Three-phase workflow for resolving secret references in configuration:
//! 1. **Audit** — scan config for `$ENV{...}`, `$EXEC{...}`, `$SOPS{...}` refs
//! 2. **Configure** — resolve refs through providers
//! 3. **Apply** — inject resolved values into runtime config
//!
//! Ported from OpenClaw `src/infra/secrets/`.

pub mod env_provider;
pub mod exec_provider;
pub mod reload;
pub mod resolver;
pub mod snapshot;
pub mod sops_provider;
pub mod types;

pub use resolver::resolve_all_secrets;
pub use snapshot::{SecretSnapshot, SecretWorkflow};
pub use types::{SecretProvider, SecretRef, SecretRefKind, SecretResolution};
