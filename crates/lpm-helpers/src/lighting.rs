//! Spectrum keyboard lighting (Legion Gen10 per-key RGB) — shared by
//! lighting-helper and lpm-gamemode.
//!
//! Unlike the other helpers this one normally runs *as the user*: the udev
//! rule (70-legion-power-manager-lighting.rules) tags the controller's hidraw
//! node `uaccess`. Callers try it directly first and fall back to pkexec only
//! when the reply carries `"denied": true` (rule missing, or the node was
//! created before it was installed).
//!
//! Request (one JSON object):
//!   {"op": "probe"}                         -> {"ok", "present"}
//!   {"op": "state", "profile"?: n}          -> device state, key map, effects of `profile` (default: active)
//!   {"op": "effects", "profile": n}         -> {"ok", "profile", "effects", "bytes"}
//!   {"op": "set", "profile"?, "brightness"?, "logo"?}
//!   {"op": "write", "profile": n, "effects": [..], "activate"?: bool}
//!   {"op": "reset", "profile": n}           factory default for one profile
//! Effect: {"type": 1..13, "speed": 0..3, "direction": 0..4, "clockwise": 0..2,
//!          "color_mode": 0..2, "colors": ["rrggbb", ..], "keys": [keycode, ..]}
//!
//! Every write goes to the controller's non-volatile profile store, so the
//! GUI only writes on an explicit Apply (never while dragging a slider).

use lpm_spectrum::{self as sp, Device, Effect, Rgb};
use serde_json::{json, Map, Value};

pub const MAX_REQUEST: usize = 16 * 1024;

fn err(e: sp::Error) -> Value {
    let denied = matches!(&e, sp::Error::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied);
    let mut v = json!({"ok": false, "error": e.to_string()});
    if denied {
        v["denied"] = json!(true);
        v["error"] = json!("no access to the keyboard's hidraw node (udev rule not applied yet — re-login or re-plug)");
    }
    if matches!(e, sp::Error::NotFound) {
        v["present"] = json!(false);
    }
    v
}

fn small(v: &Value, key: &str, max: u8) -> Result<Option<u8>, String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(x) => x.as_u64().filter(|n| *n <= max as u64).map(|n| Some(n as u8))
            .ok_or_else(|| format!("'{key}' must be an integer 0-{max}")),
    }
}

pub fn effect_to_json(e: &Effect) -> Value {
    json!({
        "type": e.kind, "speed": e.speed, "direction": e.direction, "clockwise": e.clockwise,
        "color_mode": e.color_mode,
        "colors": e.colors.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
        "keys": e.keys,
    })
}

pub fn effect_from_json(v: &Value) -> Result<Effect, String> {
    let kind = small(v, "type", 13)?.filter(|k| *k >= 1).ok_or("effect 'type' must be 1-13")?;
    let colors = match v.get("colors") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) if a.len() <= 32 => a.iter()
            .map(|c| c.as_str().and_then(Rgb::parse_hex).ok_or("colour must be \"rrggbb\""))
            .collect::<Result<_, _>>()?,
        _ => return Err("'colors' must be an array of at most 32 colours".into()),
    };
    let keys = match v.get("keys") {
        Some(Value::Array(a)) if a.len() <= 255 => a.iter()
            .map(|k| k.as_u64().filter(|n| (1..=0xFFFF).contains(n)).map(|n| n as u16).ok_or("bad keycode"))
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("'keys' must be an array of at most 255 keycodes".into()),
    };
    if keys.is_empty() {
        return Err("an effect needs at least one key".into());
    }
    Ok(Effect {
        kind,
        speed: small(v, "speed", 3)?.unwrap_or(0),
        direction: small(v, "direction", 4)?.unwrap_or(0),
        clockwise: small(v, "clockwise", 2)?.unwrap_or(0),
        color_mode: small(v, "color_mode", 2)?.unwrap_or(if colors.is_empty() { 0 } else { 2 }),
        colors,
        keys,
    })
}

fn profile_arg(req: &Value) -> Result<Option<u8>, String> { small(req, "profile", sp::MAX_PROFILE) }

fn effects_json(dev: &Device, profile: u8) -> Result<Value, sp::Error> {
    let fx = dev.read_effects(profile)?;
    Ok(json!({
        "profile": profile,
        "effects": fx.iter().map(effect_to_json).collect::<Vec<_>>(),
        "bytes": sp::encoded_len(&fx),
    }))
}

fn merge(mut a: Value, b: Value) -> Value {
    if let (Some(a), Value::Object(b)) = (a.as_object_mut(), b) { a.extend(b); }
    a
}

/// `profile` / `brightness` / `logo`, in that order (the profile switch
/// re-applies the profile's own brightness on some firmware).
fn set(dev: &Device, req: &Value) -> Result<Value, Value> {
    let bad = |m: String| json!({"ok": false, "error": m});
    let profile = profile_arg(req).map_err(bad)?;
    let brightness = small(req, "brightness", sp::MAX_BRIGHTNESS).map_err(bad)?;
    let logo = match req.get("logo") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        _ => return Err(bad("'logo' must be true/false".into())),
    };
    if let Some(p) = profile { dev.set_profile(p).map_err(err)?; }
    if let Some(b) = brightness { dev.set_brightness(b).map_err(err)?; }
    if let Some(l) = logo { dev.set_logo(l).map_err(err)?; }
    Ok(json!({"ok": true, "profile": dev.profile().map_err(err)?,
              "brightness": dev.brightness().map_err(err)?, "logo": dev.logo().map_err(err)?}))
}

pub fn handle(req: &Value) -> Value {
    let op = req.get("op").and_then(Value::as_str).unwrap_or("");
    if !matches!(op, "probe" | "state" | "effects" | "set" | "write" | "reset") {
        return json!({"ok": false, "error": format!("unknown op '{op}'")});
    }
    let dev = match Device::find() {
        Ok(d) => d,
        Err(sp::Error::NotFound) if op == "probe" => return json!({"ok": true, "present": false}),
        Err(e) => return err(e),
    };
    let run = || -> Result<Value, Value> {
        let bad = |m: String| json!({"ok": false, "error": m});
        match op {
            "probe" => Ok(json!({"ok": true, "present": true, "node": dev.path().display().to_string()})),
            "state" => {
                let active = dev.profile().map_err(err)?;
                let profile = profile_arg(req).map_err(bad)?.unwrap_or(active);
                let m = dev.keymap().map_err(err)?;
                let base = json!({
                    "ok": true, "present": true, "node": dev.path().display().to_string(),
                    "active_profile": active, "brightness": dev.brightness().map_err(err)?,
                    "logo": dev.logo().map_err(err)?, "bytes_max": sp::REPORT_LEN,
                    "keymap": {"rows": m.rows, "cols": m.cols, "grid": m.grid, "extra": m.extra},
                });
                Ok(merge(base, effects_json(&dev, profile).map_err(err)?))
            }
            "effects" => {
                let p = profile_arg(req).map_err(bad)?.ok_or_else(|| bad("'profile' required".into()))?;
                Ok(merge(json!({"ok": true}), effects_json(&dev, p).map_err(err)?))
            }
            "set" => set(&dev, req),
            "write" => {
                let p = profile_arg(req).map_err(bad)?.ok_or_else(|| bad("'profile' required".into()))?;
                let list = req.get("effects").and_then(Value::as_array)
                    .filter(|a| a.len() <= 64).ok_or_else(|| bad("'effects' must be an array (max 64)".into()))?;
                let fx = list.iter().map(effect_from_json).collect::<Result<Vec<_>, _>>().map_err(bad)?;
                dev.write_effects(p, &fx).map_err(err)?;
                if req.get("activate").and_then(Value::as_bool).unwrap_or(false) {
                    dev.set_profile(p).map_err(err)?;
                }
                Ok(json!({"ok": true, "profile": p, "bytes": sp::encoded_len(&fx)}))
            }
            "reset" => {
                let p = profile_arg(req).map_err(bad)?.ok_or_else(|| bad("'profile' required".into()))?;
                dev.reset_profile(p).map_err(err)?;
                Ok(json!({"ok": true, "profile": p}))
            }
            _ => unreachable!(),
        }
    };
    run().unwrap_or_else(|e| e)
}

/// A scene's "lighting" object: {"profile"?, "brightness"?, "logo"?}.
/// Returns the request to send (op "set"), or None when it changes nothing.
pub fn scene_request(v: &Value) -> Option<Value> {
    let o = v.as_object()?;
    let mut req = Map::new();
    req.insert("op".into(), json!("set"));
    for k in ["profile", "brightness", "logo"] {
        if let Some(x) = o.get(k).filter(|x| !x.is_null()) { req.insert(k.into(), x.clone()); }
    }
    (req.len() > 1).then_some(Value::Object(req))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_json_roundtrip() {
        let e = Effect { kind: 11, speed: 0, direction: 0, clockwise: 0, color_mode: 2,
                         colors: vec![Rgb(255, 0, 64)], keys: vec![1, 0x3e9] };
        assert_eq!(effect_from_json(&effect_to_json(&e)).unwrap(), e);
    }

    #[test]
    fn effect_validation() {
        assert!(effect_from_json(&json!({"type": 0, "keys": [1]})).is_err());
        assert!(effect_from_json(&json!({"type": 11, "keys": []})).is_err());
        assert!(effect_from_json(&json!({"type": 11, "speed": 4, "keys": [1]})).is_err());
        assert!(effect_from_json(&json!({"type": 11, "colors": ["zz0000"], "keys": [1]})).is_err());
        assert_eq!(effect_from_json(&json!({"type": 2, "keys": [1]})).unwrap().color_mode, 0);
    }

    #[test]
    fn scene() {
        assert_eq!(scene_request(&json!({})), None);
        assert_eq!(scene_request(&json!({"profile": 3, "brightness": null})),
                   Some(json!({"op": "set", "profile": 3})));
    }
}
