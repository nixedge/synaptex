/// DHCP static reservation management via the Kea Control Agent.
///
/// Kea exposes a JSON-over-HTTP or JSON-over-Unix-socket control API.
/// This module will send `reservation-add` / `reservation-del` commands
/// to the `kea-dhcp4` daemon.
///
/// # Current state
/// Stub — returns `Ok(())` for all operations.  Implementation will be
/// added once the router crate is deployed and the Kea socket path is
/// configured.
///
/// # References
/// - https://kea.readthedocs.io/en/latest/arm/ctrl-channel.html
/// - Command: `{ "command": "reservation-add", "service": ["dhcp4"], "arguments": { ... } }`

use anyhow::Result;

use synaptex_router_proto::DhcpReservation;

pub async fn add(reservation: &DhcpReservation) -> Result<()> {
    // TODO: send reservation-add to Kea control socket / agent
    tracing::warn!(
        mac      = %reservation.mac,
        ip       = %reservation.ip,
        hostname = %reservation.hostname,
        "dhcp: reservation-add not yet implemented",
    );
    Ok(())
}

pub async fn remove(mac: &str) -> Result<()> {
    // TODO: send reservation-del to Kea control socket / agent
    tracing::warn!(%mac, "dhcp: reservation-del not yet implemented");
    Ok(())
}

pub async fn list() -> Result<Vec<DhcpReservation>> {
    // TODO: send reservation-get-all to Kea
    Ok(vec![])
}
