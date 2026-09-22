//! nvcurve core — Rust port of the Python `nvcurve` package (nvapi/, hal/,
//! safety, atomicio, config, profiles/).
//!
//! NvAPI (`libnvidia-api.so`) and NVML (`libnvidia-ml.so.1`) are loaded with
//! dlopen at first use, so linking this crate never requires the NVIDIA
//! driver, and a missing driver is an ordinary error, not a crash.

pub mod atomicio;
pub mod config;
pub mod hal;
pub mod logging;
pub mod ops;
pub mod nvapi;
pub mod nvml;
pub mod proc;
pub mod profiles;
pub mod safety;
pub mod timefmt;
pub mod types;

pub use nvapi::{Gpu, NvError};
pub use types::*;
