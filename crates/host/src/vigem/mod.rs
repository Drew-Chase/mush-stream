//! Virtual Xbox 360 gamepad on the host, fed by client input packets via
//! ViGEmBus.
//!
//! Requires the ViGEmBus driver to be installed on the host (free, signed
//! driver from the ViGEm project). If `Client::connect` fails — usually
//! because the driver isn't installed — this module logs and the host
//! degrades gracefully: video keeps streaming, input is just dropped.

use mush_stream_common::protocol::input::{InputPacket, InputReceiver};
use thiserror::Error;
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

#[derive(Debug, Error)]
pub enum VirtualGamepadError {
    #[error("ViGEmBus client error: {0}")]
    Vigem(#[from] vigem_client::Error),
}

/// Owns a virtual Xbox 360 controller plugged into the host's ViGEmBus.
/// Drop unplugs the device.
pub struct VirtualGamepad {
    target: Xbox360Wired<Client>,
    /// Drops out-of-order input packets per the spec's "Drop on receive if
    /// sequence is older than latest seen" rule.
    receiver: InputReceiver,
}

impl VirtualGamepad {
    /// Connect to ViGEmBus, plug in a virtual Xbox 360 wired controller,
    /// and wait until Windows is ready to accept updates.
    pub fn connect() -> Result<Self, VirtualGamepadError> {
        let client = Client::connect()?;
        let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
        target.plugin()?;
        target.wait_ready()?;
        tracing::info!("ViGEm virtual Xbox 360 gamepad plugged in and ready");
        Ok(Self {
            target,
            receiver: InputReceiver::new(),
        })
    }

    /// Apply one input packet to the virtual gamepad. Older-sequence
    /// packets are dropped silently.
    pub fn apply(&mut self, packet: InputPacket) -> Result<(), VirtualGamepadError> {
        let Some(p) = self.receiver.ingest(packet) else {
            return Ok(());
        };

        // The client packs button bits using the same layout as
        // vigem_client::XButtons (and Microsoft's XINPUT_GAMEPAD), so the
        // u16 lower bits drop straight in.
        let buttons_u16 = u16::try_from(p.buttons & 0xFFFF).unwrap_or(0);
        let state = XGamepad {
            buttons: XButtons::from(buttons_u16),
            left_trigger: p.triggers.0,
            right_trigger: p.triggers.1,
            thumb_lx: p.left_stick.0,
            thumb_ly: p.left_stick.1,
            thumb_rx: p.right_stick.0,
            thumb_ry: p.right_stick.1,
        };
        self.target.update(&state)?;
        Ok(())
    }

    /// How many input packets the receiver has accepted (excluding
    /// drops). Useful for shutdown logging.
    pub fn accepted(&self) -> u64 {
        self.receiver.accepted
    }

    /// How many input packets were dropped as stale.
    pub fn dropped_old(&self) -> u64 {
        self.receiver.dropped_old
    }
}
