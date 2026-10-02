//! Network guard — Health → Network.
//!
//! Three parts, one module:
//!   * connection list   sock_diag (the kernel interface behind `ss`) + /proc/<pid>/fd
//!   * IP blacklist      nftables table `inet lpm_netguard` (drop in + out)
//!   * Wine/.exe guard   the first packet of every new outbound flow is queued
//!                       (NFQUEUE) to the daemon; it finds the owning socket
//!                       and process, and if that is a Wine program which is
//!                       not whitelisted: verdict DROP, socket destroyed
//!                       (SOCK_DESTROY), one line in the block log.
//!
//! Config   /etc/legion-power-manager/netguard.json   (root-owned, world-readable)
//! Log      /var/log/legion-power-manager/netguard.log (JSON lines, rotated at 1 MiB)
//! Daemon   lpm-netguard daemon; holds a lock on /run/legion-power-manager/netguard.pid,
//!          reloads on SIGHUP (sent by the helper after every rule change).
//!
//! Cost model: nothing is polled. With the guard off there is no queue rule at
//! all; with it on, only the first packet(s) of a new non-loopback flow reach
//! the daemon (ct state new), and a flow that does not belong to a Wine
//! process costs one exact socket lookup plus a readlink per known Wine pid.
//! The queue rule carries `bypass`: without a daemon traffic flows unguarded,
//! it is never blackholed.
//!
//! Kernel: NETFILTER_NETLINK_QUEUE, NF_TABLES, NF_TABLES_INET, NFT_QUEUE,
//! NF_CONNTRACK, NFT_CT, INET_DIAG, INET_TCP_DIAG, INET_UDP_DIAG,
//! INET_DIAG_DESTROY. Userspace: nft (net-firewall/nftables).

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

pub const CONF_DIR: &str = "/etc/legion-power-manager";
pub const CONF_FILE: &str = "/etc/legion-power-manager/netguard.json";
const RUN_DIR: &str = "/run/legion-power-manager";
const LOCK_FILE: &str = "/run/legion-power-manager/netguard.pid";
const LOG_DIR: &str = "/var/log/legion-power-manager";
pub const LOG_FILE: &str = "/var/log/legion-power-manager/netguard.log";
const LOG_MAX: u64 = 1 << 20;
const QUEUE_NUM: u16 = 19536;
const TABLE: &str = "lpm_netguard";
const MAX_BLACKLIST: usize = 4096;
const MAX_WHITELIST: usize = 1024;

pub const KCONFIG: &[&str] = &["NETFILTER_NETLINK_QUEUE", "NF_TABLES", "NF_TABLES_INET", "NFT_QUEUE", "NF_CONNTRACK",
                               "NFT_CT", "INET_DIAG", "INET_TCP_DIAG", "INET_UDP_DIAG", "INET_DIAG_DESTROY"];

const AF_INET: u8 = libc::AF_INET as u8;
const AF_INET6: u8 = libc::AF_INET6 as u8;
const TCP: u8 = 6;
const UDP: u8 = 17;

// ── networks ───────────────────────────────────────────────────────────────

/// An address or CIDR block, host bits cleared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Net { addr: IpAddr, prefix: u8 }

fn unmap(ip: IpAddr) -> IpAddr {
    match ip { IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4), v4 => v4 }
}

impl Net {
    pub fn parse(s: &str) -> Option<Net> {
        let (a, p) = match s.trim().split_once('/') { Some((a, p)) => (a, Some(p)), None => (s.trim(), None) };
        let mut addr = unmap(a.parse().ok()?);
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix: u8 = match p { Some(p) => p.parse().ok()?, None => max };
        if prefix == 0 || prefix > max { return None; }  // /0 would cut the machine off
        addr = match addr {
            IpAddr::V4(v) => IpAddr::V4(Ipv4Addr::from(u32::from(v) & (!0u32 << (32 - prefix)))),
            IpAddr::V6(v) => IpAddr::V6(Ipv6Addr::from(u128::from(v) & (!0u128 << (128 - prefix)))),
        };
        Some(Net { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, unmap(ip)) {
            (IpAddr::V4(n), IpAddr::V4(i)) => (u32::from(i) ^ u32::from(n)) >> (32 - self.prefix) == 0,
            (IpAddr::V6(n), IpAddr::V6(i)) => (u128::from(i) ^ u128::from(n)) >> (128 - self.prefix) == 0,
            _ => false,
        }
    }
}

impl std::fmt::Display for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.prefix == if self.addr.is_ipv4() { 32 } else { 128 } { write!(f, "{}", self.addr) }
        else { write!(f, "{}/{}", self.addr, self.prefix) }
    }
}

// ── config ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Config {
    /// Block Wine programs that are not on the whitelist.
    pub guard: bool,
    /// Private / link-local / multicast destinations are not "internet".
    pub allow_lan: bool,
    /// Exact path, directory prefix (trailing '/') or bare file name (case-insensitive).
    pub whitelist: Vec<String>,
    pub blacklist: Vec<Net>,
}

impl Default for Config {
    fn default() -> Self { Config { guard: false, allow_lan: true, whitelist: Vec::new(), blacklist: Vec::new() } }
}

fn basename(s: &str) -> &str { s.rsplit(|c| c == '/' || c == '\\').next().unwrap_or(s) }

fn valid_entry(s: &str) -> bool { !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control) }

impl Config {
    pub fn load() -> Result<Config, String> {
        if !Path::new(CONF_FILE).exists() { return Ok(Config::default()); }
        let text = crate::read_root_file(CONF_FILE, 1 << 20).ok_or_else(|| format!("{CONF_FILE} is not a root-owned regular file"))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{CONF_FILE}: {e}"))?;
        let strs = |k: &str| v[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect::<Vec<_>>()).unwrap_or_default();
        Ok(Config {
            guard: v["guard"].as_bool().unwrap_or(false),
            allow_lan: v["allow_lan"].as_bool().unwrap_or(true),
            whitelist: strs("whitelist").into_iter().filter(|s| valid_entry(s)).take(MAX_WHITELIST).collect(),
            blacklist: strs("blacklist").iter().filter_map(|s| Net::parse(s)).take(MAX_BLACKLIST).collect(),
        })
    }

    pub fn to_json(&self) -> Value {
        json!({"guard": self.guard, "allow_lan": self.allow_lan, "whitelist": self.whitelist,
               "blacklist": self.blacklist.iter().map(Net::to_string).collect::<Vec<_>>()})
    }

    pub fn save(&self) -> Result<(), String> {
        crate::secure_dir(CONF_DIR)?;
        crate::write_root_file(CONF_FILE, &serde_json::to_vec_pretty(&self.to_json()).map_err(|e| e.to_string())?)
    }

    pub fn whitelisted(&self, ident: &str) -> bool {
        let base = basename(ident).to_lowercase();
        self.whitelist.iter().any(|w| {
            if w.ends_with('/') { ident.starts_with(w.as_str()) }
            else if w.contains('/') || w.contains('\\') { w == ident }
            else { w.to_lowercase() == base }
        })
    }

    pub fn blacklisted(&self, ip: IpAddr) -> bool { self.blacklist.iter().any(|n| n.contains(ip)) }
}

// ── netlink plumbing ───────────────────────────────────────────────────────

const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLM_F_DUMP: u16 = 0x300;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;

fn nl_open(proto: libc::c_int) -> io::Result<OwnedFd> {
    let raw = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, proto) };
    if raw < 0 { return Err(io::Error::last_os_error()); }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    let r = unsafe { libc::bind(raw, &sa as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t) };
    if r < 0 { return Err(io::Error::last_os_error()); }
    Ok(fd)
}

fn nl_msg(ty: u16, flags: u16, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(16 + body.len() + 3);
    m.extend_from_slice(&((16 + body.len()) as u32).to_ne_bytes());
    m.extend_from_slice(&ty.to_ne_bytes());
    m.extend_from_slice(&flags.to_ne_bytes());
    m.extend_from_slice(&[0u8; 8]);  // seq, pid
    m.extend_from_slice(body);
    while m.len() % 4 != 0 { m.push(0); }
    m
}

fn nl_attr(out: &mut Vec<u8>, ty: u16, data: &[u8]) {
    out.extend_from_slice(&((4 + data.len()) as u16).to_ne_bytes());
    out.extend_from_slice(&ty.to_ne_bytes());
    out.extend_from_slice(data);
    while out.len() % 4 != 0 { out.push(0); }
}

fn nl_send(fd: &OwnedFd, msg: &[u8]) -> io::Result<()> {
    loop {
        let n = unsafe { libc::send(fd.as_raw_fd(), msg.as_ptr().cast(), msg.len(), 0) };
        if n >= 0 { return Ok(()); }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted { return Err(e); }
    }
}

fn nl_recv(fd: &OwnedFd, buf: &mut [u8], flags: libc::c_int) -> io::Result<usize> {
    let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), flags) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

/// Calls `f(type, payload)` for every netlink message in one datagram.
fn nl_each(mut buf: &[u8], mut f: impl FnMut(u16, &[u8])) {
    while buf.len() >= 16 {
        let len = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if len < 16 || len > buf.len() { break; }
        f(u16::from_ne_bytes([buf[4], buf[5]]), &buf[16..len]);
        buf = &buf[((len + 3) & !3).min(buf.len())..];
    }
}

/// errno of an NLMSG_ERROR payload (0 = plain ACK).
fn nl_errno(p: &[u8]) -> i32 { if p.len() >= 4 { -i32::from_ne_bytes([p[0], p[1], p[2], p[3]]) } else { libc::EIO } }

// ── sock_diag ──────────────────────────────────────────────────────────────

const SOCK_DIAG_BY_FAMILY: u16 = 20;
const SOCK_DESTROY: u16 = 21;
const TCP_LISTEN: u8 = 10;

#[derive(Clone, Debug)]
pub struct Sock {
    pub family: u8,
    pub proto: u8,
    pub state: u8,
    pub src: IpAddr,
    pub sport: u16,
    pub dst: IpAddr,
    pub dport: u16,
    pub uid: u32,
    pub inode: u32,
    id: [u8; 48],  // inet_diag_sockid as the kernel reported it (with cookie), for SOCK_DESTROY
}

fn ip_of(family: u8, b: &[u8]) -> IpAddr {
    if family == AF_INET { IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3])) }
    else { let mut a = [0u8; 16]; a.copy_from_slice(&b[..16]); unmap(IpAddr::V6(Ipv6Addr::from(a))) }
}

/// struct inet_diag_msg (72 bytes) → Sock.
fn parse_sock(p: &[u8], proto: u8) -> Option<Sock> {
    if p.len() < 72 || (p[0] != AF_INET && p[0] != AF_INET6) { return None; }
    let mut id = [0u8; 48];
    id.copy_from_slice(&p[4..52]);
    Some(Sock {
        family: p[0], proto, state: p[1],
        sport: u16::from_be_bytes([id[0], id[1]]), dport: u16::from_be_bytes([id[2], id[3]]),
        src: ip_of(p[0], &id[4..20]), dst: ip_of(p[0], &id[20..36]),
        uid: u32::from_ne_bytes([p[64], p[65], p[66], p[67]]),
        inode: u32::from_ne_bytes([p[68], p[69], p[70], p[71]]),
        id,
    })
}

fn put_ip(dst: &mut [u8], ip: IpAddr) {
    match ip { IpAddr::V4(v) => dst[..4].copy_from_slice(&v.octets()), IpAddr::V6(v) => dst[..16].copy_from_slice(&v.octets()) }
}

pub struct Diag { fd: OwnedFd }

impl Diag {
    pub fn open() -> io::Result<Diag> {
        let fd = nl_open(libc::NETLINK_SOCK_DIAG)?;
        // A lost reply must never hang the daemon with a packet waiting for its verdict.
        let tv = libc::timeval { tv_sec: 1, tv_usec: 0 };
        unsafe { libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv as *const _ as *const libc::c_void,
                                  std::mem::size_of::<libc::timeval>() as libc::socklen_t); }
        Ok(Diag { fd })
    }

    /// struct inet_diag_req_v2.
    fn request(&self, ty: u16, flags: u16, family: u8, proto: u8, states: u32, id: &[u8; 48]) -> io::Result<()> {
        let mut b = Vec::with_capacity(56);
        b.extend_from_slice(&[family, proto, 0, 0]);
        b.extend_from_slice(&states.to_ne_bytes());
        b.extend_from_slice(id);
        nl_send(&self.fd, &nl_msg(ty, NLM_F_REQUEST | flags, &b))
    }

    /// All sockets of one family/protocol; `sport` != 0 filters by local port in the kernel.
    pub fn dump(&self, family: u8, proto: u8, states: u32, sport: u16) -> io::Result<Vec<Sock>> {
        let mut id = [0u8; 48];
        id[0..2].copy_from_slice(&sport.to_be_bytes());
        self.request(SOCK_DIAG_BY_FAMILY, NLM_F_DUMP, family, proto, states, &id)?;
        let mut out = Vec::new();
        let mut buf = [0u8; 32768];
        loop {
            let n = nl_recv(&self.fd, &mut buf, 0)?;
            let (mut done, mut err) = (false, 0);
            nl_each(&buf[..n], |ty, p| match ty {
                NLMSG_DONE => done = true,
                NLMSG_ERROR => { err = nl_errno(p); done = true; }
                SOCK_DIAG_BY_FAMILY => out.extend(parse_sock(p, proto)),
                _ => {}
            });
            if err != 0 { return Err(io::Error::from_raw_os_error(err)); }
            if done { return Ok(out); }
        }
    }

    /// One TCP socket by its 4-tuple (hash lookup in the kernel, no dump).
    fn exact_tcp(&self, family: u8, local: (IpAddr, u16), remote: (IpAddr, u16)) -> Option<Sock> {
        let mut id = [0u8; 48];
        id[0..2].copy_from_slice(&local.1.to_be_bytes());
        id[2..4].copy_from_slice(&remote.1.to_be_bytes());
        put_ip(&mut id[4..20], local.0);
        put_ip(&mut id[20..36], remote.0);
        id[40..48].fill(0xff);  // INET_DIAG_NOCOOKIE
        self.request(SOCK_DIAG_BY_FAMILY, 0, family, TCP, !0, &id).ok()?;
        let mut buf = [0u8; 4096];
        let n = nl_recv(&self.fd, &mut buf, 0).ok()?;
        let mut found = None;
        nl_each(&buf[..n], |ty, p| if ty == SOCK_DIAG_BY_FAMILY { found = parse_sock(p, TCP); });
        found
    }

    /// The local socket an outgoing packet came from.
    pub fn find_socket(&self, f: &Flow) -> Option<Sock> {
        let v4 = f.src.is_ipv4();
        let fams: &[u8] = if v4 { &[AF_INET, AF_INET6] } else { &[AF_INET6] };  // v4 flows may use a dual-stack socket
        if f.proto == TCP {
            if let Some(s) = self.exact_tcp(fams[0], (f.src, f.sport), (f.dst, f.dport)) { return Some(s); }
            // Device-bound or dual-stack socket: port-filtered dump instead.
            for &fam in fams {
                let hit = self.dump(fam, TCP, !(1u32 << TCP_LISTEN), f.sport).ok()?.into_iter()
                    .find(|s| s.dport == f.dport && s.dst == f.dst);
                if hit.is_some() { return hit; }
            }
            return None;
        }
        for &fam in fams {
            let v = self.dump(fam, UDP, !0, f.sport).ok()?;
            let hit = v.iter().find(|s| s.dport == f.dport && s.dst == f.dst)  // connected to this peer
                .or_else(|| v.iter().find(|s| s.dport == 0))                     // unconnected (sendto)
                .or(v.first());
            if hit.is_some() { return hit.cloned(); }
        }
        None
    }

    /// Aborts the socket (TCP: ECONNABORTED to the owner). Needs CAP_NET_ADMIN
    /// and CONFIG_INET_DIAG_DESTROY (EOPNOTSUPP without it).
    pub fn destroy(&self, s: &Sock) -> io::Result<()> {
        self.request(SOCK_DESTROY, NLM_F_ACK, s.family, s.proto, !0, &s.id)?;
        let mut buf = [0u8; 4096];
        let n = nl_recv(&self.fd, &mut buf, 0)?;
        let mut err = libc::EIO;
        nl_each(&buf[..n], |ty, p| if ty == NLMSG_ERROR { err = nl_errno(p); });
        if err == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(err)) }
    }
}

// ── processes ──────────────────────────────────────────────────────────────

fn pids() -> impl Iterator<Item = i32> {
    std::fs::read_dir("/proc").into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse().ok())
}

fn socket_inodes(pid: i32) -> impl Iterator<Item = u32> {
    std::fs::read_dir(format!("/proc/{pid}/fd")).into_iter().flatten().flatten().filter_map(|e| {
        let l = std::fs::read_link(e.path()).ok()?;
        l.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
    })
}

fn exe_of(pid: i32) -> Option<String> {
    let p = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let s = p.to_string_lossy();
    Some(s.strip_suffix(" (deleted)").unwrap_or(&s).to_owned())
}

/// wine, wine64, wine-preloader, wine64-preloader, wineserver, wineloader…
/// (system Wine, Proton, Lutris/Bottles runners, CrossOver).
pub fn is_wine_loader(exe: &str) -> bool { basename(exe).starts_with("wine") }

fn comm(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_owned()).unwrap_or_default()
}

/// What a Wine process really runs: the unix path of the mapped Windows .exe
/// (the image named by argv[0] when several are mapped). Without a mapped
/// .exe yet: argv[0] (Z:\ paths made unix), else the loader path.
pub fn wine_ident(pid: i32, loader: &str) -> String {
    let argv0 = std::fs::read(format!("/proc/{pid}/cmdline")).ok()
        .map(|b| String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(&[])).into_owned()).unwrap_or_default();
    let want = basename(&argv0).to_lowercase();
    let mut first: Option<String> = None;
    if let Ok(f) = std::fs::File::open(format!("/proc/{pid}/maps")) {
        for line in io::BufReader::new(f).lines().map_while(Result::ok).take(100_000) {
            // address perms offset dev inode   pathname
            let Some(path) = line.splitn(6, ' ').nth(5).map(str::trim_start) else { continue };
            let path = path.strip_suffix(" (deleted)").unwrap_or(path);
            if !path.starts_with('/') || path.len() < 5 || !path.as_bytes()[path.len() - 4..].eq_ignore_ascii_case(b".exe") { continue; }
            if basename(path).to_lowercase() == want { return path.to_owned(); }
            first.get_or_insert_with(|| path.to_owned());
        }
    }
    if let Some(p) = first { return p; }
    if argv0.len() > 3 && argv0.as_bytes()[..3].eq_ignore_ascii_case(b"z:\\") { return argv0[2..].replace('\\', "/"); }
    if argv0.to_lowercase().ends_with(".exe") { argv0 } else { loader.to_owned() }
}

/// inode → pid for the wanted socket inodes (first owner wins). Sees only the
/// caller's own processes unless root.
fn owners_of(want: &HashSet<u32>) -> HashMap<u32, i32> {
    let mut map = HashMap::new();
    for pid in pids() {
        for ino in socket_inodes(pid) {
            if want.contains(&ino) { map.entry(ino).or_insert(pid); }
        }
        if map.len() == want.len() { break; }
    }
    map
}

// ── nftables ───────────────────────────────────────────────────────────────

fn sys_bin(name: &str) -> Option<PathBuf> {
    ["/usr/sbin", "/sbin", "/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(name)).find(|p| crate::trusted_path(p))
}

/// The whole table, replaced atomically (declare → delete → define is one transaction).
fn nft_script(cfg: &Config) -> String {
    let set = |name: &str, ty: &str, v4: bool| {
        let el: Vec<String> = cfg.blacklist.iter().filter(|n| n.addr.is_ipv4() == v4).map(Net::to_string).collect();
        let elements = if el.is_empty() { String::new() } else { format!(" elements = {{ {} }}", el.join(", ")) };
        format!("\tset {name} {{ type {ty}; flags interval; auto-merge;{elements} }}\n")
    };
    let mut s = format!("table inet {TABLE}\ndelete table inet {TABLE}\ntable inet {TABLE} {{\n");
    s += &set("bl4", "ipv4_addr", true);
    s += &set("bl6", "ipv6_addr", false);
    s += "\tchain in {\n\t\ttype filter hook input priority 0; policy accept;\n\t\tip saddr @bl4 drop\n\t\tip6 saddr @bl6 drop\n\t}\n";
    s += "\tchain out {\n\t\ttype filter hook output priority 0; policy accept;\n\t\tip daddr @bl4 drop\n\t\tip6 daddr @bl6 drop\n";
    if cfg.guard {
        s += "\t\toifname \"lo\" accept\n\t\tct state != new accept\n";
        if cfg.allow_lan {
            s += "\t\tip daddr { 10.0.0.0/8, 100.64.0.0/10, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16, 224.0.0.0/4, 255.255.255.255 } accept\n";
            s += "\t\tip6 daddr { fc00::/7, fe80::/10, ff00::/8 } accept\n";
        }
        s += &format!("\t\tmeta l4proto {{ tcp, udp }} queue num {QUEUE_NUM} bypass\n");
    }
    s + "\t}\n}\n"
}

fn nft_run(script: &str) -> Result<(), String> {
    let nft = sys_bin("nft").ok_or("nft not found (emerge net-firewall/nftables)")?;
    let mut c = std::process::Command::new(nft).args(["-f", "-"]).env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .current_dir("/").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped()).spawn().map_err(|e| format!("nft: {e}"))?;
    let _ = c.stdin.take().map(|mut i| i.write_all(script.as_bytes()));
    let out = c.wait_with_output().map_err(|e| format!("nft: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(format!("nft: {}", String::from_utf8_lossy(&out.stderr).trim())) }
}

fn nft_remove() { let _ = nft_run(&format!("table inet {TABLE}\ndelete table inet {TABLE}\n")); }

// ── packets ────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub struct Flow { pub proto: u8, pub src: IpAddr, pub sport: u16, pub dst: IpAddr, pub dport: u16, l4: usize }

/// IPv4/IPv6 + TCP/UDP header of a packet starting at the network header.
pub fn parse_packet(p: &[u8]) -> Option<Flow> {
    let (proto, src, dst, l4) = match p.first()? >> 4 {
        4 if p.len() >= 20 => {
            if u16::from_be_bytes([p[6], p[7]]) & 0x1fff != 0 { return None; }  // not the first fragment
            (p[9], ip_of(AF_INET, &p[12..16]), ip_of(AF_INET, &p[16..20]), usize::from(p[0] & 0xf) * 4)
        }
        6 if p.len() >= 40 => (p[6], ip_of(AF_INET6, &p[8..24]), ip_of(AF_INET6, &p[24..40]), 40),
        _ => return None,
    };
    if proto != TCP && proto != UDP { return None; }
    let h = p.get(l4..l4 + 4)?;
    Some(Flow { proto, src, dst, sport: u16::from_be_bytes([h[0], h[1]]), dport: u16::from_be_bytes([h[2], h[3]]), l4 })
}

/// Question name of a DNS query (UDP payload).
fn dns_qname(p: &[u8]) -> Option<String> {
    let (mut i, mut name) = (12usize, String::new());
    loop {
        let l = usize::from(*p.get(i)?);
        if l == 0 { break; }
        if l > 63 || name.len() > 253 { return None; }
        if !name.is_empty() { name.push('.'); }
        name.extend(p.get(i + 1..i + 1 + l)?.iter().map(|&c| if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' { c as char } else { '?' }));
        i += 1 + l;
    }
    (!name.is_empty()).then_some(name)
}

// ── NFQUEUE ────────────────────────────────────────────────────────────────

const NFQ_PACKET: u16 = 3 << 8;       // NFNL_SUBSYS_QUEUE << 8 | NFQNL_MSG_PACKET
const NFQ_VERDICT: u16 = 3 << 8 | 1;
const NFQ_CONFIG: u16 = 3 << 8 | 2;
const NFQA_PACKET_HDR: u16 = 1;
const NFQA_VERDICT_HDR: u16 = 2;
const NFQA_PAYLOAD: u16 = 10;
const NFQA_CFG_CMD: u16 = 1;
const NFQA_CFG_PARAMS: u16 = 2;
const NFQA_CFG_MASK: u16 = 4;
const NFQA_CFG_FLAGS: u16 = 5;

struct Queue { fd: OwnedFd }

impl Queue {
    fn nfgen() -> Vec<u8> { let q = QUEUE_NUM.to_be_bytes(); vec![0, 0, q[0], q[1]] }  // AF_UNSPEC, NFNETLINK_V0, res_id

    fn config(&self, attrs: &[u8]) -> io::Result<()> {
        let mut body = Self::nfgen();
        body.extend_from_slice(attrs);
        nl_send(&self.fd, &nl_msg(NFQ_CONFIG, NLM_F_REQUEST | NLM_F_ACK, &body))?;
        let mut buf = [0u8; 4096];
        loop {
            let n = nl_recv(&self.fd, &mut buf, 0)?;
            let mut err = None;
            nl_each(&buf[..n], |ty, p| if ty == NLMSG_ERROR { err = Some(nl_errno(p)); });
            match err { Some(0) => return Ok(()), Some(e) => return Err(io::Error::from_raw_os_error(e)), None => {} }
        }
    }

    fn open() -> io::Result<Queue> {
        let q = Queue { fd: nl_open(libc::NETLINK_NETFILTER)? };
        let mut a = Vec::new();
        nl_attr(&mut a, NFQA_CFG_CMD, &[1, 0, 0, 0]);  // NFQNL_CFG_CMD_BIND
        q.config(&a)?;
        a.clear();
        let mut params = 256u32.to_be_bytes().to_vec();  // copy_range: headers + a DNS question
        params.push(2);                                  // NFQNL_COPY_PACKET
        nl_attr(&mut a, NFQA_CFG_PARAMS, &params);
        q.config(&a)?;
        // Queue full → accept instead of drop; do not segment GSO packets for us. Optional.
        a.clear();
        let flags = (1u32 | 1 << 2).to_be_bytes();  // NFQA_CFG_F_FAIL_OPEN | NFQA_CFG_F_GSO
        nl_attr(&mut a, NFQA_CFG_FLAGS, &flags);
        nl_attr(&mut a, NFQA_CFG_MASK, &flags);
        let _ = q.config(&a);
        let one: libc::c_int = 1;
        unsafe { libc::setsockopt(q.fd.as_raw_fd(), libc::SOL_NETLINK, libc::NETLINK_NO_ENOBUFS,
                                  &one as *const _ as *const libc::c_void, 4); }
        Ok(q)
    }

    fn verdict(&self, id: u32, accept: bool) {
        let mut body = Self::nfgen();
        let mut v = (accept as u32).to_be_bytes().to_vec();  // NF_DROP 0, NF_ACCEPT 1
        v.extend_from_slice(&id.to_be_bytes());
        nl_attr(&mut body, NFQA_VERDICT_HDR, &v);
        let _ = nl_send(&self.fd, &nl_msg(NFQ_VERDICT, NLM_F_REQUEST, &body));
    }
}

/// (packet id, payload) of an NFQNL_MSG_PACKET message body.
fn queued_packet(p: &[u8]) -> Option<(u32, &[u8])> {
    let (mut a, mut id, mut payload) = (p.get(4..)?, None, &[][..]);
    while a.len() >= 4 {
        let len = usize::from(u16::from_ne_bytes([a[0], a[1]]));
        if len < 4 || len > a.len() { break; }
        match u16::from_ne_bytes([a[2], a[3]]) & 0x3fff {
            NFQA_PACKET_HDR if len >= 8 => id = Some(u32::from_be_bytes([a[4], a[5], a[6], a[7]])),
            NFQA_PAYLOAD => payload = &a[4..len],
            _ => {}
        }
        a = &a[((len + 3) & !3).min(a.len())..];
    }
    Some((id?, payload))
}

// ── daemon ─────────────────────────────────────────────────────────────────

static STOP: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);
static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

fn poke() {
    let fd = WAKE_FD.load(Ordering::SeqCst);
    if fd >= 0 { unsafe { libc::write(fd, b"x".as_ptr().cast(), 1); } }
}
extern "C" fn on_stop(_: libc::c_int) { STOP.store(true, Ordering::SeqCst); poke(); }
extern "C" fn on_reload(_: libc::c_int) { RELOAD.store(true, Ordering::SeqCst); poke(); }
fn handler(f: extern "C" fn(libc::c_int)) -> libc::sighandler_t { f as *const () as libc::sighandler_t }

fn now_secs() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) }

struct Guard {
    cfg: Config,
    diag: Diag,
    wine: HashSet<i32>,                    // pids running a Wine loader, as of the last scan
    idents: HashMap<i32, (u64, String)>,   // pid → (start time, .exe path)
    scanned: Instant,
    last_pid: String,
    logged: HashMap<String, Instant>,      // same program → same destination: one line a minute
}

/// Newest pid the kernel handed out (5th field of /proc/loadavg).
fn newest_pid() -> String {
    std::fs::read_to_string("/proc/loadavg").ok().and_then(|s| s.split_whitespace().nth(4).map(str::to_owned)).unwrap_or_default()
}

impl Guard {
    /// The Wine process holding socket `inode`, if any.
    fn owner(&mut self, inode: u32) -> Option<i32> {
        let has = |p: &i32| socket_inodes(*p).any(|i| i == inode);
        if let Some(&p) = self.wine.iter().find(|p| has(p)) {
            if exe_of(p).map_or(false, |e| is_wine_loader(&e)) { return Some(p); }  // pid not recycled
        }
        // No process was created since the last scan (and it is fresh): the set is complete.
        let np = newest_pid();
        if np == self.last_pid && !np.is_empty() && self.scanned.elapsed() < Duration::from_millis(250) { return None; }
        let fresh: HashSet<i32> = pids().filter(|&p| exe_of(p).map_or(false, |e| is_wine_loader(&e))).collect();
        let old = std::mem::replace(&mut self.wine, fresh);
        self.scanned = Instant::now();
        self.last_pid = np;
        self.idents.retain(|p, _| self.wine.contains(p));
        self.wine.iter().copied().find(|p| !old.contains(p) && has(p))
    }

    fn ident(&mut self, pid: i32) -> String {
        let start = crate::proc_start_time(pid).unwrap_or(0);
        if let Some((s, id)) = self.idents.get(&pid) { if *s == start { return id.clone(); } }
        let id = wine_ident(pid, &exe_of(pid).unwrap_or_default());
        if id.to_lowercase().ends_with(".exe") { self.idents.insert(pid, (start, id.clone())); }
        id
    }

    /// true = let the packet through.
    fn decide(&mut self, pkt: &[u8]) -> bool {
        if !self.cfg.guard { return true; }
        let Some(f) = parse_packet(pkt) else { return true };
        let Some(s) = self.diag.find_socket(&f) else { return true };
        if s.inode == 0 { return true; }
        let Some(pid) = self.owner(s.inode) else { return true };
        let ident = self.ident(pid);
        if self.cfg.whitelisted(&ident) { return true; }
        let killed = self.diag.destroy(&s).is_ok();
        self.log(&f, pkt, pid, &ident, killed);
        false
    }

    fn log(&mut self, f: &Flow, pkt: &[u8], pid: i32, ident: &str, killed: bool) {
        let proto = if f.proto == TCP { "tcp" } else { "udp" };
        let key = format!("{ident}|{proto}|{}|{}", f.dst, f.dport);
        if self.logged.get(&key).map_or(false, |t| t.elapsed() < Duration::from_secs(60)) { return; }
        if self.logged.len() > 4096 { self.logged.clear(); }
        self.logged.insert(key, Instant::now());
        let host = if f.proto == UDP && f.dport == 53 { pkt.get(f.l4 + 8..).and_then(dns_qname) } else { None };
        let line = json!({"ts": now_secs(), "proto": proto, "dst": f.dst.to_string(), "dport": f.dport, "pid": pid,
                          "exe": ident, "host": host.unwrap_or_default(), "killed": killed});
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o755).create(LOG_DIR);
        if std::fs::metadata(LOG_FILE).map_or(false, |m| m.len() > LOG_MAX) { let _ = std::fs::rename(LOG_FILE, format!("{LOG_FILE}.1")); }
        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).create(true).mode(0o644)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(LOG_FILE) {
            let _ = file.write_all(format!("{line}\n").as_bytes());
        }
    }
}

/// Takes the daemon lock and records our pid; None if another daemon holds it.
fn lock_daemon() -> Option<std::fs::File> {
    crate::secure_dir(RUN_DIR).ok()?;
    let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(LOCK_FILE).ok()?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 { return None; }
    f.set_len(0).ok()?;
    f.write_all(format!("{}\n", std::process::id()).as_bytes()).ok()?;
    Some(f)
}

/// Pid of the running daemon (its lock is held), readable by any user.
pub fn daemon_pid() -> Option<i32> {
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(LOCK_FILE).ok()?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 { return None; }  // nobody holds it
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    s.trim().parse().ok().filter(|&p| p > 1)
}

fn apply_rules(cfg: &Config) {
    if let Err(e) = nft_run(&nft_script(cfg)) { eprintln!("lpm-netguard: {e}"); }
}

pub fn run_daemon() -> i32 {
    if unsafe { libc::geteuid() } != 0 { eprintln!("lpm-netguard: the daemon must run as root"); return 1; }
    let Some(_lock) = lock_daemon() else { eprintln!("lpm-netguard: already running (or {LOCK_FILE} is not usable)"); return 1; };
    let mut pipe = [-1 as libc::c_int; 2];
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 { return 1; }
    WAKE_FD.store(pipe[1], Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGTERM, handler(on_stop));
        libc::signal(libc::SIGINT, handler(on_stop));
        libc::signal(libc::SIGHUP, handler(on_reload));
    }
    let cfg = Config::load().unwrap_or_else(|e| { eprintln!("lpm-netguard: {e}; using defaults"); Config::default() });
    let diag = match Diag::open().and_then(|d| d.dump(AF_INET, TCP, !0, 1).map(|_| d)) {
        Ok(d) => d,
        Err(e) => { eprintln!("lpm-netguard: sock_diag unavailable: {e} (kernel: INET_DIAG, INET_TCP_DIAG, INET_UDP_DIAG)"); return 1; }
    };
    // A table left by a killed daemon must not feed packets into a queue that is still being set up.
    nft_remove();
    let queue = match Queue::open() {
        Ok(q) => q,
        Err(e) => { eprintln!("lpm-netguard: cannot bind NFQUEUE {QUEUE_NUM}: {e} (kernel: NETFILTER_NETLINK_QUEUE)"); return 1; }
    };
    if let Err(e) = nft_run(&nft_script(&cfg)) {
        eprintln!("lpm-netguard: {e} (kernel: NF_TABLES, NF_TABLES_INET, NFT_QUEUE, NFT_CT, NF_CONNTRACK)");
        return 1;
    }
    eprintln!("lpm-netguard: started (guard {}, {} blacklisted, {} whitelisted)",
              if cfg.guard { "on" } else { "off" }, cfg.blacklist.len(), cfg.whitelist.len());
    let mut g = Guard { cfg, diag, wine: HashSet::new(), idents: HashMap::new(), scanned: Instant::now(),
                        last_pid: String::new(), logged: HashMap::new() };
    let mut buf = [0u8; 8192];
    while !STOP.load(Ordering::SeqCst) {
        if RELOAD.swap(false, Ordering::SeqCst) {
            match Config::load() {
                Ok(c) => { g.cfg = c; apply_rules(&g.cfg); eprintln!("lpm-netguard: configuration reloaded"); }
                Err(e) => eprintln!("lpm-netguard: reload failed, keeping the old rules: {e}"),
            }
        }
        let mut fds = [libc::pollfd { fd: queue.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                       libc::pollfd { fd: pipe[0], events: libc::POLLIN, revents: 0 }];
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 { continue; }  // EINTR: flags are checked above
        if fds[1].revents != 0 { let mut b = [0u8; 64]; unsafe { libc::read(pipe[0], b.as_mut_ptr().cast(), b.len()); } }
        if fds[0].revents == 0 { continue; }
        for _ in 0..64 {  // then back to poll, so a flood cannot starve SIGTERM/SIGHUP
            let Ok(n) = nl_recv(&queue.fd, &mut buf, libc::MSG_DONTWAIT) else { break };
            let mut pkts: Vec<(u32, bool)> = Vec::new();
            nl_each(&buf[..n], |ty, p| if ty == NFQ_PACKET {
                if let Some((id, payload)) = queued_packet(p) { pkts.push((id, g.decide(payload))); }
            });
            for (id, accept) in pkts { queue.verdict(id, accept); }
        }
    }
    nft_remove();
    eprintln!("lpm-netguard: stopped");
    0
}

// ── helper ops (netguard-helper) ───────────────────────────────────────────

fn state_name(s: &Sock) -> &'static str {
    if s.proto == UDP { return if s.dport != 0 { "CONNECTED" } else { "UNCONN" }; }
    match s.state { 1 => "ESTAB", 2 => "SYN-SENT", 3 => "SYN-RECV", 4 => "FIN-WAIT-1", 5 => "FIN-WAIT-2", 6 => "TIME-WAIT",
                    7 => "CLOSE", 8 => "CLOSE-WAIT", 9 => "LAST-ACK", 10 => "LISTEN", 11 => "CLOSING", _ => "?" }
}

fn dump_all(d: &Diag) -> Result<Vec<Sock>, String> {
    let mut out = Vec::new();
    let mut err = None;
    for (fam, proto) in [(AF_INET, TCP), (AF_INET6, TCP), (AF_INET, UDP), (AF_INET6, UDP)] {
        match d.dump(fam, proto, !0, 0) { Ok(v) => out.extend(v), Err(e) => err = Some(e) }
    }
    match err { Some(e) if out.is_empty() => Err(format!("sock_diag: {e} (kernel: INET_DIAG, INET_TCP_DIAG, INET_UDP_DIAG)")), _ => Ok(out) }
}

/// Every TCP/UDP socket with its owner. Unprivileged: owners of the caller's own processes only.
pub fn list() -> Value {
    let cfg = Config::load().unwrap_or_default();
    let socks = match Diag::open().map_err(|e| format!("sock_diag: {e}")).and_then(|d| dump_all(&d)) {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let want: HashSet<u32> = socks.iter().map(|s| s.inode).filter(|&i| i != 0).collect();
    let owners = owners_of(&want);
    let mut procs: HashMap<i32, (String, String, bool)> = HashMap::new();  // pid → (name, exe, wine)
    let conns: Vec<Value> = socks.iter().map(|s| {
        let pid = owners.get(&s.inode).copied().unwrap_or(0);
        let (name, exe, wine) = if pid == 0 { (String::new(), String::new(), false) } else {
            procs.entry(pid).or_insert_with(|| {
                let exe = exe_of(pid).unwrap_or_default();
                if is_wine_loader(&exe) { let id = wine_ident(pid, &exe); (basename(&id).to_owned(), id, true) }
                else { (comm(pid), exe, false) }
            }).clone()
        };
        json!({"proto": if s.proto == TCP { "tcp" } else { "udp" }, "state": state_name(s),
               "local": s.src.to_string(), "lport": s.sport,
               "remote": if s.dport == 0 { String::new() } else { s.dst.to_string() }, "rport": s.dport,
               "uid": s.uid, "pid": pid, "name": name, "exe": exe, "wine": wine,
               "bl": s.dport != 0 && cfg.blacklisted(s.dst)})
    }).collect();
    json!({"ok": true, "root": unsafe { libc::geteuid() } == 0, "conns": conns})
}

/// Kernel options from KCONFIG the running kernel lacks; None when its config is not readable.
fn kconfig_missing() -> Option<Vec<&'static str>> {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut u); }
    let rel = unsafe { std::ffi::CStr::from_ptr(u.release.as_ptr()) }.to_string_lossy().into_owned();
    let text = sys_bin("gzip").filter(|_| Path::new("/proc/config.gz").exists())
        .and_then(|gz| std::process::Command::new(gz).args(["-dc", "/proc/config.gz"]).env_clear().output().ok())
        .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .or_else(|| std::fs::read_to_string(format!("/boot/config-{rel}")).ok())?;
    let set: HashSet<&str> = text.lines().filter_map(|l| l.strip_prefix("CONFIG_")?.split_once('=').map(|(k, _)| k)).collect();
    Some(KCONFIG.iter().copied().filter(|k| !set.contains(k)).collect())
}

pub fn status(full: bool) -> Value {
    let (cfg, err) = match Config::load() { Ok(c) => (c, None), Err(e) => (Config::default(), Some(e)) };
    let mut v = cfg.to_json();
    let pid = daemon_pid();
    v["ok"] = json!(true);
    v["running"] = json!(pid.is_some());
    v["pid"] = json!(pid);
    if let Some(e) = err { v["config_error"] = json!(e); }
    if full {
        v["nft"] = json!(sys_bin("nft").is_some());
        if let Some(m) = kconfig_missing() { v["missing"] = json!(m); }
    }
    v
}

fn reload_daemon() {
    if let Some(p) = daemon_pid() { unsafe { libc::kill(p, libc::SIGHUP); } }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.trim().to_owned())).collect()).unwrap_or_default()
}

/// Aborts every live socket whose peer matches. Returns (destroyed, last error).
fn kill_where(pred: impl Fn(&Sock) -> bool) -> (usize, Option<String>) {
    let Ok(d) = Diag::open() else { return (0, Some("sock_diag unavailable".into())) };
    let (mut n, mut err) = (0, None);
    for s in dump_all(&d).unwrap_or_default().iter().filter(|s| s.dport != 0 && pred(s)) {
        match d.destroy(s) {
            Ok(()) => n += 1,
            Err(e) if e.raw_os_error() == Some(libc::EOPNOTSUPP) => err = Some("kernel lacks CONFIG_INET_DIAG_DESTROY".to_owned()),
            Err(e) => err = Some(e.to_string()),
        }
    }
    (n, err)
}

/// One request of netguard-helper. `list` and `status` work for any user; the rest is root-only.
pub fn handle(req: &Value) -> Value {
    let op = req["op"].as_str().unwrap_or("");
    match op { "list" => return list(), "status" => return status(true), _ => {} }
    if unsafe { libc::geteuid() } != 0 { return json!({"ok": false, "needs_root": true, "error": "this operation needs root (pkexec)"}); }
    let mut cfg = match Config::load() { Ok(c) => c, Err(e) => return json!({"ok": false, "error": e}) };
    let mut killed = 0;
    match op {
        "set" => {
            if let Some(b) = req["guard"].as_bool() { cfg.guard = b; }
            if let Some(b) = req["allow_lan"].as_bool() { cfg.allow_lan = b; }
        }
        "blacklist" => {
            let mut added = Vec::new();
            for s in strings(&req["add"]) {
                let Some(n) = Net::parse(&s) else { return json!({"ok": false, "error": format!("not an IP address or CIDR block: {s}")}) };
                if !cfg.blacklist.contains(&n) { cfg.blacklist.push(n); added.push(n); }
            }
            let del: Vec<Net> = strings(&req["remove"]).iter().filter_map(|s| Net::parse(s)).collect();
            cfg.blacklist.retain(|n| !del.contains(n));
            if cfg.blacklist.len() > MAX_BLACKLIST { return json!({"ok": false, "error": "blacklist is full"}); }
            if !added.is_empty() { killed = kill_where(|s| added.iter().any(|n| n.contains(s.dst))).0; }
        }
        "whitelist" => {
            for s in strings(&req["add"]) {
                if !valid_entry(&s) { return json!({"ok": false, "error": "invalid whitelist entry"}); }
                if !cfg.whitelist.contains(&s) { cfg.whitelist.push(s); }
            }
            let del = strings(&req["remove"]);
            cfg.whitelist.retain(|w| !del.contains(w));
            if cfg.whitelist.len() > MAX_WHITELIST { return json!({"ok": false, "error": "whitelist is full"}); }
        }
        "kill" => {
            let (proto, lport, rport) = (if req["proto"] == "udp" { UDP } else { TCP }, req["lport"].as_u64().unwrap_or(0), req["rport"].as_u64().unwrap_or(0));
            let remote: Option<IpAddr> = req["remote"].as_str().and_then(|s| s.parse().ok());
            let (n, err) = kill_where(|s| s.proto == proto && u64::from(s.sport) == lport && u64::from(s.dport) == rport && Some(s.dst) == remote);
            return match (n, err) {
                (0, Some(e)) => json!({"ok": false, "error": e}),
                (0, None) => json!({"ok": false, "error": "connection no longer exists"}),
                _ => json!({"ok": true, "killed": n}),
            };
        }
        "clear_log" => {
            let _ = std::fs::remove_file(LOG_FILE);
            let _ = std::fs::remove_file(format!("{LOG_FILE}.1"));
            return json!({"ok": true});
        }
        _ => return json!({"ok": false, "error": "unknown op"}),
    }
    if let Err(e) = cfg.save() { return json!({"ok": false, "error": e}); }
    reload_daemon();
    let mut v = status(false);
    v["killed"] = json!(killed);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nets() {
        let n = Net::parse("10.1.2.3/8").unwrap();
        assert_eq!(n.to_string(), "10.0.0.0/8");
        assert!(n.contains("10.200.0.1".parse().unwrap()) && !n.contains("11.0.0.1".parse().unwrap()));
        assert!(n.contains("::ffff:10.9.9.9".parse().unwrap()));
        assert_eq!(Net::parse("1.2.3.4").unwrap().to_string(), "1.2.3.4");
        assert_eq!(Net::parse("2001:db8::1/32").unwrap().to_string(), "2001:db8::/32");
        assert!(Net::parse("0.0.0.0/0").is_none() && Net::parse("1.2.3.4/33").is_none() && Net::parse("x; drop").is_none());
    }

    #[test]
    fn whitelist() {
        let c = Config { whitelist: vec!["Game.exe".into(), "/games/a/".into(), "/opt/x/tool.exe".into()], ..Config::default() };
        assert!(c.whitelisted("/home/u/pfx/drive_c/GAME.EXE") && c.whitelisted("C:\\dir\\game.exe"));
        assert!(c.whitelisted("/games/a/bin/launcher.exe") && c.whitelisted("/opt/x/tool.exe"));
        assert!(!c.whitelisted("/opt/y/tool.exe") && !c.whitelisted("/games/b/launcher.exe"));
    }

    #[test]
    fn packets() {
        let mut p = vec![0x45, 0, 0, 40, 0, 0, 0x40, 0, 64, 17, 0, 0, 192, 168, 1, 5, 8, 8, 8, 8, 0xC0, 0x00, 0, 53, 0, 0, 0, 0];
        p.extend_from_slice(&[0; 12]);
        p.extend_from_slice(b"\x03www\x07example\x03com\x00\x00\x01\x00\x01");
        let f = parse_packet(&p).unwrap();
        assert_eq!((f.proto, f.sport, f.dport, f.dst.to_string().as_str()), (UDP, 0xC000, 53, "8.8.8.8"));
        assert_eq!(dns_qname(&p[f.l4 + 8..]).as_deref(), Some("www.example.com"));
        let mut v6 = vec![0x60, 0, 0, 0, 0, 20, 6, 64];
        v6.extend_from_slice(&[0; 15]); v6.push(1);
        v6.extend_from_slice(&[0x20, 1, 0xd, 0xb8]); v6.extend_from_slice(&[0; 12]);
        v6.extend_from_slice(&[0x9c, 0x40, 0x01, 0xbb]);
        let f = parse_packet(&v6).unwrap();
        assert_eq!((f.proto, f.sport, f.dport, f.dst.to_string().as_str()), (TCP, 40000, 443, "2001:db8::"));
        assert!(parse_packet(&[0x45, 0, 0]).is_none());
    }

    #[test]
    fn nft_rules() {
        let mut c = Config::default();
        assert!(!nft_script(&c).contains("queue") && !nft_script(&c).contains("elements"));
        c.guard = true;
        c.blacklist = vec![Net::parse("1.2.3.4").unwrap(), Net::parse("2001:db8::/32").unwrap()];
        let s = nft_script(&c);
        assert!(s.contains("elements = { 1.2.3.4 }") && s.contains("elements = { 2001:db8::/32 }") && s.contains("queue num 19536 bypass"));
    }

    #[test]
    fn queue_message() {
        let mut body = vec![2, 0, 0x4c, 0x50];
        nl_attr(&mut body, NFQA_PACKET_HDR, &[0, 0, 0, 7, 8, 0, 3]);
        nl_attr(&mut body, NFQA_PAYLOAD, &[0x45, 1, 2]);
        assert_eq!(queued_packet(&body), Some((7, &[0x45u8, 1, 2][..])));
    }
}
