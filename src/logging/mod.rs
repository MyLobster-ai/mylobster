pub mod redact;

pub use redact::{redact_text, REDACTED};

pub fn init() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("mylobster=info".parse().unwrap()),
        )
        .init();
}
