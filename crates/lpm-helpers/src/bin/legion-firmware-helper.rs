//! Root helper for firmware-persistent changes (pkexec target).
//!
//! Everything here survives a reboot or only takes effect at the next one, and
//! a bad value can leave the machine unable to POST or with a black screen.
//! That is why these operations live in their own binary: polkit pins one
//! action per helper path, and com.legion-power-manager.firmware.write always
//! asks for the administrator password (auth_admin, no keep, no silent grant
//! in 49-legion-power-manager.rules). The day-to-day knobs in the other
//! helpers stay password-less.
//!
//!   {"op": "aod_set", "values": {"tCL": 36, ...}}   BIOS DRAM timing overrides (AodSetupRpl)
//!   {"op": "aod_restore"}                            write the newest AodSetupRpl backup back
//!   {"op": "set_fw_oc", "key": "pbo_scalar"|"boost_mhz"|"curve_optimizer", "value": n}
//!   {"op": "set_gpu_mode", "mode": "hybrid"|"dgpu", "force": bool}   MUX, next boot
//!   {"op": "set_igpu_mode", "mode": 0|1|2, "force": true}             only the forced override;
//!                                                   the guarded path stays in legion-gpu-helper
//! Same contract as the other helpers: one JSON object on stdin, one JSON line out.

use lpm_helpers::{legion_wmi, memory_spd, *};
use serde_json::{json, Value};

fn run() -> Value {
    let req = match read_request(8192) { Ok(v) => v, Err(e) => return e };
    let Some(o) = req.as_object() else { return json!({"ok": false, "error": "payload must be an object"}) };
    let force = o.get("force").and_then(Value::as_bool).unwrap_or(false);
    match o.get("op").and_then(Value::as_str) {
        Some("aod_set") => memory_spd::aod_set(o.get("values")),
        Some("aod_restore") => memory_spd::aod_restore(),
        Some("set_fw_oc") => match (o.get("key").and_then(Value::as_str), o.get("value").and_then(Value::as_i64)) {
            (Some(k), Some(v)) => legion_wmi::set_fw_oc(k, v),
            _ => json!({"ok": false, "error": "'key' and 'value' required"}),
        },
        Some("set_gpu_mode") => legion_wmi::set_gpu_mode(o.get("mode").and_then(Value::as_str).unwrap_or(""), force),
        Some("set_igpu_mode") => legion_wmi::set_igpu_mode(o.get("mode").and_then(Value::as_u64).unwrap_or(99), force),
        other => json!({"ok": false, "error": format!("unknown op: {}", other.unwrap_or("None"))}),
    }
}

fn main() { init(); std::process::exit(finish(run())); }
