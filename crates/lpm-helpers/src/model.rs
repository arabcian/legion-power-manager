//! Joint response model of one machine: what every calibrated knob does alone
//! (with a smooth dose-response for numeric ladders), next to every other knob,
//! and - once the data can carry it - in threes.
//!
//! * Data: rows = (configuration of the knobs a run changed, measured effect per
//!   objective). lpm-calibrate runs randomised multi-knob designs (sparse at small
//!   budgets, crowded at large ones), so every run informs every knob, pair and
//!   triple it touched.
//! * Model: one additive Gaussian process per objective (Duvenaud, Nickisch &
//!   Rasmussen, "Additive Gaussian Processes", 2011):
//!       f(x) = sum of main effects + sum over pairs + sum over triples
//!   For knob f, m_f / u_f are the dot products of its features in x and x'
//!   (thermometer coding for numeric ladders, so neighbouring doses share strength
//!   and doses in between are interpolated; one feature per value for choices),
//!   zero when either configuration leaves the knob at its reference. With
//!   z_f = h_f * u_f:
//!       k(x,x') = s_main^2 sum m_f
//!               + s_pin^2 sum_{a<b same cluster} z_a z_b + s_px^2 sum_{a<b other} z_a z_b
//!               + s_tri^2 (sum_{in-cluster triples} + (s_px/s_pin)^2 sum_{mixed}) z_a z_b z_c
//!   evaluated through elementary symmetric polynomials of the knobs both runs
//!   changed: EVERY pair and triple of ALL knobs is in the model at a cost linear in
//!   the number of changed knobs. The coupled clusters (memory/reclaim/THP/writeback
//!   vs CPU/scheduler) only set prior scales; h_f is a heredity weight (a knob with a
//!   strong main effect is a priori likelier to interact). Triples switch on once
//!   there are enough rows (max(150, 4 x knobs)).
//! * Nuisance: every session has its own offset, linear trend and an
//!   Ornstein-Uhlenbeck drift in wall-clock time (thermals, background work), so the
//!   interleaved reference runs absorb drift instead of the knobs. Prior scales, noise
//!   and drift come from evidence maximisation; outlier runs are down-weighted
//!   (Student-t style, from leave-one-out residuals).
//! * Everything is a posterior of a linear functional (`Func`): a configuration, the
//!   difference of two, a 2x2 or 2x2x2 interaction contrast. Decisions are risk-aware
//!   and the next experiment goes where it changes a decision - or the verdict on an
//!   interaction - the most. The posterior mean is also kept as explicit sparse
//!   coefficients, so the search evaluates a configuration in O(changes^3) whatever
//!   the number of rows.
//! * Search: coordinate ascent with joint moves of the pairs that interact, restarts,
//!   then pruning of changes whose in-context gain misses the margin.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

pub type Cfg = Vec<(String, String)>;
/// Prior sd of a session's offset and trend (objective units; an objective is the gain summed
/// over its metrics since benchmark set 4, hence twice the earlier 0.03).
const DRIFT_SD: f64 = 0.06;
/// Correlation time of the within-session drift (seconds).
const OU_TAU: f64 = 150.0;
/// Risk aversion of decisions: gain must exceed the margin by this many posterior sds.
pub const RISK_Z: f64 = 0.5;
/// Steps (values) per knob that the coefficient keys can address.
const MAX_STEPS: usize = 60;
/// Rows one fit uses at most (highest weight first): the Cholesky factor is O(n^3).
pub const MAX_FIT_ROWS: usize = 1100;
/// Kernel classes: main, pair in-cluster, pair cross-cluster, triple in-cluster, triple mixed.
const NC: usize = 5;

// ── small helpers ────────────────────────────────────────────────────────────

pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Rng { Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1) }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12; x ^= x << 25; x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: usize) -> usize { if n <= 1 { 0 } else { (self.next() % n as u64) as usize } }
    pub fn unit(&mut self) -> f64 { (self.next() >> 11) as f64 / (1u64 << 53) as f64 }
    pub fn shuffle<T>(&mut self, v: &mut [T]) { for i in (1..v.len()).rev() { let j = self.below(i + 1); v.swap(i, j); } }
    /// Uniform in lo..=hi.
    pub fn range(&mut self, lo: usize, hi: usize) -> usize { if hi <= lo { lo } else { lo + self.below(hi - lo + 1) } }
}

pub fn norm_cdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * (x / std::f64::consts::SQRT_2).abs());
    let y = 1.0 - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t + 0.254_829_592) * t
        * (-(x * x) / 2.0).exp();
    0.5 * (1.0 + if x < 0.0 { -y } else { y })
}

/// Knobs that physically interact: memory/reclaim/THP/writeback, and CPU frequency + scheduler.
pub fn cluster(key: &str) -> u8 {
    if key == "thp" || key.starts_with("vm.") || key.starts_with("mm.") || key.starts_with("thp.") || key.starts_with("zswap.") { 0 } else { 1 }
}

/// Position of a number on the dose axis (log2, symmetric around 0).
fn pos(x: f64) -> f64 { (x.abs() + 1.0).log2() * if x < 0.0 { -1.0 } else { 1.0 } }

/// log2 distance of two values on the dose axis (None for text).
/// 0 against a non-zero value is no distance at all (None): 0 switches the feature off
/// (writeback throttling, boosted reclaim, background compaction, APST, ...), a mode change
/// costed like a choice, not a dose ten doublings away.
pub fn dose_dist(a: &str, b: &str) -> Option<f64> {
    let (x, y) = (a.trim().parse::<f64>().ok()?, b.trim().parse::<f64>().ok()?);
    if (x == 0.0) != (y == 0.0) { return None; }
    Some((pos(x) - pos(y)).abs())
}

/// Utility cost of leaving the reference (same modesty as autotune's default).
pub fn modest_cost(reference: &str, value: &str) -> f64 { -0.1 * (0.2 + 0.1 * dose_dist(reference, value).unwrap_or(1.0)) }

/// Measured values plus geometric midpoints between neighbours (whole numbers only).
pub fn with_mids(reference: &str, values: &[String]) -> Vec<String> {
    let Ok(r) = reference.trim().parse::<i64>() else { return values.to_vec() };
    let mut nums: Vec<i64> = vec![r];
    for v in values { match v.trim().parse::<i64>() { Ok(x) => nums.push(x), Err(_) => return values.to_vec() } }
    nums.sort(); nums.dedup();
    let mut out: Vec<String> = values.to_vec();
    for w in nums.windows(2) {
        let (a, b) = (w[0], w[1]);
        // No dose between "off" (0) and a setting: 0 is a mode, not the end of the ladder.
        if a <= 0 || b - a < 2 { continue; }
        let mid = (((a as f64 + 1.0) * (b as f64 + 1.0)).sqrt() - 1.0).round() as i64;
        let s = mid.to_string();
        if mid > a && mid < b && !out.contains(&s) { out.push(s); }
    }
    out
}

#[derive(Default, Clone, Copy)]
struct Fx(u64);
impl Hasher for Fx {
    fn finish(&self) -> u64 { self.0 }
    fn write(&mut self, b: &[u8]) { for x in b { self.write_u64(*x as u64); } }
    fn write_u64(&mut self, x: u64) { self.0 = (self.0.rotate_left(5) ^ x).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95); }
}
type FxMap<V> = HashMap<u64, V, BuildHasherDefault<Fx>>;

/// Coefficient key of one (knob, step): 13 + 6 bits.
fn k1(f: u16, s: u8) -> u64 { ((f as u64) << 6) | s as u64 }
fn k2(a: u64, b: u64) -> u64 { (a << 19) | b }
fn k3(a: u64, b: u64, c: u64) -> u64 { (a << 38) | (b << 19) | c }
fn unk(k: u64) -> (u16, u8) { ((k >> 6) as u16, (k & 63) as u8) }

// ── the space: factors, features, kernel ─────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct Factor { pub key: String, pub reference: String, pub values: Vec<String> }

/// Ordered values above/below the reference, each ordered away from it.
#[derive(Clone, Debug, PartialEq)]
struct Geo { r: f64, up: Vec<f64>, down: Vec<f64> }

impl Geo {
    fn new(reference: &str, values: &[String]) -> Option<Geo> {
        let r: f64 = reference.trim().parse().ok()?;
        let (mut up, mut down) = (Vec::new(), Vec::new());
        for v in values {
            let x: f64 = v.trim().parse().ok()?;
            if x > r { up.push(x) } else if x < r { down.push(x) }
        }
        up.sort_by(|a, b| a.total_cmp(b)); up.dedup();
        down.sort_by(|a, b| b.total_cmp(a)); down.dedup();
        (up.len() + down.len() <= MAX_STEPS).then_some(Geo { r, up, down })
    }
}

/// A configuration in feature form: changed knobs (ascending index), each with its
/// active steps (ascending) and their weights.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PCfg(Vec<(u16, Vec<(u8, f64)>)>);

impl PCfg {
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
    pub fn knobs(&self) -> usize { self.0.len() }
    fn keep(&self, f: impl Fn(u16) -> bool) -> PCfg { PCfg(self.0.iter().filter(|e| f(e.0)).cloned().collect()) }
}

/// A linear functional of the response surface: sum of coef * f(configuration).
#[derive(Clone, Debug, Default)]
pub struct Func(pub Vec<(PCfg, f64)>);

impl Func {
    pub fn of(p: PCfg) -> Func { Func(vec![(p, 1.0)]) }
}

/// a - b.
pub fn sub(a: &Func, b: &Func) -> Func {
    let mut v = a.0.clone();
    v.extend(b.0.iter().map(|(p, c)| (p.clone(), -c)));
    Func(v)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Space {
    pub factors: Vec<Factor>,
    geo: Vec<Option<Geo>>,
    /// Prior length of every step (main terms): log2 distance to the previous dose.
    len: Vec<Vec<f64>>,
    clu: Vec<u8>,
    /// Interaction prior scale per knob (effect heredity).
    pub heredity: Vec<f64>,
    /// Highest interaction order in the model (1 = mains only, 2 = pairs, 3 = triples).
    pub order: u8,
    by_key: HashMap<String, usize>,
}

impl Space {
    pub fn new(factors: Vec<Factor>, heredity: Option<Vec<f64>>, order: u8) -> Space {
        let geo: Vec<Option<Geo>> = factors.iter().map(|f| Geo::new(&f.reference, &f.values)).collect();
        let len = factors.iter().zip(&geo).map(|(f, g)| match g {
            Some(g) => {
                let mut v = Vec::new();
                for list in [&g.up, &g.down] {
                    let mut prev = g.r;
                    for x in list.iter() { v.push((pos(*x) - pos(prev)).abs().clamp(0.25, 3.0)); prev = *x; }
                }
                v
            }
            None => vec![1.0; f.values.len().min(MAX_STEPS)],
        }).collect();
        let clu = factors.iter().map(|f| cluster(&f.key)).collect();
        let h = heredity.filter(|h| h.len() == factors.len()).unwrap_or_else(|| vec![1.0; factors.len()]);
        let by_key = factors.iter().enumerate().map(|(i, f)| (f.key.clone(), i)).collect();
        Space { factors, geo, len, clu, heredity: h, order: order.clamp(1, 3), by_key }
    }
    pub fn has(&self, key: &str) -> bool { self.by_key.contains_key(key) }
    pub fn fidx(&self, key: &str) -> Option<usize> { self.by_key.get(key).copied() }
    /// Features of one value (thermometer coding; a value between two measured ones
    /// takes a fraction of the next step). None = not representable.
    fn level(&self, f: usize, value: &str) -> Option<Vec<(u8, f64)>> {
        let fac = &self.factors[f];
        if value == fac.reference { return Some(Vec::new()); }
        let Some(g) = &self.geo[f] else {
            let j = fac.values.iter().position(|v| v == value)?;
            return (j < MAX_STEPS).then(|| vec![(j as u8, 1.0)]);
        };
        let x: f64 = value.trim().parse().ok()?;
        if x == g.r { return Some(Vec::new()); }
        let (up, list, base) = if x > g.r { (true, &g.up, 0) } else { (false, &g.down, g.up.len()) };
        let (mut out, mut prev) = (Vec::new(), g.r);
        for (i, v) in list.iter().enumerate() {
            let past = if up { x >= *v - 1e-9 } else { x <= *v + 1e-9 };
            if past { out.push(((base + i) as u8, 1.0)); prev = *v; } else {
                let t = (pos(x) - pos(prev)) / (pos(*v) - pos(prev));
                if t > 1e-9 { out.push(((base + i) as u8, t.min(1.0))); }
                return Some(out);
            }
        }
        ((x - prev).abs() < 1e-9).then_some(out)
    }
    /// Feature form of a configuration. `strict`: every knob must be known here (training
    /// rows); otherwise unknown knobs are skipped (a phase that does not model them).
    pub fn parse(&self, cfg: &[(String, String)], strict: bool) -> Option<PCfg> {
        let mut out: Vec<(u16, Vec<(u8, f64)>)> = Vec::new();
        for (k, v) in cfg {
            match self.fidx(k).map(|f| (f, self.level(f, v))) {
                Some((f, Some(t))) => {
                    if t.is_empty() { continue; }
                    if out.iter().any(|e| e.0 == f as u16) { if strict { return None; } continue; }
                    out.push((f as u16, t));
                }
                _ => if strict { return None; },
            }
        }
        out.sort_by_key(|e| e.0);
        Some(PCfg(out))
    }
    pub fn func(&self, cfg: &[(String, String)], strict: bool) -> Option<Func> { Some(Func::of(self.parse(cfg, strict)?)) }
    /// Value label of one step.
    pub fn step_value(&self, f: usize, s: u8) -> String {
        let fac = &self.factors[f];
        match &self.geo[f] {
            Some(g) => {
                let s = s as usize;
                let x = if s < g.up.len() { g.up.get(s) } else { g.down.get(s - g.up.len()) };
                x.map(|x| format!("{x}")).unwrap_or_default()
            }
            None => fac.values.get(s as usize).cloned().unwrap_or_default(),
        }
    }
    /// Kernel between two configurations, split by class (structural terms only).
    fn kparts(&self, a: &PCfg, b: &PCfg) -> [f64; NC] {
        let mut out = [0.0; NC];
        // Power sums of z per cluster (0, 1) and in total (2).
        let mut p = [[0.0f64; 3]; 3];
        let mut cnt = 0usize;
        let (mut i, mut j) = (0, 0);
        while i < a.0.len() && j < b.0.len() {
            let (fa, fb) = (a.0[i].0, b.0[j].0);
            if fa < fb { i += 1; continue; }
            if fb < fa { j += 1; continue; }
            let f = fa as usize;
            let (sa, sb) = (&a.0[i].1, &b.0[j].1);
            let (mut m, mut u, mut x, mut y) = (0.0, 0.0, 0, 0);
            while x < sa.len() && y < sb.len() {
                if sa[x].0 < sb[y].0 { x += 1 } else if sb[y].0 < sa[x].0 { y += 1 } else {
                    let pr = sa[x].1 * sb[y].1;
                    m += pr * self.len[f][sa[x].0 as usize];
                    u += pr;
                    x += 1; y += 1;
                }
            }
            out[0] += m;
            if u != 0.0 && self.order >= 2 {
                let z = self.heredity[f] * u;
                for idx in [(self.clu[f] as usize).min(1), 2] { p[idx][0] += z; p[idx][1] += z * z; p[idx][2] += z * z * z; }
                cnt += 1;
            }
            i += 1; j += 1;
        }
        if cnt >= 2 {
            let e2 = |q: &[f64; 3]| 0.5 * (q[0] * q[0] - q[1]);
            let pin = e2(&p[0]) + e2(&p[1]);
            out[1] = pin;
            out[2] = e2(&p[2]) - pin;
            if cnt >= 3 && self.order >= 3 {
                let e3 = |q: &[f64; 3]| (q[0] * q[0] * q[0] - 3.0 * q[0] * q[1] + 2.0 * q[2]) / 6.0;
                let tin = e3(&p[0]) + e3(&p[1]);
                out[3] = tin;
                out[4] = e3(&p[2]) - tin;
            }
        }
        out
    }
    fn same_cluster(&self, fs: &[u16]) -> bool { fs.iter().all(|f| self.clu[*f as usize] == self.clu[fs[0] as usize]) }
}

// ── regression ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hyper { pub sigma: f64, pub main: f64, pub pin: f64, pub px: f64, pub tri: f64, pub ou: f64 }

impl Hyper {
    pub const START: Hyper = Hyper { sigma: 0.03, main: 0.03, pin: 0.012, px: 0.004, tri: 0.004, ou: 0.01 };
    fn clamp(mut self) -> Hyper {
        self.sigma = self.sigma.clamp(0.003, 0.6); self.main = self.main.clamp(0.003, 0.5);
        self.pin = self.pin.clamp(0.001, 0.2); self.px = self.px.clamp(0.0005, 0.1);
        self.tri = self.tri.clamp(0.0003, 0.1); self.ou = self.ou.clamp(0.001, 0.2);
        self
    }
    fn scales(&self) -> [f64; NC] {
        let r = (self.px / self.pin).powi(2).min(4.0);
        [self.main * self.main, self.pin * self.pin, self.px * self.px, self.tri * self.tri, self.tri * self.tri * r]
    }
    pub fn to_vec(&self) -> Vec<f64> { vec![self.sigma, self.main, self.pin, self.px, self.tri, self.ou] }
    pub fn from_slice(v: &[f64]) -> Option<Hyper> {
        (v.len() == 6 && v.iter().all(|x| x.is_finite() && *x > 0.0))
            .then(|| Hyper { sigma: v[0], main: v[1], pin: v[2], px: v[3], tri: v[4], ou: v[5] }.clamp())
    }
}

/// One training observation: configuration, session (index), position in the
/// session (0..1), wall-clock time (s), weight and the measured value.
#[derive(Clone, Debug)]
pub struct Obs { pub x: PCfg, pub sess: usize, pub pos: f64, pub t: f64, pub w: f64, pub y: f64 }

#[derive(Clone, Debug)]
pub struct Fit {
    pub space: Arc<Space>,
    pub nobs: usize,
    x: Vec<PCfg>,
    l: Vec<f64>,
    #[allow(dead_code)]
    alpha: Vec<f64>,
    scales: [f64; NC],
    pub hyper: Hyper,
    pub loglik: f64,
    pub loo_rmse: f64,
    pub loo_r2: f64,
    pub cover90: f64,
    /// Posterior-mean coefficients of every term the data touched.
    main: Vec<Vec<f64>>,
    pair: FxMap<f64>,
    tri: FxMap<f64>,
}

fn chol(a: &mut [f64], n: usize) -> bool {
    for i in 0..n {
        for j in 0..=i {
            let (ri, rj) = (i * n, j * n);
            let mut s = a[ri + j];
            for k in 0..j { s -= a[ri + k] * a[rj + k]; }
            if i == j { if s <= 1e-12 { return false; } a[ri + i] = s.sqrt(); } else { a[ri + j] = s / a[rj + j]; }
        }
    }
    true
}
fn fwd(l: &[f64], n: usize, b: &mut [f64]) {
    for i in 0..n { let mut s = b[i]; for k in 0..i { s -= l[i * n + k] * b[k]; } b[i] = s / l[i * n + i]; }
}
fn bwd(l: &[f64], n: usize, b: &mut [f64]) {
    for i in (0..n).rev() { let mut s = b[i]; for k in i + 1..n { s -= l[k * n + i] * b[k]; } b[i] = s / l[i * n + i]; }
}
/// diag(K^-1) from K's Cholesky factor.
fn inv_diag(l: &[f64], n: usize) -> Vec<f64> {
    let mut d = vec![0.0; n];
    let mut u = vec![0.0; n];
    for j in 0..n {
        u[j] = 1.0 / l[j * n + j];
        for i in j + 1..n { let mut s = 0.0; for k in j..i { s -= l[i * n + k] * u[k]; } u[i] = s / l[i * n + i]; }
        d[j] = u[j..].iter().map(|x| x * x).sum();
    }
    d
}

/// Fits one objective. `init`: starting (or, with `passes` = 0, final) hyperparameters.
pub fn fit(space: Arc<Space>, obs: &[Obs], init: Option<Hyper>, passes: usize) -> Option<Fit> {
    let n = obs.len();
    if n < 6 { return None; }
    let order = space.order;
    let active = |c: usize| match c { 0 => true, 1 | 2 => order >= 2, _ => order >= 3 };
    let mut kc: Vec<Option<Vec<f64>>> = (0..NC).map(|c| active(c).then(|| vec![0.0; n * n])).collect();
    let (mut kn, mut ko) = (vec![0.0; n * n], vec![0.0; n * n]);
    for i in 0..n {
        for j in 0..=i {
            let p = space.kparts(&obs[i].x, &obs[j].x);
            for c in 0..NC { if let Some(m) = kc[c].as_mut() { m[i * n + j] = p[c]; m[j * n + i] = p[c]; } }
            if obs[i].sess == obs[j].sess {
                let v = 1.0 + (obs[i].pos - 0.5) * (obs[j].pos - 0.5);
                let o = (-(obs[i].t - obs[j].t).abs() / OU_TAU).exp();
                kn[i * n + j] = v; kn[j * n + i] = v;
                ko[i * n + j] = o; ko[j * n + i] = o;
            }
        }
    }
    let y: Vec<f64> = obs.iter().map(|o| o.y).collect();
    let w0: Vec<f64> = obs.iter().map(|o| o.w.max(1e-3)).collect();
    let eval = |h: &Hyper, w: &[f64]| -> Option<(f64, Vec<f64>, Vec<f64>)> {
        let s = h.scales();
        let d2 = DRIFT_SD * DRIFT_SD;
        let o2 = h.ou * h.ou;
        let mut k: Vec<f64> = kn.iter().zip(&ko).map(|(a, b)| d2 * a + o2 * b).collect();
        for c in 0..NC { if let Some(m) = &kc[c] { if s[c] != 0.0 { for (a, b) in k.iter_mut().zip(m) { *a += s[c] * b; } } } }
        for i in 0..n { k[i * n + i] += h.sigma * h.sigma / w[i]; }
        if !chol(&mut k, n) { return None; }
        let mut a = y.clone();
        fwd(&k, n, &mut a); bwd(&k, n, &mut a);
        let quad: f64 = y.iter().zip(&a).map(|(p, q)| p * q).sum();
        let logdet: f64 = (0..n).map(|i| k[i * n + i].ln()).sum();
        Some((-0.5 * quad - logdet - 0.5 * n as f64 * (2.0 * std::f64::consts::PI).ln(), k, a))
    };
    let mean_y = y.iter().sum::<f64>() / n as f64;
    let sd_y = (y.iter().map(|v| (v - mean_y).powi(2)).sum::<f64>() / n as f64).sqrt();
    let refs: Vec<f64> = obs.iter().filter(|o| o.x.is_empty()).map(|o| o.y).collect();
    let sigma0 = if refs.len() >= 3 {
        let m = refs.iter().sum::<f64>() / refs.len() as f64;
        (refs.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (refs.len() - 1) as f64).sqrt()
    } else { 0.6 * sd_y };
    let mut h = init.unwrap_or(Hyper { sigma: sigma0.max(0.005), ..Hyper::START }).clamp();
    let mut res = eval(&h, &w0);
    if res.is_none() { h = Hyper { sigma: (sigma0.max(0.005)).max(h.sigma * 2.0), ..h }.clamp(); res = eval(&h, &w0); }
    let mut best = res?.0;
    for _ in 0..passes {
        for which in 0..6 {
            match which { 2 | 3 if order < 2 => continue, 4 if order < 3 => continue, _ => {} }
            let base = h;
            for m in [0.35, 0.6, 1.7, 3.0] {
                let mut c = base;
                match which { 0 => c.sigma *= m, 1 => c.main *= m, 2 => c.pin *= m, 3 => c.px *= m, 4 => c.tri *= m, _ => c.ou *= m }
                let c = c.clamp();
                if let Some((ll, ..)) = eval(&c, &w0) { if ll > best { best = ll; h = c; } }
            }
        }
    }
    // Outlier runs count less (Student-t style reweighting from leave-one-out residuals).
    let mut w = w0.clone();
    let mut res = eval(&h, &w)?;
    for _ in 0..2 {
        let d = inv_diag(&res.1, n);
        for i in 0..n { let z = res.2[i] / d[i].sqrt(); let f = (2.5 / z.abs().max(1e-9)).min(1.0); w[i] = w0[i] * f * f; }
        res = eval(&h, &w)?;
    }
    let d = inv_diag(&res.1, n);
    let (mut sse, mut cover) = (0.0, 0.0);
    for i in 0..n {
        let r = res.2[i] / d[i];
        sse += r * r;
        if (res.2[i] / d[i].sqrt()).abs() < 1.645 { cover += 1.0; }
    }
    let sst: f64 = y.iter().map(|v| (v - mean_y).powi(2)).sum::<f64>().max(1e-12);
    let s = h.scales();
    let alpha = &res.2;
    // Explicit posterior-mean coefficients: beta_term = prior_var(term) * sum_i phi_term(x_i) alpha_i.
    let mut main: Vec<Vec<f64>> = space.len.iter().map(|l| vec![0.0; l.len()]).collect();
    let (mut pair, mut tri): (FxMap<f64>, FxMap<f64>) = (FxMap::default(), FxMap::default());
    for (i, o) in obs.iter().enumerate() {
        let a = alpha[i];
        let e = &o.x.0;
        for (f, st) in e { for (st, v) in st { if let Some(m) = main[*f as usize].get_mut(*st as usize) { *m += a * v; } } }
        if order >= 2 {
            for p in 0..e.len() { for q in p + 1..e.len() {
                for (sa, va) in &e[p].1 { for (sb, vb) in &e[q].1 {
                    *pair.entry(k2(k1(e[p].0, *sa), k1(e[q].0, *sb))).or_insert(0.0) += a * va * vb;
                } }
            } }
        }
        if order >= 3 {
            for p in 0..e.len() { for q in p + 1..e.len() { for r in q + 1..e.len() {
                for (sa, va) in &e[p].1 { for (sb, vb) in &e[q].1 { for (sc, vc) in &e[r].1 {
                    *tri.entry(k3(k1(e[p].0, *sa), k1(e[q].0, *sb), k1(e[r].0, *sc))).or_insert(0.0) += a * va * vb * vc;
                } } }
            } } }
        }
    }
    for (f, m) in main.iter_mut().enumerate() { for (st, b) in m.iter_mut().enumerate() { *b *= s[0] * space.len[f][st]; } }
    for (k, b) in pair.iter_mut() {
        let ((fa, _), (fb, _)) = (unk(k >> 19), unk(k & 0x7FFFF));
        *b *= if space.same_cluster(&[fa, fb]) { s[1] } else { s[2] } * space.heredity[fa as usize] * space.heredity[fb as usize];
    }
    for (k, b) in tri.iter_mut() {
        let (fa, fb, fc) = (unk(k >> 38).0, unk((k >> 19) & 0x7FFFF).0, unk(k & 0x7FFFF).0);
        *b *= if space.same_cluster(&[fa, fb, fc]) { s[3] } else { s[4] }
            * space.heredity[fa as usize] * space.heredity[fb as usize] * space.heredity[fc as usize];
    }
    pair.retain(|_, b| *b != 0.0);
    tri.retain(|_, b| *b != 0.0);
    let alpha = res.2.clone();
    Some(Fit { space, nobs: n, x: obs.iter().map(|o| o.x.clone()).collect(), l: res.1, alpha, scales: s, hyper: h, loglik: res.0,
               loo_rmse: (sse / n as f64).sqrt(), loo_r2: 1.0 - sse / sst, cover90: cover / n as f64, main, pair, tri })
}

impl Fit {
    fn k(&self, a: &PCfg, b: &PCfg) -> f64 { self.space.kparts(a, b).iter().zip(&self.scales).map(|(p, s)| p * s).sum() }
    /// Posterior mean of one configuration from the explicit coefficients.
    fn mean_pc(&self, x: &PCfg) -> f64 {
        let e = &x.0;
        let mut s = 0.0;
        for (f, st) in e { let m = &self.main[*f as usize]; for (k, v) in st { s += v * m.get(*k as usize).copied().unwrap_or(0.0); } }
        if !self.pair.is_empty() {
            for p in 0..e.len() { for q in p + 1..e.len() {
                for (sa, va) in &e[p].1 { for (sb, vb) in &e[q].1 {
                    if let Some(b) = self.pair.get(&k2(k1(e[p].0, *sa), k1(e[q].0, *sb))) { s += va * vb * b; }
                } }
            } }
        }
        if !self.tri.is_empty() {
            for p in 0..e.len() { for q in p + 1..e.len() { for r in q + 1..e.len() {
                for (sa, va) in &e[p].1 { for (sb, vb) in &e[q].1 { for (sc, vc) in &e[r].1 {
                    if let Some(b) = self.tri.get(&k3(k1(e[p].0, *sa), k1(e[q].0, *sb), k1(e[r].0, *sc))) { s += va * vb * vc * b; }
                } } }
            } } }
        }
        s
    }
    pub fn mean(&self, f: &Func) -> f64 { f.0.iter().map(|(p, c)| c * self.mean_pc(p)).sum() }
    /// Prior covariance of two functionals.
    pub fn prior(&self, a: &Func, b: &Func) -> f64 {
        let mut s = 0.0;
        for (p, c) in &a.0 { for (q, d) in &b.0 { if *c != 0.0 && *d != 0.0 { s += c * d * self.k(p, q); } } }
        s
    }
    /// L^-1 k(X, f): what the data already explain of this functional.
    pub fn proj(&self, f: &Func) -> Vec<f64> {
        let mut u: Vec<f64> = self.x.iter().map(|x| f.0.iter().map(|(p, c)| if *c == 0.0 { 0.0 } else { c * self.k(x, p) }).sum()).collect();
        fwd(&self.l, self.nobs, &mut u);
        u
    }
    pub fn var(&self, f: &Func) -> f64 {
        let p = self.proj(f);
        (self.prior(f, f) - p.iter().map(|v| v * v).sum::<f64>()).max(0.0)
    }
    pub fn sigma(&self) -> f64 { self.hyper.sigma }
    /// Sum of |coefficients| per knob pair (triples count towards each of their pairs).
    fn pair_strength(&self) -> HashMap<(u16, u16), f64> {
        let mut out: HashMap<(u16, u16), f64> = HashMap::new();
        for (k, b) in &self.pair { let (fa, fb) = (unk(k >> 19).0, unk(k & 0x7FFFF).0); *out.entry((fa, fb)).or_insert(0.0) += b.abs(); }
        for (k, b) in &self.tri {
            let (fa, fb, fc) = (unk(k >> 38).0, unk((k >> 19) & 0x7FFFF).0, unk(k & 0x7FFFF).0);
            for pr in [(fa, fb), (fa, fc), (fb, fc)] { *out.entry(pr).or_insert(0.0) += b.abs() / 2.0; }
        }
        out
    }
    fn triple_strength(&self) -> HashMap<(u16, u16, u16), f64> {
        let mut out: HashMap<(u16, u16, u16), f64> = HashMap::new();
        for (k, b) in &self.tri {
            *out.entry((unk(k >> 38).0, unk((k >> 19) & 0x7FFFF).0, unk(k & 0x7FFFF).0)).or_insert(0.0) += b.abs();
        }
        out
    }
    /// Stand-alone effect size of every knob (sum of squared main coefficients per prior length).
    fn main_strength(&self) -> Vec<f64> {
        self.main.iter().enumerate().map(|(f, m)| m.iter().enumerate().map(|(s, b)| b * b / self.space.len[f][s].max(1e-9)).sum()).collect()
    }
}

// ── phases, per-objective fits, joint prediction ─────────────────────────────

#[derive(Clone, Debug)]
pub struct Row { pub cfg: Cfg, pub y: [f64; 4], pub w: f64, pub sess: u64, pub pos: f64, pub t: f64 }

/// All rows of one phase with their space (heredity and interaction order decided on build).
#[derive(Clone, Debug)]
pub struct PhaseSet { pub space: Arc<Space>, rows: Vec<Row> }

impl PhaseSet {
    pub fn build(factors: Vec<Factor>, rows: Vec<Row>) -> Option<PhaseSet> { PhaseSet::build_with(factors, rows, None) }
    /// `order`: None = decided by the amount of data (pairs always, triples from max(150, 4 x knobs) rows).
    pub fn build_with(factors: Vec<Factor>, mut rows: Vec<Row>, order: Option<u8>) -> Option<PhaseSet> {
        if factors.is_empty() || rows.len() < 6 { return None; }
        if rows.len() > MAX_FIT_ROWS {
            rows.sort_by(|a, b| b.w.total_cmp(&a.w).then(b.t.total_cmp(&a.t)));
            rows.truncate(MAX_FIT_ROWS);
        }
        let nf = factors.len();
        let order = order.unwrap_or(if rows.len() >= 150.max(4 * nf) { 3 } else { 2 });
        // Screening fit (mains only, all objectives summed): knobs that respond carry more interaction prior.
        let screen = Arc::new(Space::new(factors.clone(), None, 1));
        let tmp = PhaseSet { space: screen.clone(), rows };
        let heredity = fit(screen, &tmp.obs(None), None, 1).map(|m| {
            let s = m.main_strength();
            let mean = s.iter().sum::<f64>() / s.len().max(1) as f64;
            s.iter().map(|x| if mean > 1e-12 { (0.2 + 0.8 * x / mean).sqrt().clamp(0.35, 2.5) } else { 1.0 }).collect()
        });
        Some(PhaseSet { space: Arc::new(Space::new(factors, heredity, order)), rows: tmp.rows })
    }
    pub fn nrows(&self) -> usize { self.rows.len() }
    pub fn rows(&self) -> &[Row] { &self.rows }
    /// Observations of one objective (None = the sum of the objectives a row measured).
    fn obs(&self, o: Option<usize>) -> Vec<Obs> {
        let mut sess: Vec<u64> = self.rows.iter().map(|r| r.sess).collect();
        sess.sort(); sess.dedup();
        self.rows.iter().filter_map(|r| {
            let y = match o {
                Some(o) => r.y[o],
                None => { let v: Vec<f64> = r.y.iter().copied().filter(|x| !x.is_nan()).collect(); if v.is_empty() { f64::NAN } else { v.iter().sum() } }
            };
            if y.is_nan() { return None; }
            Some(Obs { x: self.space.parse(&r.cfg, true)?, sess: sess.iter().position(|s| *s == r.sess)?, pos: r.pos, t: r.t, w: r.w, y })
        }).collect()
    }
    /// One objective on its own (rows that did not measure it are left out).
    pub fn fit_obj(&self, o: usize, init: Option<Hyper>, passes: usize) -> Option<Arc<Fit>> {
        fit(self.space.clone(), &self.obs(Some(o)), init, passes).map(Arc::new)
    }
    /// Utility model for objective weights (latency, throughput, power, footprint).
    pub fn fit(&self, wts: [f64; 4]) -> Option<Model> {
        let fits: [Option<Arc<Fit>>; 4] = std::array::from_fn(|o| if wts[o] != 0.0 { self.fit_obj(o, None, 2) } else { None });
        Model::new(self.space.clone(), fits, wts)
    }
}

/// Weighted sum of per-objective fits (independent outputs): mean = sum w_o mu_o, var = sum w_o^2 var_o.
#[derive(Clone, Debug)]
pub struct Model { pub space: Arc<Space>, pub fits: [Option<Arc<Fit>>; 4], pub wts: [f64; 4] }

/// A measured interaction: the knobs (with values), the contrast's posterior (utility units),
/// the stand-alone effects of its knobs and the contrast per objective.
#[derive(Clone, Debug)]
pub struct Interaction { pub knobs: Cfg, pub mean: f64, pub sd: f64, pub alone: Vec<f64>, pub per_obj: [Option<f64>; 4] }

impl Interaction {
    /// Plain-language class of a pair: synergy, overlap (both help, less together) or conflict.
    pub fn kind(&self) -> &'static str {
        let best_alone = self.alone.iter().cloned().fold(f64::MIN, f64::max);
        let together: f64 = self.alone.iter().sum::<f64>() + self.mean;
        // Conflict only when together is credibly worse than the better knob alone.
        if self.mean > 0.0 { "synergy" } else if self.alone.iter().all(|a| *a > 0.0) && together >= best_alone - 2.0 * self.sd { "overlap" } else { "conflict" }
    }
    pub fn label(&self) -> String { self.knobs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" + ") }
}

impl Model {
    pub fn new(space: Arc<Space>, fits: [Option<Arc<Fit>>; 4], wts: [f64; 4]) -> Option<Model> {
        (0..4).any(|o| wts[o] != 0.0 && fits[o].is_some()).then(|| Model { space, fits, wts })
    }
    fn parts(&self) -> impl Iterator<Item = (usize, f64, &Fit)> + '_ {
        (0..4).filter_map(move |o| Some((o, self.wts[o], self.fits[o].as_deref()?))).filter(|(_, w, _)| *w != 0.0)
    }
    pub fn mean(&self, f: &Func) -> f64 { self.parts().map(|(_, w, m)| w * m.mean(f)).sum() }
    pub fn var(&self, f: &Func) -> f64 { self.parts().map(|(_, w, m)| w * w * m.var(f)).sum() }
    /// Mean, variance and the per-objective projections (for acquisition).
    pub fn post(&self, f: &Func) -> (f64, f64, Vec<(usize, Vec<f64>)>) {
        let (mut mu, mut var, mut pr) = (0.0, 0.0, Vec::new());
        for (o, w, m) in self.parts() {
            let p = m.proj(f);
            mu += w * m.mean(f);
            var += w * w * (m.prior(f, f) - p.iter().map(|v| v * v).sum::<f64>()).max(0.0);
            pr.push((o, p));
        }
        (mu, var, pr)
    }
    fn avg(&self, g: impl Fn(&Fit) -> f64) -> f64 {
        let (mut s, mut ws) = (0.0, 0.0);
        for (_, w, m) in self.parts() { s += w.abs() * g(m); ws += w.abs(); }
        if ws > 0.0 { s / ws } else { f64::NAN }
    }
    /// Leave-one-out R² and 90 % interval coverage, averaged over the objectives by weight.
    pub fn r2(&self) -> f64 { self.avg(|m| m.loo_r2) }
    pub fn cover90(&self) -> f64 { self.avg(|m| m.cover90) }
    /// Run-to-run noise of the utility.
    pub fn noise(&self) -> f64 { self.parts().map(|(_, w, m)| (w * m.sigma()).powi(2)).sum::<f64>().sqrt() }
    pub fn nobs(&self) -> usize { self.parts().map(|(_, _, m)| m.nobs).max().unwrap_or(0) }
    /// Interaction strength per knob pair (by key), weighted like the utility.
    pub fn pair_strength(&self) -> HashMap<(String, String), f64> {
        let mut out: HashMap<(String, String), f64> = HashMap::new();
        for (_, w, m) in self.parts() {
            for ((a, b), s) in m.pair_strength() {
                let (ka, kb) = (self.space.factors[a as usize].key.clone(), self.space.factors[b as usize].key.clone());
                *out.entry((ka.clone(), kb.clone())).or_insert(0.0) += w.abs() * s;
                *out.entry((kb, ka)).or_insert(0.0) += w.abs() * s;
            }
        }
        out
    }
    /// 2^k interaction contrast of `knobs` (empty context): sum over subsets T of (-1)^(k-|T|) f(T).
    fn contrast(&self, knobs: &[(String, String)]) -> Option<Func> {
        let k = knobs.len();
        let mut f = Func::default();
        for mask in 0..(1usize << k) {
            let sub: Cfg = (0..k).filter(|i| mask & (1 << i) != 0).map(|i| knobs[i].clone()).collect();
            let sign = if (k - sub.len()) % 2 == 0 { 1.0 } else { -1.0 };
            f.0.push((self.space.parse(&sub, true)?, sign));
        }
        Some(f)
    }
    /// The strongest credible interactions of `order` (2 or 3) knobs: ranked by the lower bound
    /// of their magnitude (|mean| - sd), then by |mean| / sd.
    pub fn interactions(&self, order: usize, k: usize) -> Vec<Interaction> {
        let nf = self.space.factors.len();
        let vals = |f: usize| -> Vec<String> { self.space.factors[f].values.clone() };
        let mut cand: Vec<(f64, Cfg)> = Vec::new();
        let push = |cfg: Cfg, cand: &mut Vec<(f64, Cfg)>| {
            if let Some(f) = self.contrast(&cfg) { let m = self.mean(&f); if m.abs() > 1e-5 { cand.push((m.abs(), cfg)); } }
        };
        if order == 2 {
            let mut strength: HashMap<(u16, u16), f64> = HashMap::new();
            for (_, w, m) in self.parts() { for (p, s) in m.pair_strength() { *strength.entry(p).or_insert(0.0) += w.abs() * s; } }
            let mut pairs: Vec<((u16, u16), f64)> = strength.into_iter().collect();
            pairs.sort_by(|a, b| b.1.total_cmp(&a.1));
            for ((a, b), _) in pairs.into_iter().take(400) {
                let (a, b) = (a as usize, b as usize);
                if a >= nf || b >= nf { continue; }
                for va in vals(a) { for vb in vals(b) {
                    push(vec![(self.space.factors[a].key.clone(), va.clone()), (self.space.factors[b].key.clone(), vb)], &mut cand);
                } }
            }
        } else {
            let mut strength: HashMap<(u16, u16, u16), f64> = HashMap::new();
            for (_, w, m) in self.parts() { for (t, s) in m.triple_strength() { *strength.entry(t).or_insert(0.0) += w.abs() * s; } }
            let mut tris: Vec<((u16, u16, u16), f64)> = strength.into_iter().collect();
            tris.sort_by(|a, b| b.1.total_cmp(&a.1));
            for ((a, b, c), _) in tris.into_iter().take(150) {
                let (a, b, c) = (a as usize, b as usize, c as usize);
                for va in vals(a) { for vb in vals(b) { for vc in vals(c) {
                    push(vec![(self.space.factors[a].key.clone(), va.clone()), (self.space.factors[b].key.clone(), vb.clone()),
                              (self.space.factors[c].key.clone(), vc)], &mut cand);
                } } }
            }
        }
        cand.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut seen: HashSet<Vec<String>> = HashSet::new();
        let mut out: Vec<Interaction> = Vec::new();
        for (_, cfg) in cand.into_iter() {
            if out.len() >= k * 4 { break; }
            // One value combination per knob set: the strongest.
            let ks: Vec<String> = cfg.iter().map(|(k, _)| k.clone()).collect();
            if !seen.insert(ks) { continue; }
            let Some(f) = self.contrast(&cfg) else { continue };
            let (mean, var) = (self.mean(&f), self.var(&f));
            let alone = cfg.iter().map(|kv| self.space.func(std::slice::from_ref(kv), true).map_or(0.0, |g| self.mean(&g))).collect();
            let per_obj = std::array::from_fn(|o| self.fits[o].as_ref().map(|m| m.mean(&f)));
            out.push(Interaction { knobs: cfg, mean, sd: var.sqrt(), alone, per_obj });
        }
        out.sort_by(|a, b| (b.mean.abs() - b.sd).max(0.0).total_cmp(&(a.mean.abs() - a.sd).max(0.0))
            .then((b.mean.abs() / b.sd.max(1e-9)).total_cmp(&(a.mean.abs() / a.sd.max(1e-9)))));
        out.truncate(k);
        out
    }
    /// Pair terms with the largest posterior |mean| / sd: (label, mean, sd).
    pub fn top_pairs(&self, k: usize) -> Vec<(String, f64, f64)> {
        self.interactions(2, k).into_iter().map(|i| (i.label(), i.mean, i.sd)).collect()
    }
}

/// Idle and load models blended by the goal's load share (a knob only one of them models
/// keeps its full weight there), plus the IO model, whose knobs no other phase models: its
/// objective weights already carry the goal's storage weight, so it simply adds.
pub struct Joint { pub idle: Option<Model>, pub load: Option<Model>, pub io: Option<Model>, pub share: f64 }

impl Joint {
    pub fn model(&self, which: usize) -> Option<&Model> { match which { 0 => self.idle.as_ref(), 1 => self.load.as_ref(), _ => self.io.as_ref() } }
    pub fn single_phase(&self) -> Option<usize> {
        let have: Vec<usize> = (0..3).filter(|w| self.model(*w).is_some()).collect();
        (have.len() == 1).then(|| have[0])
    }
    fn models(&self) -> impl Iterator<Item = &Model> + '_ { [&self.idle, &self.load, &self.io].into_iter().filter_map(|m| m.as_ref()) }
    pub fn keys(&self) -> Vec<String> {
        let mut k: Vec<String> = self.models().flat_map(|m| m.space.factors.iter().map(|f| f.key.clone())).collect();
        k.sort(); k.dedup();
        k
    }
    pub fn values(&self, key: &str) -> Vec<String> {
        let mut v: Vec<String> = self.models().filter_map(|m| m.space.fidx(key).map(|i| m.space.factors[i].values.clone())).flatten().collect();
        v.sort(); v.dedup();
        v
    }
    /// The configuration as a functional of one phase's model: terms made only of knobs both
    /// idle and load model count with the phase's blend share, all others in full.
    pub fn feats(&self, cfg: &[(String, String)], which: usize) -> Func {
        let Some(m) = self.model(which) else { return Func::default() };
        let Some(p) = m.space.parse(cfg, false) else { return Func::default() };
        if which == 2 { return Func::of(p); }
        let Some(other) = self.model(1 - which) else { return Func::of(p) };
        let f = if which == 1 { self.share } else { 1.0 - self.share };
        let shared = p.keep(|i| other.space.has(&m.space.factors[i as usize].key));
        if shared.is_empty() || (f - 1.0).abs() < 1e-12 { return Func::of(p); }
        Func(vec![(p, 1.0), (shared, f - 1.0)])
    }
    pub fn mean(&self, cfg: &[(String, String)]) -> f64 {
        (0..3).filter_map(|w| self.model(w).map(|m| m.mean(&self.feats(cfg, w)))).sum()
    }
    pub fn eval(&self, cfg: &[(String, String)]) -> (f64, f64) {
        let (mut mu, mut var) = (0.0, 0.0);
        for w in 0..3 { if let Some(m) = self.model(w) { let f = self.feats(cfg, w); mu += m.mean(&f); var += m.var(&f); } }
        (mu, var)
    }
    /// Effect of `a` minus effect of `b`: (mean, variance) of the difference.
    pub fn eval_diff(&self, a: &[(String, String)], b: &[(String, String)]) -> (f64, f64) {
        let (mut mu, mut var) = (0.0, 0.0);
        for w in 0..3 { if let Some(m) = self.model(w) { let f = sub(&self.feats(a, w), &self.feats(b, w)); mu += m.mean(&f); var += m.var(&f); } }
        (mu, var)
    }
    fn pair_strength(&self) -> HashMap<(String, String), f64> {
        let mut out = HashMap::new();
        for w in 0..3 { if let Some(m) = self.model(w) { for (k, s) in m.pair_strength() { *out.entry(k).or_insert(0.0) += s; } } }
        out
    }
}

// ── decisions ────────────────────────────────────────────────────────────────

/// One decision problem: free knobs with candidate values (index 0 = reference)
/// and their cost, a fixed context, and configurations known to be unsafe.
pub struct Problem<'a> {
    pub joint: &'a Joint,
    pub keys: Vec<String>,
    pub cands: Vec<Vec<(String, f64)>>,
    pub fixed: Cfg,
    pub kappa: f64,
    /// Every change must pay this much on its own: counted as a cost in the search.
    pub margin: f64,
    pub bad: Vec<Cfg>,
    /// Largest number of changes in a random candidate experiment (the design's crowd size).
    pub crowd: usize,
    pairs: Vec<(usize, usize)>,
}

/// A contrast whose sign/size is still in doubt: a decision (knob `i`: level `alt` vs the
/// selection) or, with `pair`, an interaction of knobs i and j at levels (alt, pair.1).
pub struct Target { pub i: usize, pub alt: usize, pub pair: Option<(usize, usize)>, pub mean: f64, pub sd: f64, pub pwrong: f64,
                    func: Func, proj: Vec<(usize, Vec<f64>)> }

impl<'a> Problem<'a> {
    pub fn new(joint: &'a Joint, keys: Vec<String>, cands: Vec<Vec<(String, f64)>>, fixed: Cfg, kappa: f64, margin: f64, bad: Vec<Cfg>) -> Problem<'a> {
        // Joint moves for the pairs the model says interact (and, in small problems, every coupled pair).
        let strength = joint.pair_strength();
        let mut scored: Vec<(f64, usize, usize)> = Vec::new();
        for a in 0..keys.len() { for b in a + 1..keys.len() {
            if cands[a].len() < 2 || cands[b].len() < 2 { continue; }
            let s = strength.get(&(keys[a].clone(), keys[b].clone())).copied().unwrap_or(0.0);
            let small = keys.len() <= 14 && cluster(&keys[a]) == cluster(&keys[b]);
            if s > 1e-5 || small { scored.push((s + if small { 1.0 } else { 0.0 }, a, b)); }
        } }
        scored.sort_by(|x, y| y.0.total_cmp(&x.0));
        scored.truncate(60);
        let pairs = scored.into_iter().map(|(_, a, b)| (a, b)).collect();
        Problem { joint, keys, cands, fixed, kappa, margin, bad, crowd: 6, pairs }
    }
    pub fn cfg(&self, sel: &[usize]) -> Cfg {
        let mut c = self.fixed.clone();
        for (i, &s) in sel.iter().enumerate() { if s > 0 { c.push((self.keys[i].clone(), self.cands[i][s].0.clone())); } }
        c
    }
    fn cost(&self, sel: &[usize]) -> f64 { sel.iter().enumerate().map(|(i, &s)| self.cands[i][s].1 - if s > 0 { self.margin } else { 0.0 }).sum() }
    pub fn violates(&self, cfg: &[(String, String)]) -> bool { self.bad.iter().any(|b| b.iter().all(|kv| cfg.contains(kv))) }
    pub fn score(&self, sel: &[usize], kappa: f64) -> f64 {
        let cfg = self.cfg(sel);
        if self.violates(&cfg) { return f64::NEG_INFINITY; }
        let (m, v) = if kappa == 0.0 { (self.joint.mean(&cfg), 0.0) } else { self.joint.eval(&cfg) };
        m - kappa * v.sqrt() + self.cost(sel)
    }
    /// Best selection: coordinate ascent + joint moves of interacting pairs on the posterior mean (restarts),
    /// then a risk-aware polish (mean - kappa * sd).
    pub fn optimize(&self, start: &[usize], rng: &mut Rng, restarts: usize) -> Vec<usize> {
        let n = self.keys.len();
        let (mut best, mut bs) = (start.to_vec(), self.score(start, 0.0));
        for r in 0..=restarts {
            let mut cur: Vec<usize> = match r {
                0 => start.to_vec(),
                1 => vec![0; n],
                _ => (0..n).map(|i| if rng.unit() < 0.2 && self.cands[i].len() > 1 { 1 + rng.below(self.cands[i].len() - 1) } else { 0 }).collect(),
            };
            for (i, c) in cur.iter_mut().enumerate() { if *c >= self.cands[i].len() { *c = 0; } }
            let mut cs = self.score(&cur, 0.0);
            for _ in 0..8 {
                let mut improved = false;
                for i in 0..n { for c in 0..self.cands[i].len() {
                    if c == cur[i] { continue; }
                    let old = cur[i]; cur[i] = c;
                    let s = self.score(&cur, 0.0);
                    if s > cs + 1e-12 { cs = s; improved = true; } else { cur[i] = old; }
                } }
                for &(a, b) in &self.pairs { for ca in 0..self.cands[a].len() { for cb in 0..self.cands[b].len() {
                    if (ca, cb) == (cur[a], cur[b]) { continue; }
                    let (oa, ob) = (cur[a], cur[b]); cur[a] = ca; cur[b] = cb;
                    let s = self.score(&cur, 0.0);
                    if s > cs + 1e-12 { cs = s; improved = true; } else { cur[a] = oa; cur[b] = ob; }
                } } }
                if !improved { break; }
            }
            if cs > bs { bs = cs; best = cur; }
        }
        if self.kappa > 0.0 {
            let mut cs = self.score(&best, self.kappa);
            for _ in 0..4 {
                let mut improved = false;
                for i in 0..n { for c in 0..self.cands[i].len() {
                    if c == best[i] { continue; }
                    let old = best[i]; best[i] = c;
                    let s = self.score(&best, self.kappa);
                    if s > cs + 1e-12 { cs = s; improved = true; } else { best[i] = old; }
                } }
                if !improved { break; }
            }
        }
        best
    }
    /// In-context gain of every changed knob: (index, mean, sd) of "config" minus "config with the knob back at its reference".
    pub fn marginals(&self, sel: &[usize]) -> Vec<(usize, f64, f64)> {
        let cfg = self.cfg(sel);
        sel.iter().enumerate().filter(|(_, &s)| s > 0).map(|(i, &s)| {
            let mut off = sel.to_vec(); off[i] = 0;
            let (m, v) = self.joint.eval_diff(&cfg, &self.cfg(&off));
            (i, m + self.cands[i][s].1, v.sqrt())
        }).collect()
    }
    /// Backward elimination inside the model. Returns the dropped changes (index, mean, sd).
    pub fn prune(&self, sel: &mut [usize], z: f64, margin: f64) -> Vec<(usize, f64, f64)> {
        let mut dropped = Vec::new();
        loop {
            let worst = self.marginals(sel).into_iter().filter(|(_, m, sd)| m - z * sd < margin)
                .min_by(|a, b| (a.1 - z * a.2).total_cmp(&(b.1 - z * b.2)));
            let Some(w) = worst else { break };
            sel[w.0] = 0;
            dropped.push(w);
        }
        dropped
    }
    // ── which experiment next ──
    /// The decisions still in doubt (single-phase joint only): for every knob, the
    /// contrast "change it / leave it", its posterior and the probability that the decision is wrong.
    pub fn targets(&self, sel: &[usize], margin: f64) -> Vec<Target> {
        let Some(which) = self.joint.single_phase() else { return Vec::new() };
        let Some(model) = self.joint.model(which) else { return Vec::new() };
        let mut out = Vec::new();
        for i in 0..self.keys.len() {
            let levels: Vec<usize> = if sel[i] > 0 { vec![sel[i]] } else { (1..self.cands[i].len()).collect() };
            let mut best: Option<Target> = None;
            for la in levels {
                let (mut a, mut b) = (sel.to_vec(), sel.to_vec());
                a[i] = la; b[i] = 0;
                let ca = self.cfg(&a);
                if self.violates(&ca) { continue; }
                let func = sub(&self.joint.feats(&ca, which), &self.joint.feats(&self.cfg(&b), which));
                let (mu, var, proj) = model.post(&func);
                let mean = mu + self.cands[i][la].1;
                let sd = var.sqrt().max(1e-6);
                if best.as_ref().map_or(true, |t| mean + sd > t.mean + t.sd) {
                    best = Some(Target { i, alt: if sel[i] > 0 { 0 } else { la }, pair: None, mean, sd,
                                         pwrong: norm_cdf(-(mean - margin).abs() / sd), func, proj });
                }
            }
            if let Some(t) = best { out.push(t); }
        }
        out
    }
    /// Interactions still in doubt among the knobs that matter here (changed in `sel`, or the
    /// strongest alternatives): the 2x2 contrast of knobs i and j in the context of `sel`, and
    /// the probability of misjudging whether it exceeds the margin. At most `k`, most doubtful first.
    pub fn pair_targets(&self, sel: &[usize], margin: f64, k: usize) -> Vec<Target> {
        if k == 0 { return Vec::new(); }
        let Some(which) = self.joint.single_phase() else { return Vec::new() };
        let Some(model) = self.joint.model(which) else { return Vec::new() };
        // Level per knob: the selected one, else the best alternative on its own.
        let mut lv: Vec<(f64, usize, usize)> = Vec::new();
        for i in 0..self.keys.len() {
            if self.cands[i].len() < 2 { continue; }
            if sel[i] > 0 { lv.push((f64::MAX, i, sel[i])); continue; }
            let best = (1..self.cands[i].len()).map(|l| {
                let mut a = sel.to_vec(); a[i] = l;
                (self.joint.eval_diff(&self.cfg(&a), &self.cfg(sel)).0 + self.cands[i][l].1, l)
            }).max_by(|a, b| a.0.total_cmp(&b.0));
            if let Some((g, l)) = best { lv.push((g, i, l)); }
        }
        lv.sort_by(|a, b| b.0.total_cmp(&a.0));
        lv.truncate(12);
        let mut out: Vec<Target> = Vec::new();
        for x in 0..lv.len() { for y in x + 1..lv.len() {
            let ((_, i, li), (_, j, lj)) = (lv[x], lv[y]);
            let mut ctx = sel.to_vec(); ctx[i] = 0; ctx[j] = 0;
            let corner = |a: usize, b: usize| { let mut s = ctx.clone(); s[i] = a; s[j] = b; self.cfg(&s) };
            let c11 = corner(li, lj);
            if self.violates(&c11) { continue; }
            let f = |c: &Cfg| self.joint.feats(c, which);
            let func = sub(&sub(&f(&c11), &f(&corner(li, 0))), &sub(&f(&corner(0, lj)), &f(&corner(0, 0))));
            let (mean, var, proj) = model.post(&func);
            let sd = var.sqrt().max(1e-6);
            out.push(Target { i, alt: li, pair: Some((j, lj)), mean, sd, pwrong: norm_cdf(-(mean.abs() - margin).abs() / sd), func, proj });
        } }
        out.sort_by(|a, b| b.pwrong.total_cmp(&a.pwrong));
        out.truncate(k);
        out
    }
    /// Expected reduction of doubt by running each candidate configuration: per objective,
    /// the posterior covariance of the run with every doubtful contrast, squared, over the
    /// run's own predictive variance (independent objectives, all measured by one run).
    pub fn acquisition(&self, targets: &[Target], pool: &[Cfg]) -> Vec<f64> {
        let Some(which) = self.joint.single_phase() else { return vec![0.0; pool.len()] };
        let Some(model) = self.joint.model(which) else { return vec![0.0; pool.len()] };
        pool.iter().map(|c| {
            let func = self.joint.feats(c, which);
            let mut score = 0.0;
            for (o, w, m) in model.parts() {
                let p = m.proj(&func);
                let vc = (m.prior(&func, &func) - p.iter().map(|v| v * v).sum::<f64>()).max(0.0);
                let s2 = m.sigma().powi(2);
                for t in targets {
                    let Some((_, tp)) = t.proj.iter().find(|(to, _)| *to == o) else { continue };
                    let cov = m.prior(&t.func, &func) - tp.iter().zip(&p).map(|(a, b)| a * b).sum::<f64>();
                    score += t.pwrong * w * w * cov * cov / (s2 + vc);
                }
            }
            score
        }).collect()
    }
    /// Candidate experiments: each doubtful decision toggled (alone and with a companion), the
    /// corners of each doubtful interaction, random configurations of 2..=crowd changes, and the
    /// current optimum itself (a confirmation).
    pub fn pool(&self, sel: &[usize], targets: &[Target], n: usize, rng: &mut Rng) -> Vec<Cfg> {
        let k = self.keys.len();
        let mut sels: Vec<Vec<usize>> = vec![sel.to_vec()];
        let mut doubt: Vec<&Target> = targets.iter().filter(|t| t.pwrong > 0.05).collect();
        doubt.sort_by(|a, b| b.pwrong.total_cmp(&a.pwrong));
        for t in doubt.iter().take(14) {
            if let Some((j, lj)) = t.pair {
                let mut ctx = sel.to_vec(); ctx[t.i] = 0; ctx[j] = 0;
                for (a, b) in [(t.alt, lj), (t.alt, 0), (0, lj), (0, 0)] { let mut s = ctx.clone(); s[t.i] = a; s[j] = b; sels.push(s); }
                continue;
            }
            let mut s = sel.to_vec(); s[t.i] = t.alt;
            sels.push(s.clone());
            for _ in 0..2 {
                let mut s2 = s.clone();
                for _ in 0..1 + rng.below(2) { let j = rng.below(k); if self.cands[j].len() > 1 { s2[j] = 1 + rng.below(self.cands[j].len() - 1); } }
                sels.push(s2);
            }
        }
        let hi = self.crowd.max(2).min(k.max(2));
        for _ in 0..n {
            let mut s = vec![0; k];
            for _ in 0..rng.range(2, hi) { let j = rng.below(k); if self.cands[j].len() > 1 { s[j] = 1 + rng.below(self.cands[j].len() - 1); } }
            sels.push(s);
        }
        let mut seen: HashSet<Vec<usize>> = HashSet::new();
        sels.into_iter().filter(|s| seen.insert(s.clone())).map(|s| self.cfg(&s)).filter(|c| !self.violates(c)).collect()
    }
}

// ── space-filling designs ────────────────────────────────────────────────────

/// Shape of a space-filling design: the share of runs that change several knobs of one
/// coupled cluster, the share of crowded runs (many knobs at once: saturation and higher-order
/// effects, the region where an optimum usually lives), and the size ranges.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mix { pub cluster: f64, pub cluster_k: (usize, usize), pub spread_k: (usize, usize), pub crowd: f64, pub crowd_k: (usize, usize) }

impl Mix {
    pub fn lean() -> Mix { Mix { cluster: 0.45, cluster_k: (2, 4), spread_k: (3, 7), crowd: 0.0, crowd_k: (8, 12) } }
}

/// Balanced random design: every knob, every value and - greedily - every PAIR of knobs is
/// drawn about equally often, counting what `prior` (earlier runs) already covered, so a new
/// session fills the gaps of the old ones.
pub fn design(factors: &[Factor], n: usize, rng: &mut Rng, mix: &Mix, prior: &[Cfg]) -> Vec<Cfg> {
    let f = factors.len();
    if f == 0 { return Vec::new(); }
    let idx: HashMap<&str, usize> = factors.iter().enumerate().map(|(i, x)| (x.key.as_str(), i)).collect();
    let (mut used, mut co) = (vec![0.0f64; f], vec![0.0f64; f * f]);
    for c in prior {
        let ids: Vec<usize> = c.iter().filter_map(|(k, _)| idx.get(k.as_str()).copied()).collect();
        for &a in &ids { used[a] += 0.5; for &b in &ids { if a != b { co[a * f + b] += 0.5; } } }
    }
    let mut vdecks: Vec<Vec<usize>> = vec![Vec::new(); f];
    let mut out: Vec<Cfg> = Vec::new();
    let mut seen: HashSet<Cfg> = HashSet::new();
    let mut tries = 0;
    while out.len() < n && tries < n * 20 {
        tries += 1;
        let r = rng.unit();
        let (pool, k): (Vec<usize>, usize) = if r < mix.cluster {
            let c = rng.below(2) as u8;
            let p: Vec<usize> = (0..f).filter(|&i| cluster(&factors[i].key) == c).collect();
            if p.len() >= 2 { let k = rng.range(mix.cluster_k.0, mix.cluster_k.1); (p, k) } else { ((0..f).collect(), rng.range(1, 3.min(f))) }
        } else if r < mix.cluster + mix.crowd {
            ((0..f).collect(), rng.range(mix.crowd_k.0, mix.crowd_k.1))
        } else {
            let lo = if f < 6 { 1 } else { mix.spread_k.0 };
            ((0..f).collect(), rng.range(lo, mix.spread_k.1))
        };
        let k = k.clamp(1, pool.len());
        let mut set: Vec<usize> = Vec::with_capacity(k);
        while set.len() < k {
            let mut best: Option<(f64, usize)> = None;
            for &x in &pool {
                if set.contains(&x) { continue; }
                let s = used[x] + 0.5 * set.iter().map(|&y| co[x * f + y]).sum::<f64>() + 1.5 * rng.unit();
                if best.map_or(true, |b| s < b.0) { best = Some((s, x)); }
            }
            let Some((_, x)) = best else { break };
            set.push(x);
        }
        let mut cfg: Cfg = Vec::new();
        for &i in &set {
            if factors[i].values.is_empty() { continue; }
            if vdecks[i].is_empty() { vdecks[i] = (0..factors[i].values.len()).collect(); rng.shuffle(&mut vdecks[i]); }
            let v = vdecks[i].pop().unwrap();
            cfg.push((factors[i].key.clone(), factors[i].values[v].clone()));
        }
        cfg.sort();
        if cfg.is_empty() || !seen.insert(cfg.clone()) { continue; }
        for &a in &set { used[a] += 1.0; for &b in &set { if a != b { co[a * f + b] += 1.0; } } }
        out.push(cfg);
    }
    // Few knobs: fewer distinct combinations than runs, so the rest are replicates.
    while out.len() < n && !out.is_empty() { let i = rng.below(out.len()); out.push(out[i].clone()); }
    out
}

/// The lean design (sparse runs, 45 % of them inside one cluster), nothing measured before.
pub fn initial_design(factors: &[Factor], n: usize, rng: &mut Rng) -> Vec<Cfg> { design(factors, n, rng, &Mix::lean(), &[]) }

#[cfg(test)]
mod tests {
    use super::*;

    fn fac(key: &str, r: &str, v: &[&str]) -> Factor { Factor { key: key.into(), reference: r.into(), values: v.iter().map(|s| s.to_string()).collect() } }
    fn c(k: &str, v: &str) -> (String, String) { (k.into(), v.into()) }
    fn rows_of(design: &[Cfg], truth: impl Fn(&Cfg) -> f64, rng: &mut Rng, sd: f64) -> Vec<Row> {
        design.iter().enumerate().map(|(i, cfg)| Row {
            cfg: cfg.clone(), y: [truth(cfg) + ((rng.unit() + rng.unit() + rng.unit()) - 1.5) * sd, f64::NAN, f64::NAN, f64::NAN],
            w: 1.0, sess: 1, pos: i as f64 / design.len() as f64, t: i as f64 * 5.0 }).collect()
    }
    fn joint_of(m: Model) -> Joint { Joint { idle: Some(m), load: None, io: None, share: 0.5 } }

    /// Synthetic machine: a synergy (a+b), a redundancy (c or d), a dose-response (e), a harmful knob (f), noise.
    fn truth(cfg: &Cfg) -> f64 {
        let has = |k: &str, v: &str| cfg.iter().any(|(a, b)| a == k && b == v);
        let val = |k: &str| cfg.iter().find(|(a, _)| a == k).map(|(_, b)| b.parse::<f64>().unwrap_or(0.0));
        let mut y = 0.0;
        if has("vm.a", "1") { y += 0.02; }
        if has("vm.b", "1") { y += 0.02; }
        // Sizes clear the decision cost (margin 0.03 + modesty ~0.03 per change) by a real amount.
        if has("vm.a", "1") && has("vm.b", "1") { y += 0.14; }
        if has("vm.c", "1") { y += 0.09; }
        if has("vm.d", "1") { y += 0.09; }
        if has("vm.c", "1") && has("vm.d", "1") { y -= 0.09; }
        if let Some(e) = val("cpu.e") {
            let pts = [(8.0f64, 0.0f64), (16.0, 0.06), (32.0, 0.11), (64.0, 0.06)];
            for w in pts.windows(2) {
                if e >= w[0].0 && e <= w[1].0 { let t = (e.ln() - w[0].0.ln()) / (w[1].0.ln() - w[0].0.ln()); y += w[0].1 + t * (w[1].1 - w[0].1); }
            }
        }
        if has("cpu.f", "1") { y -= 0.06; }
        y
    }

    #[test]
    fn recovers_synergy_redundancy_dose_and_harm() {
        let factors = vec![fac("vm.a", "0", &["1"]), fac("vm.b", "0", &["1"]), fac("vm.c", "0", &["1"]), fac("vm.d", "0", &["1"]),
                           fac("cpu.e", "8", &["16", "32", "64"]), fac("cpu.f", "0", &["1"]), fac("cpu.g", "0", &["1"]), fac("vm.h", "0", &["1"]),
                           fac("cpu.i", "0", &["1", "2"]), fac("vm.j", "0", &["1"])];
        let mut rng = Rng::new(7);
        let mut design = initial_design(&factors, 90, &mut rng);
        for _ in 0..12 { design.push(Vec::new()); }
        let rows = rows_of(&design, truth, &mut rng, 0.04);
        let ps = PhaseSet::build(factors.clone(), rows).unwrap();
        let m = ps.fit([1.0, 0.0, 0.0, 0.0]).unwrap();
        assert!(m.r2() > 0.3, "model explains the data: {}", m.r2());
        assert!(m.cover90() > 0.75, "calibrated intervals: {}", m.cover90());
        let joint = joint_of(m);
        let keys: Vec<String> = factors.iter().map(|f| f.key.clone()).collect();
        let cands: Vec<Vec<(String, f64)>> = factors.iter().map(|f| {
            let mut v = vec![(f.reference.clone(), 0.0)];
            for x in with_mids(&f.reference, &f.values) { let cst = modest_cost(&f.reference, &x); v.push((x, cst)); }
            v
        }).collect();
        let prob = Problem::new(&joint, keys, cands, Vec::new(), RISK_Z, 0.03, Vec::new());
        let mut sel = prob.optimize(&vec![0; 10], &mut rng, 6);
        prob.prune(&mut sel, RISK_Z, 0.03);
        let cfg = prob.cfg(&sel);
        let on = |k: &str| cfg.iter().any(|(a, _)| a == k);
        assert!(on("vm.a") && on("vm.b"), "synergy found: {cfg:?}");
        assert!(on("vm.c") != on("vm.d"), "redundant pair keeps exactly one: {cfg:?}");
        assert!(!on("cpu.f"), "harmful knob stays off: {cfg:?}");
        assert!(!on("cpu.g") && !on("vm.h") && !on("vm.j"), "no-effect knobs stay off: {cfg:?}");
        let e: f64 = cfg.iter().find(|(k, _)| k == "cpu.e").map_or(0.0, |(_, v)| v.parse().unwrap());
        assert!((16.0..=45.0).contains(&e), "dose-response knob lands near its optimum: {cfg:?}");
        let (mu, var) = joint.eval(&cfg);
        assert!((mu - truth(&cfg)).abs() < 0.05 && var.sqrt() > 0.0 && var.sqrt() < 0.05, "mu {mu} truth {} sd {}", truth(&cfg), var.sqrt());
        // Doubt targets (decisions and interactions) and acquisition stay finite.
        let mut t = prob.targets(&sel, 0.03);
        t.extend(prob.pair_targets(&sel, 0.03, 6));
        assert!(t.iter().any(|x| x.pair.is_some()));
        let pool = prob.pool(&sel, &t, 40, &mut rng);
        let a = prob.acquisition(&t, &pool);
        assert!(!pool.is_empty() && a.iter().all(|x| x.is_finite() && *x >= 0.0));
        // The report names the synergy and the overlap.
        let top = joint.idle.as_ref().unwrap().interactions(2, 6);
        let ab = top.iter().find(|i| i.label() == "vm.a=1 + vm.b=1").expect("a+b reported");
        assert!(ab.mean > 0.04 && ab.kind() == "synergy", "{ab:?}");
        let cd = top.iter().find(|i| i.label() == "vm.c=1 + vm.d=1").expect("c+d reported");
        assert!(cd.mean < -0.03 && cd.kind() == "overlap", "{cd:?}");
    }

    #[test]
    fn features_interpolate_and_functionals_subtract() {
        let s = Space::new(vec![fac("cpu.x", "10", &["5", "40", "160"]), fac("cpu.y", "a", &["b", "c"])], None, 2);
        let p = |cfg: Cfg| s.parse(&cfg, true).unwrap();
        assert!(p(vec![c("cpu.x", "10")]).is_empty());
        let (p40, p160) = (p(vec![c("cpu.x", "40")]), p(vec![c("cpu.x", "160")]));
        assert_eq!((p40.0[0].1.len(), p160.0[0].1.len()), (1, 2), "thermometer coding: 160 includes the step to 40");
        let mid = p(vec![c("cpu.x", "80")]);
        assert!(mid.0[0].1[1].1 > 0.0 && mid.0[0].1[1].1 < 1.0, "a dose between measured values takes a fraction of the next step");
        assert!(s.parse(&[c("cpu.x", "500")], true).is_none() && s.parse(&[c("nope", "1")], true).is_none() && s.parse(&[c("nope", "1")], false).is_some());
        let d = sub(&Func::of(p160.clone()), &Func::of(p40));
        assert_eq!((d.0.len(), d.0[1].1), (2, -1.0));
        assert_eq!(with_mids("128", &["256".into(), "1024".into()]).len(), 4);
        assert_eq!(with_mids("2000", &["0".into(), "1000".into()]), vec!["0", "1000", "1414"], "nothing between off and a dose");
        assert_eq!(dose_dist("0", "15000"), None);
        assert!((dose_dist("64", "128").unwrap() - (129f64 / 65.0).log2()).abs() < 1e-9);
        assert!(norm_cdf(0.0) - 0.5 < 1e-6 && norm_cdf(3.0) > 0.99);
        assert!(modest_cost("1", "8") < modest_cost("1", "2"));
        assert_eq!(s.step_value(0, 1), "160");
        assert_eq!(s.step_value(0, 2), "5");
    }

    /// The symmetric-polynomial kernel equals the sum over explicit pairs and triples.
    #[test]
    fn kernel_matches_explicit_pairs_and_triples() {
        let fs: Vec<Factor> = (0..7).map(|i| fac(&format!("{}.k{i}", if i % 2 == 0 { "vm" } else { "cpu" }), "1", &["2", "4"])).collect();
        let h: Vec<f64> = (0..7).map(|i| 0.5 + 0.2 * i as f64).collect();
        let s = Space::new(fs.clone(), Some(h.clone()), 3);
        let a = s.parse(&(0..6).map(|i| (fs[i].key.clone(), if i % 3 == 0 { "4".into() } else { "2".into() })).collect::<Cfg>(), true).unwrap();
        let b = s.parse(&(1..7).map(|i| (fs[i].key.clone(), if i % 2 == 0 { "4".into() } else { "3".into() })).collect::<Cfg>(), true).unwrap();
        let k = s.kparts(&a, &b);
        // Brute force: z per shared knob.
        let mut z: Vec<(f64, u8)> = Vec::new();
        for (fa, sa) in &a.0 { if let Some((_, sb)) = b.0.iter().find(|e| e.0 == *fa) {
            let u: f64 = sa.iter().filter_map(|(st, va)| sb.iter().find(|x| x.0 == *st).map(|x| va * x.1)).sum();
            if u != 0.0 { z.push((h[*fa as usize] * u, cluster(&fs[*fa as usize].key))); }
        } }
        let (mut pin, mut px, mut tin, mut tx) = (0.0, 0.0, 0.0, 0.0);
        for i in 0..z.len() { for j in i + 1..z.len() {
            if z[i].1 == z[j].1 { pin += z[i].0 * z[j].0 } else { px += z[i].0 * z[j].0 }
            for l in j + 1..z.len() { let v = z[i].0 * z[j].0 * z[l].0; if z[i].1 == z[j].1 && z[j].1 == z[l].1 { tin += v } else { tx += v } }
        } }
        for (got, want) in [(k[1], pin), (k[2], px), (k[3], tin), (k[4], tx)] { assert!((got - want).abs() < 1e-9, "{k:?} vs {pin} {px} {tin} {tx}"); }
    }

    /// The explicit coefficients reproduce the kernel posterior mean exactly.
    #[test]
    fn coefficients_equal_kernel_mean() {
        let fs: Vec<Factor> = (0..8).map(|i| fac(&format!("{}.k{i}", if i < 4 { "vm" } else { "cpu" }), "10", &["5", "20", "40"])).collect();
        let mut rng = Rng::new(4);
        let mix = Mix { crowd: 0.4, crowd_k: (4, 7), ..Mix::lean() };
        let mut d = design(&fs, 150, &mut rng, &mix, &[]);
        for _ in 0..10 { d.push(Vec::new()); }
        let rows = rows_of(&d, |c| c.len() as f64 * 0.01, &mut rng, 0.02);
        let ps = PhaseSet::build_with(fs.clone(), rows, Some(3)).unwrap();
        let f = ps.fit_obj(0, None, 1).unwrap();
        assert!(!f.pair.is_empty() && !f.tri.is_empty());
        for cfg in [vec![c("vm.k0", "20"), c("vm.k1", "40"), c("cpu.k5", "5"), c("cpu.k6", "30")], vec![c("vm.k2", "7")]] {
            let func = ps.space.func(&cfg, true).unwrap();
            let kern: f64 = f.x.iter().zip(&f.alpha).map(|(x, a)| a * f.prior(&Func::of(x.clone()), &func)).sum();
            assert!((f.mean(&func) - kern).abs() < 1e-9, "{} vs {kern}", f.mean(&func));
        }
    }

    /// A pure three-way effect (only a+b+c together) is found once triples are in the model.
    #[test]
    fn finds_a_three_way_interaction() {
        let fs: Vec<Factor> = ["vm.a", "vm.b", "vm.c", "vm.d", "cpu.e", "cpu.f", "cpu.g", "vm.h"].iter().map(|k| fac(k, "0", &["1"])).collect();
        let tru = |c: &Cfg| { let on = |k: &str| c.iter().any(|(a, _)| a == k); (if on("vm.a") && on("vm.b") && on("vm.c") { 0.12 } else { 0.0 }) + (if on("cpu.e") { 0.03 } else { 0.0 }) };
        let mut rng = Rng::new(21);
        let mix = Mix { cluster: 0.3, cluster_k: (2, 4), spread_k: (2, 5), crowd: 0.3, crowd_k: (4, 7) };
        let mut d = design(&fs, 200, &mut rng, &mix, &[]);
        for _ in 0..16 { d.push(Vec::new()); }
        let rows = rows_of(&d, tru, &mut rng, 0.03);
        let ps = PhaseSet::build(fs.clone(), rows).unwrap();
        assert_eq!(ps.space.order, 3, "enough rows for triples");
        let m = ps.fit([1.0, 0.0, 0.0, 0.0]).unwrap();
        let tri = m.interactions(3, 3);
        assert_eq!(tri.first().map(|i| i.label()), Some("vm.a=1 + vm.b=1 + vm.c=1".to_string()), "{tri:?}");
        let j = joint_of(m);
        let all3 = vec![c("vm.a", "1"), c("vm.b", "1"), c("vm.c", "1")];
        assert!((j.mean(&all3) - 0.12).abs() < 0.04, "prediction of the triple {}", j.mean(&all3));
        assert!(j.mean(&all3[..2]).abs() < 0.05, "a pair alone is not the effect: {}", j.mean(&all3[..2]));
    }

    #[test]
    fn designs_balance_knobs_pairs_and_grow_with_the_mix() {
        let factors: Vec<Factor> = (0..12).map(|i| fac(&format!("{}.k{i}", if i % 2 == 0 { "vm" } else { "cpu" }), "0", &["1", "2"])).collect();
        let d = initial_design(&factors, 60, &mut Rng::new(3));
        assert_eq!(d.len(), 60);
        let count = |d: &[Cfg], k: &str| d.iter().filter(|c| c.iter().any(|(a, _)| a == k)).count();
        assert!(factors.iter().all(|f| count(&d, &f.key) >= 8), "every knob gets its share");
        assert!(d.iter().any(|c| c.len() >= 2 && c.iter().all(|(k, _)| cluster(k) == 0)), "in-cluster combinations exist");
        assert!(d.iter().all(|c| c.len() <= 7));
        // Crowded mix: larger runs, and every pair of knobs meets.
        let mix = Mix { cluster: 0.25, cluster_k: (2, 6), spread_k: (4, 8), crowd: 0.35, crowd_k: (8, 10) };
        let d2 = design(&factors, 80, &mut Rng::new(5), &mix, &[]);
        assert!(d2.iter().filter(|c| c.len() >= 8).count() >= 15, "crowded runs present");
        let mut co = vec![0usize; 144];
        for c in &d2 { let ids: Vec<usize> = c.iter().map(|(k, _)| k[k.find('k').unwrap() + 1..].parse().unwrap()).collect();
                       for &a in &ids { for &b in &ids { co[a * 12 + b] += 1; } } }
        let pairs: Vec<usize> = (0..12).flat_map(|a| (a + 1..12).map(move |b| (a, b))).map(|(a, b)| co[a * 12 + b]).collect();
        assert!(*pairs.iter().min().unwrap() >= 5, "every pair co-occurs: {pairs:?}");
        // A prior that covered knob 0 heavily shifts the new design to the others.
        let prior: Vec<Cfg> = (0..40).map(|_| vec![(factors[0].key.clone(), "1".into())]).collect();
        let d3 = design(&factors, 40, &mut Rng::new(9), &Mix::lean(), &prior);
        assert!(count(&d3, &factors[0].key) < count(&d3, &factors[1].key), "gaps of earlier runs are filled first");
    }

    /// Fit time at the size of a large calibration (`cargo test --release -- --ignored --nocapture perf`).
    #[test]
    #[ignore]
    fn perf_realistic_size() {
        let factors: Vec<Factor> = (0..46).map(|i| {
            let key = format!("{}.k{i}", if i % 2 == 0 { "vm" } else { "cpu" });
            if i % 3 == 0 { fac(&key, "10", &["5", "20", "40", "80"]) } else { fac(&key, "a", &["b", "c"]) }
        }).collect();
        for (n, mix) in [(240, Mix::lean()), (600, Mix { cluster: 0.3, cluster_k: (2, 5), spread_k: (4, 9), crowd: 0.25, crowd_k: (8, 16) }),
                         (1000, Mix { cluster: 0.25, cluster_k: (2, 6), spread_k: (4, 10), crowd: 0.35, crowd_k: (10, 23) })] {
            let mut rng = Rng::new(9);
            let d = design(&factors, n, &mut rng, &mix, &[]);
            let rows: Vec<Row> = d.iter().enumerate().map(|(i, c)| Row { cfg: c.clone(), y: [rng.unit() * 0.1, rng.unit() * 0.1, f64::NAN, 0.0], w: 1.0,
                sess: (i / 200) as u64, pos: (i % 200) as f64 / 200.0, t: i as f64 * 6.0 }).collect();
            let t = std::time::Instant::now();
            let ps = PhaseSet::build(factors.clone(), rows).unwrap();
            let t1 = t.elapsed();
            let f0 = ps.fit_obj(0, None, 1).unwrap();
            let t2 = t.elapsed();
            let f1 = ps.fit_obj(1, Some(f0.hyper), 0).unwrap();
            let t3 = t.elapsed();
            let m = Model::new(ps.space.clone(), [Some(f0), Some(f1), None, None], [1.0, 0.6, 0.0, 0.0]).unwrap();
            let j = Joint { idle: None, load: Some(m), io: None, share: 0.5 };
            let cfg: Cfg = d[3].clone();
            let t4 = std::time::Instant::now();
            let mut acc = 0.0;
            for _ in 0..2000 { acc += j.mean(&cfg); }
            let t5 = t4.elapsed();
            for _ in 0..50 { acc += j.eval(&cfg).1; }
            eprintln!("rows {n} order {} | build {t1:?} | fit+search {:?} | refit fixed {:?} | 2000 means {t5:?} | 50 vars {:?} ({acc:.3})",
                      ps.space.order, t2 - t1, t3 - t2, t4.elapsed() - t5);
        }
    }
}
