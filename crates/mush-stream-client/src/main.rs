//! `mush-stream-client` — receives video, decodes, displays, sends gamepad input.
//!
//! Milestone 1 stub. Populated starting at milestone 4.

// The Result return type matches the shape main() will have once M4 wires up
// network/decode/display, all of which are fallible.
#[allow(clippy::unnecessary_wraps)]
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    tracing::info!("mush-stream-client placeholder — implementation begins at milestone 4");
    Ok(())
}
