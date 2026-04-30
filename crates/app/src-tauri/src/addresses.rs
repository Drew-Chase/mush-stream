//! Local network address enumeration for the Host page's "Share
//! address" card.

use std::net::IpAddr;

use serde::Serialize;
use tauri::State;

use crate::configs::current_listen_port;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalAddress {
    pub kind: AddressKind,
    pub ip: String,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AddressKind {
    Tailscale,
    Lan,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareAddresses {
    /// First-pick share string (Tailscale if present, else first LAN).
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

    let mut addresses: Vec<LocalAddress> = match local_ip_address::list_afinet_netifas() {
        Ok(v) => v
            .into_iter()
            .filter_map(|(_iface, ip)| classify(ip, port))
            .collect(),
        Err(_) => Vec::new(),
    };

    // Tailscale first, then LAN. Within a kind, sort by IP for stability.
    addresses.sort_by(|a, b| {
        let kind_rank = |k: AddressKind| match k {
            AddressKind::Tailscale => 0,
            AddressKind::Lan => 1,
        };
        kind_rank(a.kind)
            .cmp(&kind_rank(b.kind))
            .then_with(|| a.ip.cmp(&b.ip))
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

/// Returns `Some(LocalAddress)` if the IP is one we'd want to surface
/// to the user (Tailscale or normal LAN). Loopback, link-local, and
/// IPv6 are dropped — the host listens on UDP, and the design's share
/// card only has space for the practical IPv4 candidates.
fn classify(ip: IpAddr, port: u16) -> Option<LocalAddress> {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() {
                return None;
            }
            let octets = v4.octets();
            // Tailscale CGNAT range: 100.64.0.0/10
            let is_tailscale = octets[0] == 100 && (64..128).contains(&octets[1]);
            // Conventional private ranges. Anything else (public IPs assigned
            // directly to an interface) gets bucketed as "Lan" for now.
            let is_private = v4.is_private();
            if !is_tailscale && !is_private {
                return None;
            }
            Some(LocalAddress {
                kind: if is_tailscale {
                    AddressKind::Tailscale
                } else {
                    AddressKind::Lan
                },
                ip: v4.to_string(),
                port,
            })
        }
        IpAddr::V6(_) => None,
    }
}
