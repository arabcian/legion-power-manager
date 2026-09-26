//! AMD GPU tuning helper: overclock / undervolt (overdrive table), power
//! limit, power profile, performance level and PMFW fan settings of one
//! amdgpu card. Protocol and write order: see lpm_helpers::amdgpu.
//!   {"op": "apply", "card": "card1", "settings": {...}}
//!   {"op": "reset", "card": "card1"}

use lpm_helpers::*;

fn main() {
    init();
    let code = match read_request(amdgpu::MAX_REQUEST) {
        Ok(req) => finish(amdgpu::handle(&req)),
        Err(e) => finish(e),
    };
    std::process::exit(code);
}
