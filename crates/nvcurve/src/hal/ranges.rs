//! Clock boost range queries (port of hal/ranges.py).

use crate::nvapi::{fid, nvcall, Gpu, RANGES_SIZE};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ClockRanges {
    pub num_domains: u32,
    /// Raw 18-word records per domain, as the Python version exposed them.
    pub domains: Vec<Vec<i32>>,
}

pub fn get_clock_ranges(gpu: Gpu) -> Result<ClockRanges, String> {
    let d = nvcall(fid::GET_CLOCK_BOOST_RANGES, gpu, RANGES_SIZE, 1, |_| {})?;
    let num = d.u32_at(4);
    let mut domains = Vec::new();
    for i in 0..num.min(32) as usize {
        let base = 0x08 + i * 0x48;
        if base + 0x48 > d.len() { break; }
        domains.push((0..0x48).step_by(4).map(|j| d.i32_at(base + j)).collect());
    }
    Ok(ClockRanges { num_domains: num, domains })
}
