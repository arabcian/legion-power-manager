//! Keyboard lighting helper (Spectrum per-key RGB, Legion Gen10).
//! Protocol and access model: see lpm_helpers::lighting. Runs as the user
//! when the udev rule grants the hidraw node, through pkexec otherwise.

use lpm_helpers::*;

fn main() {
    init();
    let code = match read_request(lighting::MAX_REQUEST) {
        Ok(req) => finish(lighting::handle(&req)),
        Err(e) => finish(e),
    };
    std::process::exit(code);
}
