//! Supported-machine gate: Legion Power Manager only runs on Lenovo Legion,
//! LOQ and IdeaPad Gaming laptops. Every helper refuses to touch hardware
//! elsewhere (the GUI has the same check, gui/src/sysinfo.cpp).
//!
//! DMI on these models: sys_vendor "LENOVO", product_name = machine type
//! ("83RU"), product_version / product_family = the marketing name
//! ("Legion Pro 7 16AFR10H", "LOQ 15IRH8", "IdeaPad Gaming 3 15ACH6").

use std::path::Path;

/// Marketing-name prefixes accepted (case-insensitive, word-anchored).
pub const FAMILIES: &[&str] = &["Legion", "LOQ", "IdeaPad Gaming"];

/// China-market Legion / GeekPro names that may not carry "Legion" in DMI
/// (from LenovoLegionToolkit's allowed-model list): "Y9000P IAX10", "R9000K"…
pub const CN_MODELS: &[&str] = &["Y9000", "R9000", "Y7000", "R7000", "G5000"];

/// A word of the name starts with one of CN_MODELS ("Y9000P" yes, "XY9000" no).
fn names_cn_model(name: &str) -> bool {
    name.to_ascii_uppercase().split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| CN_MODELS.iter().any(|m| w.starts_with(m)))
}

fn read(base: &Path, f: &str) -> String {
    std::fs::read_to_string(base.join(f)).map(|s| s.trim().to_owned()).unwrap_or_default()
}

/// "Legion Pro 7" matches "Legion", "Lenovo LOQ 15IAX9" matches "LOQ",
/// but "Legionnaire" / "BLOQ" do not.
fn names_family(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    FAMILIES.iter().any(|fam| {
        let f = fam.to_ascii_lowercase();
        n.match_indices(&f).any(|(i, _)| {
            let before = n[..i].chars().next_back().map_or(true, |c| !c.is_ascii_alphanumeric());
            let after = n[i + f.len()..].chars().next().map_or(true, |c| !c.is_ascii_alphanumeric());
            before && after
        })
    })
}

/// Ok(model name) on a supported machine, Err(reason) otherwise.
pub fn check_at(dmi: &Path) -> Result<String, String> {
    let vendor = read(dmi, "sys_vendor");
    let names = [read(dmi, "product_version"), read(dmi, "product_family")];
    let shown = names.iter().find(|s| !s.is_empty()).cloned()
        .unwrap_or_else(|| read(dmi, "product_name"));
    if !vendor.eq_ignore_ascii_case("LENOVO") {
        let what = [vendor.as_str(), shown.as_str()].iter().filter(|s| !s.is_empty()).copied()
            .collect::<Vec<_>>().join(" ");
        let what = if what.is_empty() { "no DMI information".to_owned() } else { what };
        return Err(format!("unsupported machine ({what}): Legion Power Manager runs only on \
                            Lenovo Legion, LOQ and IdeaPad Gaming laptops"));
    }
    if names.iter().any(|n| names_family(n) || names_cn_model(n)) {
        Ok(shown)
    } else {
        Err(format!("unsupported Lenovo model ({shown}): Legion Power Manager runs only on \
                     Legion, LOQ and IdeaPad Gaming laptops"))
    }
}

pub fn check() -> Result<String, String> { check_at(Path::new("/sys/class/dmi/id")) }

/// BIOS identity: "SMCN19WW" → prefix "SMCN" (the board family), version 19.
/// Lenovo reuses a prefix across every BIOS release of one board, so firmware
/// quirks are keyed on it (the scheme LenovoLegionToolkit uses).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bios { pub prefix: String, pub version: Option<u32> }

pub fn parse_bios(raw: &str) -> Bios {
    let raw = raw.trim();
    let head: String = raw.chars().take(4).collect();
    if head.len() < 4 || !head.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return Bios::default();
    }
    let rest = &raw[4..];
    let version = rest.as_bytes().windows(2).position(|w| w[0].is_ascii_digit() && w[1].is_ascii_digit())
        .and_then(|i| rest[i..i + 2].parse().ok());
    Bios { prefix: head, version }
}

pub fn bios_at(dmi: &Path) -> Bios { parse_bios(&read(dmi, "bios_version")) }
pub fn bios() -> Bios { bios_at(Path::new("/sys/class/dmi/id")) }

/// Known firmware bugs when switching platform profiles, as worked around by
/// LenovoLegionToolkit (PowerModeFeature):
///  - J2CN boards: Quiet → Performance directly misbehaves; go through Balanced.
///  - K1CN boards: leaving Custom directly misbehaves; step through another
///    mode first (Quiet via Performance, Balanced via Quiet, Performance via
///    Balanced, Extreme is simply written twice).
/// Returns the mode to write first (kernel names), or None.
pub fn profile_detour(bios: &Bios, from: &str, to: &str) -> Option<&'static str> {
    if bios.prefix.eq_ignore_ascii_case("J2CN") && from == "low-power" && to == "performance" {
        return Some("balanced");
    }
    if bios.prefix.eq_ignore_ascii_case("K1CN") && from == "custom" && to != "custom" {
        return match to {
            "low-power" => Some("performance"),
            "balanced" => Some("low-power"),
            "performance" => Some("balanced"),
            "max-power" => Some("max-power"),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dmi(vendor: &str, version: &str, family: &str) -> tempdir::Dir {
        let d = tempdir::Dir::new();
        for (f, v) in [("sys_vendor", vendor), ("product_version", version), ("product_family", family), ("product_name", "83RU")] {
            std::fs::write(d.0.join(f), format!("{v}\n")).unwrap();
        }
        d
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let p = std::env::temp_dir().join(format!("lpm-dmi-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
    }

    #[test]
    fn accepted() {
        for (ver, fam) in [("Legion Pro 7 16AFR10H", "Legion Pro 7 16AFR10H"), ("LOQ 15IRH8", ""),
                           ("", "IdeaPad Gaming 3 15ACH6"), ("Lenovo Legion Y540-15IRH", "Legion Y540"),
                           ("Legion Go 8APU1", "Legion Go"), ("Lenovo LOQ 15IAX9", "")] {
            let d = dmi("LENOVO", ver, fam);
            assert!(check_at(&d.0).is_ok(), "{ver} / {fam}");
        }
    }

    #[test]
    fn china_models() {
        for ver in ["Lenovo Y9000P IAX10", "R9000K", "Y7000P 2024", "GeekPro G5000 IAX10"] {
            assert!(check_at(&dmi("LENOVO", ver, "").0).is_ok(), "{ver}");
        }
        for ver in ["XY9000", "Y900", "Yoga 9000"] {
            assert!(check_at(&dmi("LENOVO", ver, "").0).is_err(), "{ver}");
        }
        assert!(check_at(&dmi("HP", "Y9000P", "").0).is_err());
    }

    #[test]
    fn bios_and_detours() {
        assert_eq!(parse_bios("SMCN19WW"), Bios { prefix: "SMCN".into(), version: Some(19) });
        assert_eq!(parse_bios("J2CN25WW\n"), Bios { prefix: "J2CN".into(), version: Some(25) });
        assert_eq!(parse_bios("1.19"), Bios::default());
        let (j2, k1, sm) = (parse_bios("J2CN25WW"), parse_bios("K1CN31WW"), parse_bios("SMCN19WW"));
        assert_eq!(profile_detour(&j2, "low-power", "performance"), Some("balanced"));
        assert_eq!(profile_detour(&j2, "balanced", "performance"), None);
        assert_eq!(profile_detour(&k1, "custom", "balanced"), Some("low-power"));
        assert_eq!(profile_detour(&k1, "custom", "custom"), None);
        assert_eq!(profile_detour(&sm, "custom", "balanced"), None);
        assert_eq!(profile_detour(&sm, "low-power", "performance"), None);
    }

    #[test]
    fn refused() {
        for (vendor, ver, fam) in [("LENOVO", "ThinkPad X1 Carbon Gen 11", "ThinkPad X1 Carbon Gen 11"),
                                   ("LENOVO", "IdeaPad 5 14ALC05", "IdeaPad 5 14ALC05"),
                                   ("LENOVO", "Yoga Slim 7", ""), ("Dell Inc.", "Legion", "Legion"),
                                   ("ASUSTeK COMPUTER INC.", "ROG Strix", ""), ("", "", ""),
                                   ("LENOVO", "Legionnaire 1", ""), ("LENOVO", "BLOQ 3", "")] {
            let d = dmi(vendor, ver, fam);
            assert!(check_at(&d.0).is_err(), "{vendor} {ver} / {fam}");
        }
    }
}
