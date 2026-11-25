//! The P1 00 PIN stays out of the till, the POS phones and the command line: neither product crate
//! may list this crate as a dependency (a dev-dependency is test code only), and none of their
//! sources may name it or build a P1 00 PIN command by hand.

use std::path::Path;

fn dependencies_section(toml: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t == "[dependencies]" || (t.starts_with("[target.") && t.ends_with(".dependencies]"));
            continue;
        }
        if inside {
            out.push_str(t);
            out.push('\n');
        }
    }
    out
}

fn sources(dir: &Path, out: &mut Vec<(String, String)>) {
    for e in std::fs::read_dir(dir).expect("read_dir") {
        let p = e.expect("entry").path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push((p.display().to_string(), std::fs::read_to_string(&p).expect("read")));
        }
    }
}

#[test]
fn no_product_crate_can_reach_the_clear_pin() {
    let host = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("host");
    for krate in ["sidecar", "pos-core"] {
        let toml = std::fs::read_to_string(host.join(krate).join("Cargo.toml")).expect("Cargo.toml");
        assert!(
            !dependencies_section(&toml).contains("fedimint-card-old-card-pin"),
            "{krate} lists the P1 00 PIN crate as a dependency: no till or POS may send a P1 00 PIN"
        );
        let mut files = Vec::new();
        sources(&host.join(krate).join("src"), &mut files);
        assert!(!files.is_empty(), "{krate} has sources");
        for (path, text) in files {
            for banned in ["fedimint_card_old_card_pin", "INS_VERIFY_PIN", "INS_SET_PIN", "INS_CHANGE_PIN", "open_pin_old"] {
                assert!(!text.contains(banned), "{path} names {banned}: the product sends only the sealed PIN");
            }
        }
    }
}
