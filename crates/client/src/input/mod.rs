//! Gamepad polling on the client.
//!
//! gilrs is a sync crate; we run it in a dedicated std::thread that polls
//! at 250 Hz (4 ms cadence per project spec) and pushes [`InputCommand`]
//! values into the network task via tokio mpsc. The bit layout chosen for
//! `InputPacket::buttons` matches the host's `vigem-client` `XButtons`
//! constants so the host can `state.buttons.0 = packet.buttons as u16`
//! without remapping.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use gilrs::{Axis, Button, Gamepad, GamepadId, Gilrs};
use mush_stream_common::protocol::{control::ControlMessage, input::InputPacket};
use tokio::sync::mpsc;

/// One queued send to the host's input/control listener.
#[derive(Debug, Clone)]
pub enum InputCommand {
    Input(InputPacket),
    Control(ControlMessage),
}

/// One row in the gamepad-enumeration response surfaced to the
/// frontend. `id` is `usize::from(GamepadId)` narrowed to `u32` so it
/// crosses the Tauri JSON boundary cleanly; gilrs IDs are stable for
/// the lifetime of a process.
#[derive(Debug, Clone)]
pub struct GamepadInfo {
    pub id: u32,
    pub name: String,
    pub is_connected: bool,
}

/// 250 Hz cadence — 4000 µs between polls.
const POLL_PERIOD: Duration = Duration::from_micros(4_000);

/// Enumerate the gamepads gilrs currently sees. Spins up a fresh
/// `Gilrs` handle (cheap) and drains any pending init events so a
/// gamepad that was hot-plugged just before this call still shows up.
///
/// Returns an empty Vec when gilrs fails to initialize (common on
/// systems without XInput / evdev / IOKit). Errors are logged at
/// warn so the UI just shows "no gamepads detected" rather than
/// throwing.
pub fn list_gamepads() -> Vec<GamepadInfo> {
    let mut gilrs = match Gilrs::new() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "gilrs init failed during enumeration");
            return Vec::new();
        }
    };
    while gilrs.next_event().is_some() {}
    gilrs
        .gamepads()
        .map(|(id, gp)| {
            let raw: usize = id.into();
            GamepadInfo {
                id: u32::try_from(raw).unwrap_or(u32::MAX),
                name: gp.name().to_owned(),
                is_connected: gp.is_connected(),
            }
        })
        .collect()
}

/// Run a blocking gamepad-poll loop. Sends an [`InputCommand::Input`] every
/// 4 ms while a gamepad is connected. Exits when `shutdown` flips to true,
/// when the channel is closed, or when gilrs initialization fails.
///
/// `selected_id` filters to a specific gilrs gamepad id; `None`
/// preserves the original "first available" behavior. If a specific
/// id is set but isn't present (unplugged after selection), the loop
/// idles silently — re-plugging brings input back without a
/// reconnect.
#[allow(clippy::needless_pass_by_value)] // long-running thread entry; owns its inputs
#[allow(clippy::unnecessary_wraps)] // Result kept for future fallible paths
pub fn run_gamepad_loop(
    tx: mpsc::Sender<InputCommand>,
    shutdown: Arc<AtomicBool>,
    selected_id: Option<u32>,
) -> Result<()> {
    let mut gilrs = match Gilrs::new() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "gilrs init failed; gamepad passthrough disabled");
            return Ok(());
        }
    };
    tracing::info!(
        selected_id = ?selected_id,
        "gamepad polling at 250Hz; waiting for a connected pad"
    );

    let mut next_tick = Instant::now() + POLL_PERIOD;
    let mut sequence: u16 = 0;
    let mut last_pad_logged: Option<GamepadId> = None;

    while !shutdown.load(Ordering::Acquire) {
        // Drain events to advance gamepad state.
        while gilrs.next_event().is_some() {}

        let pad = pick_gamepad(&gilrs, selected_id);
        match (pad, last_pad_logged) {
            (Some((id, gp)), prev) if prev != Some(id) => {
                tracing::info!(name = gp.name(), id = ?id, "gamepad bound");
                last_pad_logged = Some(id);
            }
            (None, Some(_)) => {
                tracing::info!("gamepad unbound");
                last_pad_logged = None;
            }
            _ => {}
        }

        if let Some((_id, gamepad)) = pick_gamepad(&gilrs, selected_id) {
            let packet = build_input_packet(&gamepad, sequence);
            sequence = sequence.wrapping_add(1);
            match tx.try_send(InputCommand::Input(packet)) {
                // Full: network task can't keep up; drop. For 4 ms cadence,
                // dropping a tick is preferable to queueing latency.
                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }

        let now = Instant::now();
        if now < next_tick {
            std::thread::sleep(next_tick - now);
        }
        next_tick += POLL_PERIOD;
        // If we fell behind significantly (sleep granularity / GC pause /
        // device hotplug), don't try to "catch up" by bursting.
        if next_tick < Instant::now() {
            next_tick = Instant::now() + POLL_PERIOD;
        }
    }
    tracing::info!("gamepad loop exiting");
    Ok(())
}

/// Resolve `selected_id` against the current gilrs gamepad list.
/// `None` returns the first gamepad gilrs reports (matching the
/// pre-selection default). `Some(id)` looks up by id and returns
/// `None` when the chosen pad isn't currently connected.
fn pick_gamepad(
    gilrs: &Gilrs,
    selected_id: Option<u32>,
) -> Option<(GamepadId, Gamepad<'_>)> {
    match selected_id {
        None => gilrs.gamepads().next(),
        Some(want) => gilrs
            .gamepads()
            .find(|(id, _)| u32::try_from(usize::from(*id)).unwrap_or(u32::MAX) == want),
    }
}

/// Snapshot the gamepad's current state into an [`InputPacket`]. The
/// `buttons` u32's lower 16 bits use the same bit layout as
/// `vigem_client::XButtons` so the host can plug them in directly.
fn build_input_packet(gp: &Gamepad<'_>, sequence: u16) -> InputPacket {
    // ViGEm XButtons bit layout (and the Microsoft XINPUT_GAMEPAD layout):
    const UP: u32 = 0x0001;
    const DOWN: u32 = 0x0002;
    const LEFT: u32 = 0x0004;
    const RIGHT: u32 = 0x0008;
    const START: u32 = 0x0010;
    const BACK: u32 = 0x0020;
    const LTHUMB: u32 = 0x0040;
    const RTHUMB: u32 = 0x0080;
    const LB: u32 = 0x0100;
    const RB: u32 = 0x0200;
    const GUIDE: u32 = 0x0400;
    const A: u32 = 0x1000;
    const B: u32 = 0x2000;
    const X: u32 = 0x4000;
    const Y: u32 = 0x8000;

    let mut buttons: u32 = 0;
    let mut set = |cond: bool, bit: u32| {
        if cond {
            buttons |= bit;
        }
    };
    set(gp.is_pressed(Button::DPadUp), UP);
    set(gp.is_pressed(Button::DPadDown), DOWN);
    set(gp.is_pressed(Button::DPadLeft), LEFT);
    set(gp.is_pressed(Button::DPadRight), RIGHT);
    set(gp.is_pressed(Button::Start), START);
    set(gp.is_pressed(Button::Select), BACK);
    set(gp.is_pressed(Button::LeftThumb), LTHUMB);
    set(gp.is_pressed(Button::RightThumb), RTHUMB);
    // gilrs's LeftTrigger / RightTrigger are the shoulder bumpers (LB/RB);
    // LeftTrigger2 / RightTrigger2 are the analog triggers crossed
    // beyond their threshold.
    set(gp.is_pressed(Button::LeftTrigger), LB);
    set(gp.is_pressed(Button::RightTrigger), RB);
    set(gp.is_pressed(Button::Mode), GUIDE);
    set(gp.is_pressed(Button::South), A);
    set(gp.is_pressed(Button::East), B);
    set(gp.is_pressed(Button::West), X);
    set(gp.is_pressed(Button::North), Y);

    let to_i16 = |f: f32| (f.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
    let to_u8 = |f: f32| (f.clamp(0.0, 1.0) * 255.0).round() as u8;

    // Triggers: prefer the analog button_data value if available, fall
    // back to the Z axis (some platforms expose triggers there).
    let lt = gp
        .button_data(Button::LeftTrigger2)
        .map_or_else(|| gp.value(Axis::LeftZ).max(0.0), gilrs::ev::state::ButtonData::value);
    let rt = gp
        .button_data(Button::RightTrigger2)
        .map_or_else(|| gp.value(Axis::RightZ).max(0.0), gilrs::ev::state::ButtonData::value);

    InputPacket {
        buttons,
        left_stick: (
            to_i16(gp.value(Axis::LeftStickX)),
            to_i16(gp.value(Axis::LeftStickY)),
        ),
        right_stick: (
            to_i16(gp.value(Axis::RightStickX)),
            to_i16(gp.value(Axis::RightStickY)),
        ),
        triggers: (to_u8(lt), to_u8(rt)),
        sequence,
    }
}
