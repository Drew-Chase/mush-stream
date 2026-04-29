//! Optional UPnP UDP port forwarding for the client.
//!
//! When `[network] enable_upnp = true` in `client.toml`, the client attempts
//! to map its `video_bind` UDP port through the local router at startup so
//! the host can reach it without the user having to manually forward ports.
//! Only useful when the user is *not* on Tailscale (or another VPN that
//! already provides a routable address).
//!
//! Best-effort: routers without UPnP, or with it disabled, will simply log
//! a warning and the client continues running.

use upnpc_rs::Protocol;

/// RAII guard for one UPnP UDP port forwarding entry. Drop unmaps the port.
pub struct UpnpForward {
    port: u16,
}

impl UpnpForward {
    /// Try to forward `port` through the local router. Returns `Some` on
    /// success, `None` on failure (with a warning logged).
    pub fn try_forward_udp(port: u16, description: &str) -> Option<Self> {
        match upnpc_rs::add_port(
            port,
            None,
            Protocol::UDP,
            None,
            Some(description.to_owned()),
            None,
        ) {
            Ok(()) => {
                tracing::info!(port, description, "UPnP UDP port forwarded");
                Some(Self { port })
            }
            Err(e) => {
                tracing::warn!(
                    port,
                    error = %e,
                    "UPnP forward failed; continuing without port mapping \
                    (router may not support UPnP, or it may be disabled)"
                );
                None
            }
        }
    }
}

impl Drop for UpnpForward {
    fn drop(&mut self) {
        match upnpc_rs::remove_port(self.port, Protocol::UDP) {
            Ok(()) => tracing::info!(port = self.port, "UPnP port unmapped"),
            Err(e) => tracing::warn!(
                port = self.port,
                error = %e,
                "UPnP unmap failed (router may have already cleared it)"
            ),
        }
    }
}
