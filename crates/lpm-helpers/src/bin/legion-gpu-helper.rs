//! Root helper for the experimental Legion GPU power tab (pkexec).
//!   {"op": "status"}
//!   {"op": "apply", "values": {"ctgp": 140, "boost_up": 25, ...}}
use lpm_helpers::legion_wmi;
use lpm_helpers::*;
use serde_json::{json, Value};

fn run() -> Value {
    let req = match read_request(8192) { Ok(v) => v, Err(e) => return e };
    let Some(o) = req.as_object() else { return json!({"ok": false, "error": "payload must be an object"}) };
    match o.get("op").and_then(Value::as_str) {
        Some("status") => legion_wmi::status(),
        Some("apply") => match o.get("values").and_then(Value::as_object) {
            Some(v) => legion_wmi::apply(v),
            None => json!({"ok": false, "error": "apply needs a 'values' object"}),
        },
        other => json!({"ok": false, "error": format!("unknown op: {}", other.unwrap_or("None"))}),
    }
}
fn main() { std::process::exit(finish(run())); }
