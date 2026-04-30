//! Local network address enumeration for the Host page's "Share
//! address" card.

use std::net::Ipv4Addr;
use std::time::Duration;

use serde::Serialize;
use tauri::State;

use crate::configs::current_listen_port;
use crate::state::AppState;

/// Public-IP probe endpoint. Plain-text response with just the IPv4
/// address. We deliberately use the HTTPS endpoint so a hostile
/// network can't trivially feed us a forged address.
const PUBLIC_IP_ENDPOINT: &str = "https://api.ipify.org";
const PUBLIC_IP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalAddress {
    pub kind: AddressKind,
    pub ip: String,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AddressKind {
    /// RFC1918 private-range IP discovered on a local network
    /// interface — what to share for same-network play.
    Lan,
    /// External / internet-facing IP discovered via a public lookup
    /// service. What to share for cross-internet play (combined with
    /// a port forward of the listen port, since the host's UDP
    /// socket is rarely directly reachable).
    Public,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareAddresses {
    /// First-pick share string. Public IP if we resolved one, else
    /// the LAN address. Falls back to `None` if neither was found
    /// (host has no network interfaces and the lookup failed).
    pub primary: Option<String>,
    pub addresses: Vec<LocalAddress>,
    pub upnp_enabled: bool,
}

#[tauri::command]
pub async fn host_addresses(
    state: State<'_, AppState>,
) -> Result<ShareAddresses, String> {
    let port = current_listen_port(&state).unwrap_or(9002);
    let upnp_enabled = crate::configs::current_upnp_enabled(&state).unwrap_or(false);

    let mut addresses: Vec<LocalAddress> = Vec::new();

    // Real LAN IP. `local_ip_address::local_ip()` opens a UDP socket
    // toward a public destination and reads the local SocketAddr —
    // that yields the IP of the interface that would actually carry
    // the traffic, ignoring WSL bridges, Hyper-V switches, VPN
    // tunnels, Docker networks, and any other RFC1918 cruft that
    // `list_afinet_netifas` would otherwise lump in.
    if let Some(lan_ip) = pick_lan_ip() {
        addresses.push(LocalAddress {
            kind: AddressKind::Lan,
            ip: lan_ip.to_string(),
            port,
        });
    }

    // Public IP comes from an external service rather than interface
    // enumeration — most home machines sit behind NAT and don't
    // expose their public address on any local interface. We `await`
    // it inside the command (3 s timeout); a failure is logged but
    // not surfaced as an error so the LAN row still renders.
    if let Some(public_ip) = fetch_public_ip().await {
        addresses.push(LocalAddress {
            kind: AddressKind::Public,
            ip: public_ip,
            port,
        });
    }

    // Public first (most useful for sharing across the internet),
    // then LAN. Stable for the UI's two-row layout.
    addresses.sort_by_key(|a| match a.kind {
        AddressKind::Public => 0,
        AddressKind::Lan => 1,
    });

    let primary = addresses
        .first()
        .map(|a| format!("{}:{}", a.ip, a.port));

    Ok(ShareAddresses {
        primary,
        addresses,
        upnp_enabled,
    })
}

/// Pick the IPv4 address of the interface that would carry outbound
/// internet traffic — i.e. the one with the default route. Returns
/// `None` if the lookup fails or returns an IPv6/loopback/non-private
/// address (the latter shouldn't happen for a typical NAT'd machine,
/// but we'd fall through to a Public row anyway).
fn pick_lan_ip() -> Option<Ipv4Addr> {
    let ip = local_ip_address::local_ip().ok()?;
    let std::net::IpAddr::V4(v4) = ip else { return None };
    if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() {
        return None;
    }
    Some(v4)
}

/// Probe a public-IP service. Returns `None` on any error (offline,
/// timeout, non-200, malformed response) so the caller can carry on
/// without a Public row in the share card.
async fn fetch_public_ip() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(PUBLIC_IP_TIMEOUT)
        .build()
        .ok()?;
    let response = client.get(PUBLIC_IP_ENDPOINT).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let text = response.text().await.ok()?;
    let trimmed = text.trim();
    // Sanity-check: parse as an IPv4 address before we hand it to
    // the frontend. Rejects HTML 404 pages and other surprises.
    trimmed.parse::<std::net::Ipv4Addr>().ok()?;
    Some(trimmed.to_string())
}
