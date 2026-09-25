//! Root helper for the experimental Legion GPU power tab (pkexec).
//!   {"op": "status"}                      add "envelope": true for nvidia-smi's power range (wakes the dGPU)
//!   {"op": "apply", "values": {"ctgp": 140, "boost_up": 25, ...}}
//!   {"op": "gpu_mode"}                     MUX state: active now / next boot
//!   {"op": "set_gpu_mode", "mode": "hybrid"|"dgpu", "force": false}   takes effect at the next boot
use lpm_helpers::legion_wmi;
use lpm_helpers::*;
use serde_json::{json, Value};

fn run() -> Value {
    let req = match read_request(8192) { Ok(v) => v, Err(e) => return e };
    let Some(o) = req.as_object() else { return json!({"ok": false, "error": "payload must be an object"}) };
    match o.get("op").and_then(Value::as_str) {
        Some("status") => legion_wmi::status(o.get("envelope").and_then(Value::as_bool).unwrap_or(false)),
        Some("apply") => match o.get("values").and_then(Value::as_object) {
            Some(v) => legion_wmi::apply(v),
            None => json!({"ok": false, "error": "apply needs a 'values' object"}),
        },
        Some("gpu_mode") => legion_wmi::gpu_mode_status(),
        Some("set_gpu_mode") => legion_wmi::set_gpu_mode(o.get("mode").and_then(Value::as_str).unwrap_or(""),
                                                         o.get("force").and_then(Value::as_bool).unwrap_or(false)),
        other => json!({"ok": false, "error": format!("unknown op: {}", other.unwrap_or("None"))}),
    }
}
fn main() { init(); std::process::exit(finish(run())); }
