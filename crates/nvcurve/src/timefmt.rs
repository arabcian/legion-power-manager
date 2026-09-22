//! Local-time formatting without pulling in a date crate.

fn local_tm() -> (libc::tm, u32) {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&ts.tv_sec, &mut tm) };
    (tm, (ts.tv_nsec / 1000) as u32)
}

/// `%Y%m%d_%H%M%S` — snapshot file stem (lexicographic == chronological).
pub fn stamp() -> String {
    let (t, _) = local_tm();
    format!("{:04}{:02}{:02}_{:02}{:02}{:02}",
        t.tm_year + 1900, t.tm_mon + 1, t.tm_mday, t.tm_hour, t.tm_min, t.tm_sec)
}

/// Python `datetime.now().isoformat()` equivalent.
pub fn iso_now() -> String {
    let (t, us) = local_tm();
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}",
        t.tm_year + 1900, t.tm_mon + 1, t.tm_mday, t.tm_hour, t.tm_min, t.tm_sec, us)
}
