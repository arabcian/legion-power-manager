//! Backup engine behind Health → Backup (backup-helper).
//!
//! Two kinds of archive:
//!
//! * System image — the whole root file system through
//!   `tar --acls --xattrs --xattrs-include='*' -cpf - / | <compressor>`,
//!   written as `backup-YYYY-MM-DD.tar.<ext>` (0600 root: it holds /etc/shadow),
//!   and the matching restore `<decompressor> | tar … -xpf - -C <target>`.
//!   Root only; progress lines are streamed while it runs.
//!
//! * LPM configuration — everything Legion Power Manager has stored (user
//!   config, root-owned presets/boot profiles/NVIDIA curves/calibration) plus an
//!   optional "system profile" (Portage config, world set, kernel config, fstab,
//!   boot/module/sysctl configuration, package list), as one small dated
//!   `lpm-config-YYYY-MM-DD_HHMMSS.tar.gz`. Made unprivileged from readable files.
//!   Restore: the user part unprivileged; the root part through a whitelist,
//!   every file re-created root:root 0644 (owner/mode in the archive are never
//!   trusted). The system profile is reference material and is never restored.
//!
//! Cancel: the GUI closes our stdin; the children (own process groups) are
//! terminated and the partial archive is removed.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The exclude list of the system image. `/x/*` rather than `/x`: the (empty)
/// mount points stay in the archive, so a restore onto a fresh file system
/// still has /proc, /sys, /dev, /run and /tmp to mount on.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "/proc/*", "/sys/*", "/dev/*", "/run/*", "/tmp/*", "/var/tmp/*", "/mnt/*", "/media/*",
    "/lost+found", "/var/cache/*", "/usr/portage/distfiles/*",
];
pub const HOME_EXCLUDE: &str = "/home/*";
/// Mount points created in the restore target when the archive lacks them
/// (archives made with plain `--exclude=/proc`): (path, mode).
const MOUNT_POINTS: &[(&str, u32)] = &[("proc", 0o755), ("sys", 0o755), ("dev", 0o755), ("run", 0o755),
    ("tmp", 0o1777), ("var/tmp", 0o1777), ("mnt", 0o755), ("media", 0o755), ("home", 0o755)];

pub const CONFIG_FORMAT: &str = "legion-power-manager-config-backup";
const MANIFEST: &str = "LPM-BACKUP.json";
/// Root-owned LPM state saved in a configuration backup (relative to /).
const SYS_LPM: &[&str] = &["etc/legion-power-manager", "etc/nvcurve", "var/lib/legion-power-manager",
    "etc/modprobe.d/zz-legion-power-manager-nvidia.conf"];
const SYS_LPM_LOGS: &[&str] = &["var/log/legion-power-manager"];
/// /var/lib/legion-power-manager files worth restoring; the rest (boot guard
/// state, boot-default snapshot, pending GPU mode) belongs to the boot it was made in.
const VARLIB_RESTORE: &[&str] = &["signature.json", "calibration.json", "outcomes.json", "io-probe.json", "wmae-verified.json"];
/// "System profile": how this installation is put together. Reference only.
const PROFILE: &[&str] = &[
    "etc/portage", "var/lib/portage/world", "var/lib/portage/world_sets", "usr/src/linux/.config",
    "etc/fstab", "etc/default/grub", "etc/kernel", "etc/dracut.conf", "etc/dracut.conf.d",
    "etc/modprobe.d", "etc/modules-load.d", "etc/sysctl.conf", "etc/sysctl.d", "etc/conf.d", "etc/rc.conf",
    "etc/local.d", "etc/runlevels", "etc/udev/rules.d", "etc/X11/xorg.conf.d", "etc/environment", "etc/env.d",
    "etc/locale.gen", "etc/hosts", "etc/security/limits.conf", "etc/security/limits.d",
];
/// Left out of the system profile: Portage's own GnuPG home (root 0700). It is
/// the keyring `getuto` regenerates for verifying the tree, not configuration,
/// and its private-key / agent-socket directories must not land in a user file.
const PROFILE_SKIP: &[&str] = &["/etc/portage/gnupg"];
const MAX_FILE: u64 = 32 << 20;       // one staged file
const MAX_STAGE: u64 = 256 << 20;     // a whole configuration backup
const MAX_ROOT_FILE: u64 = 16 << 20;  // one root file on restore

// ── cancel / children ───────────────────────────────────────────────────────

static CANCEL: AtomicBool = AtomicBool::new(false);
static PGIDS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

fn kill_all(sig: libc::c_int) {
    for &p in PGIDS.lock().unwrap_or_else(|e| e.into_inner()).iter() { unsafe { libc::kill(-p, sig); } }
}
fn cancelled() -> bool { CANCEL.load(Ordering::Relaxed) }

/// After the request line: anything else on stdin is ignored, EOF cancels.
pub fn cancel_on_stdin_eof() {
    std::thread::spawn(|| {
        let mut b = [0u8; 256];
        while matches!(std::io::stdin().read(&mut b), Ok(n) if n > 0) {}
        CANCEL.store(true, Ordering::Relaxed);
        kill_all(libc::SIGTERM);
    });
}

fn is_root() -> bool { unsafe { libc::geteuid() == 0 } }

/// tar / compressors from the system directories only. As root the binary and
/// every directory above it must be root-owned and not group/other-writable.
fn tool(name: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"].iter().map(|d| Path::new(d).join(name))
        .find(|p| if is_root() { crate::trusted_path(p) } else { p.is_file() })
}

fn command(path: &Path, low_priority: bool) -> Command {
    let mut c = Command::new(path);
    // argv[0] = bare name: tar prefixes its messages with it ("tar: …"), which is
    // how the readers tell messages from member names.
    if let Some(n) = path.file_name() { c.arg0(n); }
    c.env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").env("LC_ALL", "C").process_group(0);
    if low_priority {
        unsafe {
            c.pre_exec(|| {
                libc::setpriority(libc::PRIO_PROCESS as _, 0, 19);
                libc::syscall(libc::SYS_ioprio_set, 1, 0, (2 << 13) | 7);  // best-effort, lowest level
                Ok(())
            });
        }
    }
    c
}

fn spawn(mut c: Command, what: &str) -> Result<Child, String> {
    let child = c.spawn().map_err(|e| format!("{what}: {e}"))?;
    PGIDS.lock().unwrap_or_else(|e| e.into_inner()).push(child.id() as i32);
    Ok(child)  // `c` (and the pipe ends it holds) is dropped here
}

// ── progress shared with the reader threads ─────────────────────────────────

#[derive(Default)]
struct Progress {
    files: AtomicU64,
    current: Mutex<String>,
    msg_count: AtomicU64,
    msgs: Mutex<VecDeque<String>>,   // last lines tar / the compressor printed
}

impl Progress {
    fn message(&self, line: String) {
        if line.contains("Removing leading") { return; }  // tar's note about "/" member names
        self.msg_count.fetch_add(1, Ordering::Relaxed);
        let mut m = self.msgs.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= 60 { m.pop_front(); }
        m.push_back(line);
    }
    fn tail(&self) -> Vec<String> { self.msgs.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect() }
    fn has(&self, needle: &str) -> bool { self.msgs.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|l| l.contains(needle)) }
}

/// Lines of a child's stream: "tar: …" (or everything, with `names` false) are
/// messages; the rest are member names from -v / -t.
fn reader<R: Read + Send + 'static>(stream: R, p: Arc<Progress>, names: bool) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for raw in BufReader::with_capacity(64 * 1024, stream).split(b'\n').flatten() {
            if names && !raw.starts_with(b"tar: ") {
                p.files.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut c) = p.current.try_lock() { *c = String::from_utf8_lossy(&raw[..raw.len().min(300)]).into_owned(); }
            } else if !raw.is_empty() {
                p.message(String::from_utf8_lossy(&raw[..raw.len().min(500)]).into_owned());
            }
        }
    })
}

/// Waits for every child, calling `tick` about once a second. A cancel sends
/// SIGTERM (done by the stdin watcher), then SIGKILL after 5 s.
fn wait_all(children: &mut [Child], mut tick: impl FnMut()) -> Vec<ExitStatus> {
    let mut st: Vec<Option<ExitStatus>> = children.iter().map(|_| None).collect();
    let mut since_tick = Instant::now();
    let mut cancel_at: Option<Instant> = None;
    loop {
        for (i, c) in children.iter_mut().enumerate() {
            if st[i].is_none() { if let Ok(Some(s)) = c.try_wait() { st[i] = Some(s); } }
        }
        if st.iter().all(Option::is_some) { break; }
        if cancelled() {
            let t = *cancel_at.get_or_insert_with(|| { kill_all(libc::SIGTERM); Instant::now() });
            if t.elapsed() > Duration::from_secs(5) { kill_all(libc::SIGKILL); }
        }
        if since_tick.elapsed() >= Duration::from_secs(1) { since_tick = Instant::now(); tick(); }
        std::thread::sleep(Duration::from_millis(100));
    }
    PGIDS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    st.into_iter().map(|s| s.unwrap()).collect()
}

fn fail(msg: impl Into<String>) -> Value {
    json!({"ok": false, "error": msg.into(), "cancelled": cancelled()})
}

// ── small helpers ───────────────────────────────────────────────────────────

struct Stamp { date: String, time: String, iso: String }

fn stamp() -> Stamp {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    unsafe { libc::localtime_r(&t, &mut tm) };
    let off = tm.tm_gmtoff;
    let date = format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday);
    Stamp {
        time: format!("{:02}{:02}{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
        iso: format!("{date}T{:02}:{:02}:{:02}{}{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec,
                     if off < 0 { '-' } else { '+' }, off.abs() / 3600, off.abs() / 60 % 60),
        date,
    }
}

fn uname() -> (String, String) {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 { return (String::new(), String::new()); }
    let s = |p: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(p.as_ptr()) }.to_string_lossy().into_owned();
    (s(&u.nodename), s(&u.release))
}

fn free_bytes(dir: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
}

fn abs_dir(v: &Value, what: &str) -> Result<PathBuf, String> {
    let s = v.as_str().filter(|s| s.starts_with('/') && s.len() < 1024 && !s.contains('\0'))
        .ok_or_else(|| format!("{what}: an absolute path is required"))?;
    let p = fs::canonicalize(s).map_err(|e| format!("{s}: {e}"))?;
    if p.is_dir() { Ok(p) } else { Err(format!("{}: not a directory", p.display())) }
}

fn abs_file(v: &Value) -> Result<PathBuf, String> {
    let s = v.as_str().filter(|s| s.starts_with('/') && s.len() < 1024 && !s.contains('\0'))
        .ok_or("archive: an absolute path is required")?;
    let p = fs::canonicalize(s).map_err(|e| format!("{s}: {e}"))?;
    if p.is_file() { Ok(p) } else { Err(format!("{}: not a regular file", p.display())) }
}

/// Escapes tar's exclude wildcards in a literal path.
fn literal(p: &Path) -> String {
    let mut o = String::new();
    for c in p.to_string_lossy().chars() { if matches!(c, '*' | '?' | '[' | '\\') { o.push('\\'); } o.push(c); }
    o
}

fn fsync_dir(dir: &Path) {
    if let Ok(d) = fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC).open(dir) { let _ = d.sync_all(); }
}

/// 0700 scratch directory `<base>/<prefix>XXXXXX`.
fn mkdtemp(base: &Path, prefix: &str) -> Result<PathBuf, String> {
    let t = base.join(format!("{prefix}XXXXXX"));
    let mut c = std::ffi::CString::new(t.as_os_str().as_bytes()).map_err(|e| e.to_string())?.into_bytes_with_nul();
    if unsafe { libc::mkdtemp(c.as_mut_ptr() as *mut libc::c_char) }.is_null() {
        return Err(format!("{}: {}", t.display(), std::io::Error::last_os_error()));
    }
    c.pop();
    Ok(PathBuf::from(std::ffi::OsString::from_vec(c)))
}

struct Scratch(PathBuf);
impl Drop for Scratch { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

// ── compression ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Comp { Gzip, Zstd, Xz, Bzip2, None }

impl Comp {
    fn from_magic(f: &File) -> Comp {
        use std::os::unix::fs::FileExt;
        let mut m = [0u8; 6];
        let n = f.read_at(&mut m, 0).unwrap_or(0);
        match &m[..n] {
            [0x1f, 0x8b, ..] => Comp::Gzip,
            [0x28, 0xb5, 0x2f, 0xfd, ..] => Comp::Zstd,
            [0xfd, b'7', b'z', b'X', b'Z', 0] => Comp::Xz,
            [b'B', b'Z', b'h', ..] => Comp::Bzip2,
            _ => Comp::None,
        }
    }
    /// Decompressor writing to stdout.
    fn decompressor(self) -> Result<(PathBuf, Vec<&'static str>), String> {
        let (names, args): (&[&str], Vec<&str>) = match self {
            Comp::Gzip => (&["pigz", "gzip"], vec!["-dc"]),
            Comp::Zstd => (&["zstd"], vec!["-dc", "-T0"]),
            Comp::Xz => (&["xz"], vec!["-dc", "-T0"]),
            Comp::Bzip2 => (&["pbzip2", "bzip2"], vec!["-dc"]),
            Comp::None => return Err("not compressed".into()),
        };
        names.iter().find_map(|n| tool(n)).map(|p| (p, args))
            .ok_or_else(|| format!("{} is not installed", names[0]))
    }
}

// ── system image: create ────────────────────────────────────────────────────

/// {"op":"system_backup","dest_dir":"/…","compressor":"pigz|zstd|xz","level":6,"threads":0,
///  "include_home":false,"excludes":[…],"low_priority":true,"verify":true,"keep":0}
pub fn system_backup(req: &Value) -> Value {
    match system_backup_inner(req) { Ok(v) => v, Err(e) => fail(e) }
}

fn system_backup_inner(req: &Value) -> Result<Value, String> {
    let dest = abs_dir(&req["dest_dir"], "dest_dir")?;
    let tar = tool("tar").ok_or("tar is not installed")?;
    let level = req["level"].as_i64();
    let threads = req["threads"].as_u64().filter(|&t| (1..=256).contains(&t))
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get() as u64));
    let lvl = |lo: i64, hi: i64, def: i64| level.filter(|l| (lo..=hi).contains(l)).unwrap_or(def);
    let (comp_path, comp_args, ext): (PathBuf, Vec<String>, &str) = match req["compressor"].as_str().unwrap_or("pigz") {
        "pigz" | "gzip" => match tool("pigz") {
            Some(p) => (p, vec![format!("-p{threads}"), format!("-{}", lvl(1, 9, 6))], "gz"),
            None => (tool("gzip").ok_or("neither pigz nor gzip is installed")?, vec![format!("-{}", lvl(1, 9, 6))], "gz"),
        },
        "zstd" => (tool("zstd").ok_or("zstd is not installed")?,
                   vec![format!("-T{threads}"), format!("-{}", lvl(1, 19, 3)), "-q".into()], "zst"),
        "xz" => (tool("xz").ok_or("xz is not installed")?, vec![format!("-T{threads}"), format!("-{}", lvl(0, 9, 6))], "xz"),
        o => return Err(format!("unknown compressor: {o}")),
    };
    let mut excludes: Vec<String> = match req["excludes"].as_array() {
        Some(a) => {
            if a.len() > 128 { return Err("too many exclude patterns".into()); }
            a.iter().map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 512 && !s.contains(['\0', '\n']))
                .map(str::to_owned).ok_or_else(|| "invalid exclude pattern".to_string())).collect::<Result<_, _>>()?
        }
        None => DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect(),
    };
    if !req["include_home"].as_bool().unwrap_or(false) { excludes.push(HOME_EXCLUDE.into()); }
    let low = req["low_priority"].as_bool().unwrap_or(true);

    if free_bytes(&dest).is_some_and(|f| f < 1 << 30) {
        return Err(format!("less than 1 GiB free in {}", dest.display()));
    }
    let st = stamp();
    let mut name = format!("backup-{}.tar.{ext}", st.date);
    if dest.join(&name).exists() { name = format!("backup-{}_{}.tar.{ext}", st.date, st.time); }
    let fin = dest.join(&name);
    let partial = dest.join(format!("{name}.partial"));
    // The archive must not swallow itself or the older images next to it.
    excludes.push(literal(&fin));
    excludes.push(literal(&partial));
    excludes.push(format!("{}/backup-*.tar.*", literal(&dest).trim_end_matches('/')));

    let out = fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&partial)
        .map_err(|e| format!("{}: {e}", partial.display()))?;
    struct Partial<'a>(&'a Path, bool);
    impl Drop for Partial<'_> { fn drop(&mut self) { if !self.1 { let _ = fs::remove_file(self.0); } } }
    let mut guard = Partial(&partial, false);

    let mut tc = command(&tar, low);
    tc.current_dir("/").args(["--acls", "--xattrs", "--xattrs-include=*", "--ignore-failed-read"]);
    for e in &excludes { tc.arg(format!("--exclude={e}")); }
    tc.args(["-cvpf", "-", "/"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::emit(&json!({"event": "start", "phase": "backup", "archive": fin, "excludes": excludes,
                        "command": format!("tar --acls --xattrs --xattrs-include='*' --exclude=… -cpf - / | {} {}",
                                           comp_path.display(), comp_args.join(" "))}));
    let mut t = spawn(tc, "tar")?;
    let prog = Arc::new(Progress::default());
    let r1 = reader(t.stderr.take().unwrap(), prog.clone(), true);
    let mut cc = command(&comp_path, low);
    cc.args(&comp_args).stdin(Stdio::from(t.stdout.take().unwrap()))
        .stdout(Stdio::from(out.try_clone().map_err(|e| e.to_string())?)).stderr(Stdio::piped());
    let mut c = match spawn(cc, "compressor") {
        Ok(c) => c,
        Err(e) => { kill_all(libc::SIGKILL); let _ = t.wait(); return Err(e); }
    };
    let r2 = reader(c.stderr.take().unwrap(), prog.clone(), false);

    let started = Instant::now();
    let mut kids = [t, c];
    let status = wait_all(&mut kids, || {
        crate::emit(&json!({"event": "progress", "phase": "backup", "bytes": out.metadata().map_or(0, |m| m.len()),
                            "files": prog.files.load(Ordering::Relaxed), "current": prog.current.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                            "elapsed": started.elapsed().as_secs()}));
    });
    let _ = (r1.join(), r2.join());
    if cancelled() { return Err("cancelled — the partial archive was removed".into()); }
    let (ts, cs) = (status[0], status[1]);
    if !cs.success() {
        return Err(format!("the compressor failed ({cs}){}", prog.tail().last().map(|l| format!(": {l}")).unwrap_or_default()));
    }
    // tar: 0 = clean, 1 = files changed while being read (normal on a live
    // system), 2 = errors on some files — the archive is still usable, flagged.
    let tcode = ts.code().ok_or_else(|| format!("tar was killed ({ts})"))?;
    if tcode > 2 { return Err(format!("tar failed with status {tcode}")); }
    out.sync_all().map_err(|e| format!("{}: {e}", partial.display()))?;
    let bytes = out.metadata().map_or(0, |m| m.len());
    drop(out);
    fs::rename(&partial, &fin).map_err(|e| format!("{}: {e}", fin.display()))?;
    guard.1 = true;
    fsync_dir(&dest);

    let files = prog.files.load(Ordering::Relaxed);
    let (host, kernel) = uname();
    let mut info = json!({"format": "legion-power-manager-system-backup", "version": 1, "created": st.iso,
        "host": host, "kernel": kernel, "archive": name, "bytes": bytes, "files": files,
        "compressor": comp_path.file_name().map(|n| n.to_string_lossy().into_owned()), "excludes": excludes,
        "include_home": req["include_home"].as_bool().unwrap_or(false), "tar_status": tcode,
        "messages": prog.msg_count.load(Ordering::Relaxed)});
    let side = sidecar(&fin);
    let _ = crate::write_root_file(&side, info.to_string().as_bytes());

    let mut verified = Value::Null;
    if req["verify"].as_bool().unwrap_or(false) {
        match read_archive(&fin, None, low) {
            Ok(r) if r.files == files => {
                verified = json!(true);
                info["verified"] = json!(stamp().iso);
                let _ = crate::write_root_file(&side, info.to_string().as_bytes());
            }
            Ok(r) => return Err(format!("verification failed: {} entries written, {} read back — {} was kept, do not rely on it",
                                        files, r.files, fin.display())),
            Err(e) => return Err(format!("verification failed: {e} — {} was kept, do not rely on it", fin.display())),
        }
    }
    let removed = prune(&dest, req["keep"].as_u64().unwrap_or(0) as usize, &name);
    Ok(json!({"ok": true, "archive": fin, "bytes": bytes, "files": files, "tar_status": tcode,
              "degraded": tcode == 2, "verified": verified, "removed": removed,
              "message_count": prog.msg_count.load(Ordering::Relaxed), "messages": prog.tail(),
              "elapsed": started.elapsed().as_secs()}))
}

fn sidecar(archive: &Path) -> String { format!("{}.info.json", archive.display()) }

/// `backup-YYYY-MM-DD[_HHMMSS].tar.<ext>` — the only names retention ever deletes.
fn is_image_name(n: &str) -> bool {
    let Some(rest) = n.strip_prefix("backup-") else { return false };
    let b = rest.as_bytes();
    b.len() > 10 && b[..10].iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
        && [".tar.gz", ".tar.zst", ".tar.xz"].iter().any(|e| {
            rest[10..].strip_suffix(e).is_some_and(|mid| mid.is_empty()
                || (mid.len() == 7 && mid.starts_with('_') && mid[1..].bytes().all(|c| c.is_ascii_digit())))
        })
}

/// Keeps the newest `keep` images in `dir` (0 = keep everything).
fn prune(dir: &Path, keep: usize, just_made: &str) -> Vec<String> {
    if keep == 0 { return Vec::new(); }
    let mut imgs: Vec<(std::time::SystemTime, String)> = fs::read_dir(dir).into_iter().flatten().flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let m = fs::symlink_metadata(e.path()).ok()?;
            (m.is_file() && is_image_name(&n)).then(|| (m.modified().unwrap_or(std::time::UNIX_EPOCH), n))
        }).collect();
    imgs.sort_by(|a, b| b.0.cmp(&a.0));
    let mut removed = Vec::new();
    for (_, n) in imgs.into_iter().filter(|(_, n)| n != just_made).skip(keep.saturating_sub(1)) {
        if fs::remove_file(dir.join(&n)).is_ok() {
            let _ = fs::remove_file(sidecar(&dir.join(&n)));
            removed.push(n);
        }
    }
    removed
}

// ── system image: verify / restore ──────────────────────────────────────────

struct ReadResult { files: u64, tar_status: i32, prog: Arc<Progress>, elapsed: u64 }

/// `<decompressor> | tar -t` (target None) or `… | tar -xp -C target`.
/// Compression is taken from the file's magic bytes, not its name.
fn read_archive(archive: &Path, target: Option<&Path>, low: bool) -> Result<ReadResult, String> {
    let tar = tool("tar").ok_or("tar is not installed")?;
    let file = File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let total = file.metadata().map_or(0, |m| m.len());
    let comp = Comp::from_magic(&file);
    let phase = if target.is_some() { "restore" } else { "verify" };
    let prog = Arc::new(Progress::default());
    let mut kids: Vec<Child> = Vec::new();
    let mut readers = Vec::new();
    // The children read through a duplicate of `file`: one shared offset, so
    // our own descriptor tells how far into the archive they are.
    let src = Stdio::from(file.try_clone().map_err(|e| e.to_string())?);
    let tar_in = if comp == Comp::None { src } else {
        let (path, args) = comp.decompressor()?;
        let mut dc = command(&path, low);
        dc.args(&args).stdin(src).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut d = spawn(dc, "decompressor")?;
        readers.push(reader(d.stderr.take().unwrap(), prog.clone(), false));
        let o = Stdio::from(d.stdout.take().unwrap());
        kids.push(d);
        o
    };
    let mut tc = command(&tar, low);
    match target {
        Some(t) => { tc.args(["--acls", "--xattrs", "--xattrs-include=*", "--numeric-owner", "-xvpf", "-", "-C"]).arg(t); }
        None => { tc.args(["-tf", "-"]); }
    }
    tc.stdin(tar_in).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut t = match spawn(tc, "tar") {
        Ok(t) => t,
        Err(e) => { kill_all(libc::SIGKILL); for k in &mut kids { let _ = k.wait(); } return Err(e); }
    };
    readers.push(reader(t.stdout.take().unwrap(), prog.clone(), true));
    readers.push(reader(t.stderr.take().unwrap(), prog.clone(), false));
    kids.push(t);

    let started = Instant::now();
    let fd = file.as_raw_fd();
    let status = wait_all(&mut kids, || {
        let pos = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) }.max(0) as u64;
        crate::emit(&json!({"event": "progress", "phase": phase, "bytes": pos, "total": total,
                            "files": prog.files.load(Ordering::Relaxed), "current": prog.current.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                            "elapsed": started.elapsed().as_secs()}));
    });
    for r in readers { let _ = r.join(); }
    if cancelled() { return Err("cancelled".into()); }
    let ts = *status.last().unwrap();
    let tcode = ts.code().ok_or_else(|| format!("tar was killed ({ts})"))?;
    let last = || prog.tail().last().map(|l| format!(": {l}")).unwrap_or_default();
    if status.len() == 2 {
        let ds = status[0];
        // gzip exits 2 for warnings; a decompressor cut off by SIGPIPE after tar
        // finished cleanly had only padding left to write.
        let fine = ds.success() || (comp == Comp::Gzip && ds.code() == Some(2)) || (ds.signal() == Some(libc::SIGPIPE) && tcode == 0);
        if !fine { return Err(format!("the archive is damaged or truncated (decompressor: {ds}){}", last())); }
    }
    if prog.has("Unexpected EOF") || prog.has("does not look like a tar archive") {
        return Err(format!("the archive is damaged or truncated{}", last()));
    }
    if tcode != 0 && (target.is_none() || tcode != 2) { return Err(format!("tar failed with status {tcode}{}", last())); }
    Ok(ReadResult { files: prog.files.load(Ordering::Relaxed), tar_status: tcode, prog, elapsed: started.elapsed().as_secs() })
}

/// {"op":"verify","archive":"/…"} — reads the whole archive back (compressed
/// stream + tar structure) and compares the entry count with the one recorded
/// when it was made.
pub fn verify(req: &Value) -> Value {
    let archive = match abs_file(&req["archive"]) { Ok(a) => a, Err(e) => return fail(e) };
    crate::emit(&json!({"event": "start", "phase": "verify", "archive": archive}));
    let r = match read_archive(&archive, None, req["low_priority"].as_bool().unwrap_or(true)) { Ok(r) => r, Err(e) => return fail(e) };
    let side = sidecar(&archive);
    let mut info = fs::read_to_string(&side).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok());
    if let Some(expect) = info.as_ref().and_then(|i| i["files"].as_u64()) {
        if expect != r.files { return fail(format!("{} entries recorded when the backup was made, {} read back", expect, r.files)); }
    }
    if let Some(i) = info.as_mut().filter(|_| is_root()) {
        i["verified"] = json!(stamp().iso);
        let _ = crate::write_root_file(&side, i.to_string().as_bytes());
    }
    json!({"ok": true, "archive": archive, "files": r.files, "compared": info.is_some(), "elapsed": r.elapsed})
}

/// {"op":"system_restore","archive":"/…","target":"/mnt/gentoo","confirm":"/mnt/gentoo"}
/// `confirm` must repeat the target: a request cannot restore somewhere by accident.
pub fn system_restore(req: &Value) -> Value {
    let (archive, target) = match (abs_file(&req["archive"]), abs_dir(&req["target"], "target")) {
        (Ok(a), Ok(t)) => (a, t),
        (Err(e), _) | (_, Err(e)) => return fail(e),
    };
    if req["confirm"].as_str().map(Path::new) != Some(target.as_path()) {
        return fail(format!("confirm must repeat the resolved target ({})", target.display()));
    }
    crate::wlog::log("backup", &format!("system_restore {} -> {}", archive.display(), target.display()));
    crate::emit(&json!({"event": "start", "phase": "restore", "archive": archive, "target": target,
                        "command": format!("tar --acls --xattrs --xattrs-include='*' --numeric-owner -xpf {} -C {}", archive.display(), target.display())}));
    let r = match read_archive(&archive, Some(&target), req["low_priority"].as_bool().unwrap_or(false)) { Ok(r) => r, Err(e) => return fail(e) };
    let mut created = Vec::new();
    for (d, mode) in MOUNT_POINTS {
        let p = target.join(d);
        if fs::symlink_metadata(&p).is_err() && fs::create_dir(&p).is_ok() {
            let _ = fs::set_permissions(&p, fs::Permissions::from_mode(*mode));
            created.push(format!("/{d}"));
        }
    }
    unsafe { libc::sync(); }
    json!({"ok": true, "archive": archive, "target": target, "files": r.files, "tar_status": r.tar_status,
           "degraded": r.tar_status != 0, "created_dirs": created,
           "message_count": r.prog.msg_count.load(Ordering::Relaxed), "messages": r.prog.tail(), "elapsed": r.elapsed})
}

// ── LPM configuration backup ────────────────────────────────────────────────

fn xdg(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute()).map(|h| h.join(fallback)))
}
/// Directories under $XDG_CONFIG_HOME that hold LPM data (the Ryzen tab shares
/// its profile folder with the standalone Curve Optimizer applet).
const USER_CONFIG_DIRS: &[&str] = &["legion-power-manager", "ryzen-curve-optimizer"];
fn config_home() -> Option<PathBuf> { xdg("XDG_CONFIG_HOME", ".config") }
fn user_state() -> Option<PathBuf> { xdg("XDG_STATE_HOME", ".local/state").map(|p| p.join("legion-power-manager")) }

#[derive(Default)]
struct Copier { files: u64, bytes: u64, skipped: Vec<String>, links: bool }

impl Copier {
    /// Copies a file or tree. The path named by the caller is followed if it
    /// is a symlink (/usr/src/linux); links inside a tree are copied as links
    /// (`links`) or left out. Unreadable or oversized entries are listed.
    fn copy(&mut self, src: &Path, dst: &Path, depth: u32) {
        if PROFILE_SKIP.iter().any(|p| src == Path::new(p)) { return; }
        let md = match if depth == 0 { fs::metadata(src) } else { fs::symlink_metadata(src) } {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => return self.skip(src, &e.to_string()),
        };
        let ft = md.file_type();
        if ft.is_symlink() {
            if self.links { if let Ok(t) = fs::read_link(src) { let _ = std::os::unix::fs::symlink(t, dst); } }
        } else if ft.is_dir() {
            if depth > 16 { return self.skip(src, "too deep"); }
            let rd = match fs::read_dir(src) { Ok(r) => r, Err(e) => return self.skip(src, &e.to_string()) };
            if fs::create_dir_all(dst).is_err() { return self.skip(src, "cannot stage"); }
            let mut names: Vec<_> = rd.flatten().map(|e| e.file_name()).collect();
            names.sort();
            for n in names { self.copy(&src.join(&n), &dst.join(&n), depth + 1); }
        } else if ft.is_file() {
            if md.len() > MAX_FILE || self.bytes + md.len() > MAX_STAGE { return self.skip(src, "too large"); }
            if let Some(p) = dst.parent() { let _ = fs::create_dir_all(p); }
            match fs::copy(src, dst) {
                Ok(n) => { self.files += 1; self.bytes += n; }
                Err(e) => self.skip(src, &e.to_string()),
            }
        }
    }
    fn skip(&mut self, p: &Path, why: &str) {
        if self.skipped.len() < 200 { self.skipped.push(format!("{} ({why})", p.display())); }
    }
}

fn write_text(p: &Path, body: &str) { if let Some(d) = p.parent() { let _ = fs::create_dir_all(d); } let _ = fs::write(p, body); }

/// Generated part of the system profile: facts no config file holds.
fn profile_facts(dir: &Path) {
    let (host, kernel) = uname();
    write_text(&dir.join("uname.txt"), &format!("host: {host}\nkernel: {kernel}\n"));
    for (name, src) in [("cmdline.txt", "/proc/cmdline"), ("mounts.txt", "/proc/mounts"), ("partitions.txt", "/proc/partitions"),
                        ("swaps.txt", "/proc/swaps")] {
        if let Ok(s) = fs::read_to_string(src) { write_text(&dir.join(name), &s); }
    }
    write_text(&dir.join("cpu.txt"), crate::cpuinfo_head());
    let dmi: String = ["sys_vendor", "product_name", "product_version", "product_family", "board_name", "bios_version",
                       "bios_date", "ec_firmware_release"].iter()
        .filter_map(|f| crate::read_trimmed(&Path::new("/sys/class/dmi/id").join(f)).ok().map(|v| format!("{f}: {v}\n"))).collect();
    write_text(&dir.join("dmi.txt"), &dmi);
    if let Ok(m) = fs::read_to_string("/proc/modules") {
        let mut names: Vec<&str> = m.lines().filter_map(|l| l.split_whitespace().next()).collect();
        names.sort_unstable();
        write_text(&dir.join("modules.txt"), &(names.join("\n") + "\n"));
    }
    let _ = fs::copy("/proc/config.gz", dir.join("running-kernel-config.gz"));
    // Installed packages (Portage VDB): category/name-version.
    let mut pkgs = Vec::new();
    for cat in fs::read_dir("/var/db/pkg").into_iter().flatten().flatten() {
        for p in fs::read_dir(cat.path()).into_iter().flatten().flatten() {
            pkgs.push(format!("{}/{}", cat.file_name().to_string_lossy(), p.file_name().to_string_lossy()));
        }
    }
    if !pkgs.is_empty() { pkgs.sort(); write_text(&dir.join("packages.txt"), &(pkgs.join("\n") + "\n")); }
}

fn safe_tag(v: &Value) -> Option<&str> {
    v.as_str().filter(|t| !t.is_empty() && t.len() <= 24 && t.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'))
}

/// {"op":"config_backup","dest_dir":"/…","include_profile":true,"include_logs":false,"tag":"pre-restore","lpm_version":"2.0.0"}
/// Runs as the user: only readable files are saved; the rest is listed in `skipped`.
pub fn config_backup(req: &Value) -> Value {
    match config_backup_inner(req) { Ok(v) => v, Err(e) => fail(e) }
}

fn config_backup_inner(req: &Value) -> Result<Value, String> {
    if is_root() { return Err("config_backup runs as the user, not through pkexec".into()); }
    let d = req["dest_dir"].as_str().filter(|s| s.starts_with('/') && s.len() < 1024).ok_or("dest_dir: an absolute path is required")?;
    fs::create_dir_all(d).map_err(|e| format!("{d}: {e}"))?;
    let dest = abs_dir(&req["dest_dir"], "dest_dir")?;
    let tar = tool("tar").ok_or("tar is not installed")?;
    let stage = Scratch(mkdtemp(&std::env::temp_dir(), "lpm-backup-")?);
    let s = &stage.0;
    let logs = req["include_logs"].as_bool().unwrap_or(false);

    let mut user = Copier::default();
    if let Some(c) = config_home() {
        for d in USER_CONFIG_DIRS { user.copy(&c.join(d), &s.join("user/config").join(d), 0); }
    }
    let mut state = Copier::default();
    if logs { if let Some(c) = user_state() { state.copy(&c, &s.join("user/state/legion-power-manager"), 0); } }
    let mut sys = Copier::default();
    for p in SYS_LPM.iter().chain(if logs { SYS_LPM_LOGS } else { &[] }) {
        sys.copy(&Path::new("/").join(p), &s.join("system").join(p), 0);
    }
    let mut prof = Copier { links: true, ..Default::default() };
    if req["include_profile"].as_bool().unwrap_or(true) {
        for p in PROFILE { prof.copy(&Path::new("/").join(p), &s.join("system-profile/root").join(p), 0); }
        profile_facts(&s.join("system-profile"));
    }
    if user.files + sys.files + prof.files == 0 { return Err("nothing to back up yet".into()); }

    let st = stamp();
    let (host, kernel) = uname();
    let dmi = |f: &str| crate::read_trimmed(&Path::new("/sys/class/dmi/id").join(f)).unwrap_or_default();
    let skipped: Vec<String> = [&user, &state, &sys, &prof].iter().flat_map(|c| c.skipped.iter().cloned()).collect();
    let manifest = json!({"format": CONFIG_FORMAT, "version": 1, "created": st.iso, "host": host,
        "lpm_version": req["lpm_version"].as_str().filter(|v| v.len() <= 32).unwrap_or(""),
        "machine": {"product": dmi("product_version"), "bios": dmi("bios_version"), "kernel": kernel},
        "contents": {"user_config": user.files, "user_state": state.files, "system": sys.files, "system_profile": prof.files},
        "skipped": skipped});
    fs::write(s.join(MANIFEST), serde_json::to_vec_pretty(&manifest).unwrap_or_default()).map_err(|e| e.to_string())?;

    let tag = safe_tag(&req["tag"]).map(|t| format!("-{t}")).unwrap_or_default();
    let fin = dest.join(format!("lpm-config-{}_{}{tag}.tar.gz", st.date, st.time));
    let partial = dest.join(format!(".{}.partial", fin.file_name().unwrap().to_string_lossy()));
    let mut tc = command(&tar, false);
    tc.arg("-czf").arg(&partial).arg("-C").arg(s).arg(MANIFEST);
    for top in ["user", "system", "system-profile"] { if s.join(top).exists() { tc.arg(top); } }
    let o = tc.stdin(Stdio::null()).output().map_err(|e| format!("tar: {e}"))?;
    if !o.status.success() {
        let _ = fs::remove_file(&partial);
        return Err(format!("tar failed: {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    let _ = fs::set_permissions(&partial, fs::Permissions::from_mode(0o600));
    fs::rename(&partial, &fin).map_err(|e| { let _ = fs::remove_file(&partial); format!("{}: {e}", fin.display()) })?;
    Ok(json!({"ok": true, "archive": fin, "bytes": fs::metadata(&fin).map_or(0, |m| m.len()),
              "contents": manifest["contents"], "skipped": manifest["skipped"]}))
}

fn read_manifest(tar: &Path, archive: &Path) -> Result<Value, String> {
    let o = command(tar, false).arg("-xOf").arg(archive).arg(MANIFEST).stdin(Stdio::null()).output().map_err(|e| format!("tar: {e}"))?;
    let m: Value = serde_json::from_slice(&o.stdout[..o.stdout.len().min(1 << 20)]).map_err(|_| "not a Legion Power Manager configuration backup")?;
    if m["format"] != CONFIG_FORMAT || m["version"] != 1 { return Err("not a Legion Power Manager configuration backup (or a newer format)".into()); }
    Ok(m)
}

/// {"op":"config_inspect","archive":"/…"} → {"ok":true,"manifest":{…}}
pub fn config_inspect(req: &Value) -> Value {
    let r = (|| Ok::<_, String>(read_manifest(&tool("tar").ok_or("tar is not installed")?, &abs_file(&req["archive"])?)?))();
    match r { Ok(m) => json!({"ok": true, "manifest": m}), Err(e) => fail(e) }
}

/// Extracts one top-level directory of a configuration backup into a 0700
/// scratch directory; ownership and modes from the archive are not applied.
fn unpack(archive: &Path, top: &str, base: &Path) -> Result<Option<Scratch>, String> {
    let tar = tool("tar").ok_or("tar is not installed")?;
    let m = read_manifest(&tar, archive)?;
    let key = if top == "user" { "user_config" } else { "system" };
    if m["contents"][key].as_u64().unwrap_or(0) == 0 { return Ok(None); }
    let stage = Scratch(mkdtemp(base, ".lpm-restore-")?);
    let o = command(&tar, false).arg("-xf").arg(archive).arg("-C").arg(&stage.0)
        .args(["--no-same-owner", "--no-same-permissions", top]).stdin(Stdio::null()).output().map_err(|e| format!("tar: {e}"))?;
    if !o.status.success() { return Err(format!("tar failed: {}", String::from_utf8_lossy(&o.stderr).trim())); }
    Ok(Some(stage))
}

/// {"op":"config_restore_user","archive":"/…"} — as the user: the saved
/// ~/.config/{legion-power-manager,ryzen-curve-optimizer} files overwrite the
/// current ones; files that are not in the backup stay.
pub fn config_restore_user(req: &Value) -> Value {
    let r: Result<Value, String> = (|| {
        if is_root() { return Err("config_restore_user runs as the user".to_string()); }
        let archive = abs_file(&req["archive"])?;
        let Some(stage) = unpack(&archive, "user", &std::env::temp_dir())? else { return Ok(json!({"ok": true, "files": 0})) };
        let dst = config_home().ok_or("no home directory")?;
        let mut c = Copier::default();
        for d in USER_CONFIG_DIRS { c.copy(&stage.0.join("user/config").join(d), &dst.join(d), 0); }
        Ok(json!({"ok": true, "files": c.files, "dir": dst, "skipped": c.skipped}))
    })();
    r.unwrap_or_else(fail)
}

fn safe_name(n: &std::ffi::OsStr) -> Option<&str> {
    n.to_str().filter(|s| !s.is_empty() && s.len() <= 128 && !s.starts_with('.')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || " _.()+-".contains(c)))
}

/// Installs the regular files of `src` (no symlinks, safe names) into the
/// root-owned directory `dst`, each root:root 0644 through an atomic write.
fn install_dir(src: &Path, dst: &str, only: Option<&[&str]>, done: &mut Vec<String>) -> Result<(), String> {
    let Ok(rd) = fs::read_dir(src) else { return Ok(()) };
    let mut made = false;
    for e in rd.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_owned) else { continue };
        if safe_name(&e.file_name()).is_none() || only.is_some_and(|o| !o.contains(&name.as_str())) { continue; }
        let Ok(md) = fs::symlink_metadata(e.path()) else { continue };
        if !md.is_file() || md.len() > MAX_ROOT_FILE { continue; }
        if !made { crate::secure_dir(dst)?; made = true; }
        let body = fs::read(e.path()).map_err(|e| e.to_string())?;
        let to = format!("{dst}/{name}");
        crate::write_root_file(&to, &body)?;
        done.push(to);
    }
    Ok(())
}

/// {"op":"config_restore_root","archive":"/…"} — root: presets, boot profiles,
/// network guard rules, NVIDIA curve profiles, calibration data and LPM's
/// NVIDIA module options. Whitelisted paths only.
pub fn config_restore_root(req: &Value) -> Value {
    let r: Result<Value, String> = (|| {
        let archive = abs_file(&req["archive"])?;
        crate::secure_dir("/var/lib/legion-power-manager")?;
        let Some(stage) = unpack(&archive, "system", Path::new("/var/lib/legion-power-manager"))? else {
            return Ok(json!({"ok": true, "files": 0, "restored": []}));
        };
        let s = stage.0.join("system");
        let mut done = Vec::new();
        crate::secure_dir("/etc/legion-power-manager")?;
        install_dir(&s.join("etc/legion-power-manager"), "/etc/legion-power-manager", None, &mut done)?;
        install_dir(&s.join("etc/legion-power-manager/presets"), "/etc/legion-power-manager/presets", None, &mut done)?;
        if s.join("etc/nvcurve").is_dir() {
            crate::secure_dir("/etc/nvcurve")?;
            install_dir(&s.join("etc/nvcurve"), "/etc/nvcurve", Some(&["config.json"]), &mut done)?;
            install_dir(&s.join("etc/nvcurve/profiles"), "/etc/nvcurve/profiles", None, &mut done)?;
        }
        install_dir(&s.join("var/lib/legion-power-manager"), "/var/lib/legion-power-manager", Some(VARLIB_RESTORE), &mut done)?;
        install_dir(&s.join("etc/modprobe.d"), "/etc/modprobe.d", Some(&["zz-legion-power-manager-nvidia.conf"]), &mut done)?;
        crate::wlog::log("backup", &format!("config_restore_root {} ({} files)", archive.display(), done.len()));
        if done.iter().any(|p| p.ends_with("/netguard.json")) {
            if let Some(p) = crate::netguard::daemon_pid() { unsafe { libc::kill(p, libc::SIGHUP); } }
        }
        Ok(json!({"ok": true, "files": done.len(), "restored": done}))
    })();
    r.unwrap_or_else(fail)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_names() {
        assert!(is_image_name("backup-2026-09-26.tar.gz"));
        assert!(is_image_name("backup-2026-09-26_142501.tar.zst"));
        assert!(is_image_name("backup-2026-09-26.tar.xz"));
        assert!(!is_image_name("backup-2026-09-26.tar.gz.partial"));
        assert!(!is_image_name("backup-2026-09-26.tar.gz.info.json"));
        assert!(!is_image_name("backup-notes.tar.gz"));
        assert!(!is_image_name("my-backup-2026-09-26.tar.gz"));
        assert!(!is_image_name("backup-2026-09-26_x.tar.gz"));
    }
    #[test]
    fn exclude_literal() {
        assert_eq!(literal(Path::new("/mnt/usb [1]/a*b")), "/mnt/usb \\[1]/a\\*b");
    }
    #[test]
    fn names() {
        use std::ffi::OsStr;
        assert!(safe_name(OsStr::new("Gaming (max).json")).is_some());
        assert!(safe_name(OsStr::new(".hidden")).is_none());
        assert!(safe_name(OsStr::new("a/b")).is_none());
        assert_eq!(safe_tag(&json!("pre-restore")), Some("pre-restore"));
        assert_eq!(safe_tag(&json!("../x")), None);
    }
}
