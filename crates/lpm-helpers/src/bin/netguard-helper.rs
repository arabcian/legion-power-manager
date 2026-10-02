//! Helper of Health → Network (same contract as the other helpers: one JSON
//! request on stdin, one JSON line on stdout).
//!   any user:  {"op":"list"}  {"op":"status"}
//!   root (pkexec):
//!     {"op":"set", "guard": bool, "allow_lan": bool}
//!     {"op":"blacklist", "add": ["ip|cidr"], "remove": [...]}   live sockets to added addresses are aborted
//!     {"op":"whitelist", "add": ["path | dir/ | name.exe"], "remove": [...]}
//!     {"op":"kill", "proto": "tcp|udp", "lport": n, "remote": "ip", "rport": n}
//!     {"op":"clear_log"}
//! Rule changes are written to /etc/legion-power-manager/netguard.json and the
//! running daemon is told to reload (SIGHUP).

use lpm_helpers::*;

fn main() {
    init();
    let out = match read_request(256 * 1024) {
        Ok(req) => netguard::handle(&req),
        Err(e) => e,
    };
    std::process::exit(finish(out));
}
