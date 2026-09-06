// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Marc Hoffmann (b14ckyy)

// MAVLink Vehicle-Control Commands
// Fire a COMMAND_LONG / COMMAND_INT to the FC and wait for its COMMAND_ACK.
// Ref: https://mavlink.io/en/services/command.html
//
// Like the mission microprotocol (mission.rs), this works entirely through the MavlinkCommand
// channel — it never holds the AppState mutex during the exchange, so disconnect stays safe.
//
// MAVLink commands are NOT request→response like MSP: we send the command and the FC replies
// asynchronously with COMMAND_ACK (ACCEPTED / DENIED / TEMPORARILY_REJECTED / UNSUPPORTED / …).
// We register a receiver, send, then match the ACK by command id and resolve the blocking call.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use ::mavlink::ardupilotmega::{
    MavMessage, MavCmd, MavFrame, MavResult, MavParamType,
    COMMAND_LONG_DATA, COMMAND_INT_DATA, PARAM_SET_DATA,
};

use super::handler::MavlinkCommand;

/// The autopilot is conventionally component 1 (MAV_COMP_ID_AUTOPILOT1). ArduPilot and PX4 both
/// answer commands addressed to it.
const AUTOPILOT_COMPONENT: u8 = 1;

/// How long we wait for the COMMAND_ACK before giving up. Commands are cheap; the FC acks fast.
const ACK_TIMEOUT: Duration = Duration::from_secs(3);

/// Send a `COMMAND_LONG` and wait for its `COMMAND_ACK`. `params` are param1..param7.
pub fn send_command_long(
    cmd_tx: &mpsc::Sender<MavlinkCommand>,
    fc_sysid: u8,
    command: MavCmd,
    params: [f32; 7],
) -> Result<(), String> {
    let rx = register(cmd_tx)?;
    let msg = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
        target_system: fc_sysid,
        target_component: AUTOPILOT_COMPONENT,
        command,
        confirmation: 0,
        param1: params[0],
        param2: params[1],
        param3: params[2],
        param4: params[3],
        param5: params[4],
        param6: params[5],
        param7: params[6],
    });
    if let Err(e) = send(cmd_tx, msg) {
        unregister(cmd_tx);
        return Err(e);
    }
    let result = wait_for_ack(&rx, command, ACK_TIMEOUT);
    unregister(cmd_tx);
    result
}

/// Send a `COMMAND_INT` and wait for its `COMMAND_ACK`. Used for commands carrying a global
/// position (lat/lon as int32 × 1e7) — `DO_REPOSITION` — where COMMAND_LONG's f32 params would
/// lose coordinate precision. `params` are param1..param4; `x`/`y` are lat/lon × 1e7; `z` is alt (m).
#[allow(clippy::too_many_arguments)] // maps directly to COMMAND_INT's fixed field set
pub fn send_command_int(
    cmd_tx: &mpsc::Sender<MavlinkCommand>,
    fc_sysid: u8,
    frame: MavFrame,
    command: MavCmd,
    params: [f32; 4],
    x: i32,
    y: i32,
    z: f32,
) -> Result<(), String> {
    let rx = register(cmd_tx)?;
    let msg = MavMessage::COMMAND_INT(COMMAND_INT_DATA {
        target_system: fc_sysid,
        target_component: AUTOPILOT_COMPONENT,
        frame,
        command,
        current: 0,
        autocontinue: 0,
        param1: params[0],
        param2: params[1],
        param3: params[2],
        param4: params[3],
        x,
        y,
        z,
    });
    if let Err(e) = send(cmd_tx, msg) {
        unregister(cmd_tx);
        return Err(e);
    }
    let result = wait_for_ack(&rx, command, ACK_TIMEOUT);
    unregister(cmd_tx);
    result
}

/// Send a `COMMAND_INT`, returning the FC's raw result instead of a message. Same wire format as
/// `send_command_int`; used where the caller adapts to the refusal (see `reposition`).
#[allow(clippy::too_many_arguments)]
fn command_int_result(
    cmd_tx: &mpsc::Sender<MavlinkCommand>,
    fc_sysid: u8,
    frame: MavFrame,
    command: MavCmd,
    params: [f32; 4],
    x: i32,
    y: i32,
    z: f32,
) -> Result<MavResult, String> {
    let rx = register(cmd_tx)?;
    let msg = MavMessage::COMMAND_INT(COMMAND_INT_DATA {
        target_system: fc_sysid,
        target_component: AUTOPILOT_COMPONENT,
        frame,
        command,
        current: 0,
        autocontinue: 0,
        param1: params[0],
        param2: params[1],
        param3: params[2],
        param4: params[3],
        x,
        y,
        z,
    });
    if let Err(e) = send(cmd_tx, msg) {
        unregister(cmd_tx);
        return Err(e);
    }
    let result = wait_for_ack_result(&rx, command, ACK_TIMEOUT);
    unregister(cmd_tx);
    result
}

/// `DO_REPOSITION` with a frame fallback, because the two firmwares accept different frames.
///
/// ArduPilot takes `GLOBAL_RELATIVE_ALT`, which is what the altitude in the UI means (metres above
/// home), so that is tried first and is the only attempt ArduPilot ever sees. INAV's MAVLink port
/// accepts *only* `MAV_FRAME_GLOBAL` and answers anything else `MAV_RESULT_UNSUPPORTED`
/// (telemetry/mavlink.c, `handleIncoming_COMMAND_INT`: the relative-alt and terrain frames are
/// commented out). On that specific refusal we retry once with `GLOBAL` and an AMSL altitude.
///
/// `amsl_offset` converts the relative altitude to AMSL and comes from the vehicle's own telemetry
/// (`GLOBAL_POSITION_INT.alt - relative_alt`), not from a stored home altitude: INAV never sends
/// HOME_POSITION, so that is the only source available on this link. `None` means the offset is not
/// known yet, and then there is nothing honest to retry with.
#[allow(clippy::too_many_arguments)]
pub fn reposition(
    cmd_tx: &mpsc::Sender<MavlinkCommand>,
    fc_sysid: u8,
    params: [f32; 4],
    x: i32,
    y: i32,
    rel_alt: f32,
    amsl_offset: Option<f32>,
) -> Result<(), String> {
    let first = command_int_result(
        cmd_tx, fc_sysid, MavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT,
        MavCmd::MAV_CMD_DO_REPOSITION, params, x, y, rel_alt,
    )?;
    if first != MavResult::MAV_RESULT_UNSUPPORTED {
        return result_to_err(first);
    }
    let Some(offset) = amsl_offset else {
        log::warn!("DO_REPOSITION refused as UNSUPPORTED and no AMSL offset is known yet");
        return result_to_err(first);
    };
    let amsl = rel_alt + offset;
    log::info!(
        "DO_REPOSITION UNSUPPORTED with GLOBAL_RELATIVE_ALT; retrying as GLOBAL at {:.1} m AMSL",
        amsl,
    );
    let second = command_int_result(
        cmd_tx, fc_sysid, MavFrame::MAV_FRAME_GLOBAL,
        MavCmd::MAV_CMD_DO_REPOSITION, params, x, y, amsl,
    )?;
    result_to_err(second)
}

/// Set a single FC parameter (fire-and-forget `PARAM_SET`). Used for tunables that have no dedicated
/// command — e.g. the fixed-wing loiter radius (`WP_LOITER_RAD`). ArduPilot ignores `param_type` for
/// its REAL32 params; we don't wait for the PARAM_VALUE echo (non-critical, keeps it simple).
pub fn set_param(
    cmd_tx: &mpsc::Sender<MavlinkCommand>,
    fc_sysid: u8,
    name: &str,
    value: f32,
) -> Result<(), String> {
    let mut param_id = [0u8; 16];
    let bytes = name.as_bytes();
    let n = bytes.len().min(16);
    param_id[..n].copy_from_slice(&bytes[..n]);

    send(cmd_tx, MavMessage::PARAM_SET(PARAM_SET_DATA {
        target_system: fc_sysid,
        target_component: AUTOPILOT_COMPONENT,
        param_id: param_id.into(),
        param_value: value,
        param_type: MavParamType::MAV_PARAM_TYPE_REAL32,
    }))
}

/// Send an explicit RC_CHANNELS_OVERRIDE **release** frame for the channels we were controlling
/// (`controlled_us`: positional µs, non-zero = controlled), repeated `count` times at `interval`, to
/// hand control back on a *deliberate* disengage (ArduPilot). Per the MAVLink field semantics a value of
/// 0 (CH1–8) / 65534 (CH9–18) releases that channel back to the RC radio; channels we never touched stay
/// ignored. Fire-and-forget (RC stream messages have no ACK). The caller must first disable the normal
/// RC stream so the handler doesn't interleave live overrides with these release frames.
pub fn send_rc_release(
    cmd_tx: &mpsc::Sender<MavlinkCommand>,
    fc_sysid: u8,
    controlled_us: &[u16],
    count: u8,
    interval: Duration,
) -> Result<(), String> {
    let ch = crate::scheduler::rc_tx::release_channels(controlled_us);
    let msg = super::handler::rc_override_msg(fc_sysid, &ch);
    for i in 0..count {
        send(cmd_tx, msg.clone())?;
        if i + 1 < count {
            std::thread::sleep(interval);
        }
    }
    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn register(cmd_tx: &mpsc::Sender<MavlinkCommand>) -> Result<mpsc::Receiver<MavMessage>, String> {
    let (tx, rx) = mpsc::channel();
    cmd_tx.send(MavlinkCommand::RegisterCommandReceiver(tx))
        .map_err(|_| "MAVLink handler stopped".to_string())?;
    // Same tiny race guard as mission.rs: let the handler pick up the registration before we send.
    std::thread::sleep(Duration::from_millis(10));
    Ok(rx)
}

fn unregister(cmd_tx: &mpsc::Sender<MavlinkCommand>) {
    let _ = cmd_tx.send(MavlinkCommand::UnregisterCommandReceiver);
}

fn send(cmd_tx: &mpsc::Sender<MavlinkCommand>, msg: MavMessage) -> Result<(), String> {
    let (reply_tx, reply_rx) = mpsc::channel();
    cmd_tx.send(MavlinkCommand::SendMessage { msg, reply: reply_tx })
        .map_err(|_| "MAVLink handler stopped".to_string())?;
    reply_rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| "MAVLink send timed out".to_string())?
}

/// Wait for the COMMAND_ACK that matches `command` and return its raw result. `IN_PROGRESS` acks are
/// intermediate (some commands report progress) — keep waiting for the final result.
///
/// Separate from `wait_for_ack` because a caller may need to act on *which* refusal it got rather
/// than only on success: the reposition fallback below retries a different frame on UNSUPPORTED,
/// and matching on a human-readable error string to do that would be fragile.
fn wait_for_ack_result(
    rx: &mpsc::Receiver<MavMessage>,
    command: MavCmd,
    timeout: Duration,
) -> Result<MavResult, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("No COMMAND_ACK from FC (timed out)".into());
        }
        match rx.recv_timeout(remaining) {
            Ok(MavMessage::COMMAND_ACK(ack)) if ack.command == command => {
                if ack.result == MavResult::MAV_RESULT_IN_PROGRESS {
                    continue;
                }
                return Ok(ack.result);
            }
            Ok(_) => continue, // ack for a different command, or stray msg — keep waiting
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err("No COMMAND_ACK from FC (timed out)".into());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("MAVLink handler stopped".into());
            }
        }
    }
}

/// The message a given refusal shows the user. `ACCEPTED` is the only success.
fn result_to_err(result: MavResult) -> Result<(), String> {
    match result {
        MavResult::MAV_RESULT_ACCEPTED => Ok(()),
        MavResult::MAV_RESULT_TEMPORARILY_REJECTED =>
            Err("Command temporarily rejected — try again".into()),
        MavResult::MAV_RESULT_DENIED =>
            Err("Command denied by the flight controller".into()),
        MavResult::MAV_RESULT_UNSUPPORTED =>
            Err("Command not supported by this firmware".into()),
        MavResult::MAV_RESULT_FAILED =>
            Err("Command failed on the flight controller".into()),
        other => Err(format!("Command rejected: {:?}", other)),
    }
}

fn wait_for_ack(
    rx: &mpsc::Receiver<MavMessage>,
    command: MavCmd,
    timeout: Duration,
) -> Result<(), String> {
    result_to_err(wait_for_ack_result(rx, command, timeout)?)
}
