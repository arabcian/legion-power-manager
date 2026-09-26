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
    if names.iter().any(|n| names_family(n)) {
        Ok(shown)
    } else {
        Err(format!("unsupported Lenovo model ({shown}): Legion Power Manager runs only on \
                     Legion, LOQ and IdeaPad Gaming laptops"))
    }
}

pub fn check() -> Result<String, String> { check_at(Path::new("/sys/class/dmi/id")) }

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
