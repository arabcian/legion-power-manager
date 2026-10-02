//! lpm-netguard — network guard daemon of Legion Power Manager (see netguard.rs).
//!
//!   lpm-netguard daemon    enforce the IP blacklist and block non-whitelisted
//!                          Wine/.exe programs (root; OpenRC/systemd service)
//!   lpm-netguard status    configuration + daemon state (JSON)
//!   lpm-netguard list      current TCP/UDP sockets with their processes (JSON)

use lpm_helpers::netguard;

fn main() {
    lpm_helpers::init();
    let code = match std::env::args().nth(1).as_deref() {
        Some("daemon") => netguard::run_daemon(),
        Some("status") => lpm_helpers::finish(netguard::status(true)),
        Some("list") => lpm_helpers::finish(netguard::list()),
        _ => { eprintln!("usage: lpm-netguard daemon | status | list"); 2 }
    };
    std::process::exit(code);
}
