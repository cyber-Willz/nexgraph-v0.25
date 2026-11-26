//! Profile a CSV for identifiers and quality problems, and write a de-identified copy.
//!
//!   nexgraph-deid profile FILE.csv
//!   nexgraph-deid run FILE.csv --key-file KEY --out OUT.csv [--k 5] [--policy-out POLICY.json --label Stay]
//!
//! KEY is a file holding at least 32 hexadecimal digits. Keep it away from the data: with it, pseudonyms can
//! be re-created for anyone you can guess.

use nexgraph::deid::{deidentify_csv, k_anonymity, suppress_small_classes};
use nexgraph::profile::{profile_csv, residual_findings, suggest_config, suggest_policy_json};
use std::collections::HashMap;

fn die(m: &str) -> ! {
    eprintln!("error: {m}");
    std::process::exit(2)
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, file) = match (a.first(), a.get(1)) {
        (Some(c), Some(f)) if c == "profile" || c == "run" => (c.as_str(), f.as_str()),
        _ => die("usage: nexgraph-deid profile FILE.csv | run FILE.csv --key-file KEY --out OUT.csv [--k N] [--policy-out F --label L]"),
    };
    let mut o: HashMap<&str, &str> = HashMap::new();
    let mut i = 2;
    while i < a.len() {
        match a.get(i + 1) {
            Some(v) if a[i].starts_with("--") => {
                o.insert(&a[i], v);
                i += 2;
            }
            _ => die(&format!("bad argument {}", a[i])),
        }
    }
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| die(&format!("cannot read {file}: {e}")));
    let table = std::path::Path::new(file).file_stem().and_then(|s| s.to_str()).unwrap_or("table");
    let p = profile_csv(&text, table).unwrap_or_else(|e| die(&e.to_string()));
    print!("{}", p.report());
    if cmd == "profile" {
        return;
    }
    let keyfile = o.get("--key-file").unwrap_or_else(|| die("--key-file is required"));
    let hex = std::fs::read_to_string(keyfile).unwrap_or_else(|e| die(&format!("cannot read key: {e}")));
    let hex = hex.trim();
    if hex.len() < 32 || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        die("key file must hold at least 32 hexadecimal digits");
    }
    let key: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
    let out = o.get("--out").unwrap_or_else(|| die("--out is required"));
    let k: usize = o.get("--k").map(|s| s.parse().unwrap_or_else(|_| die("bad --k"))).unwrap_or(5);
    let (cfg, qi) = suggest_config(&p, &key);
    let (clean, rep) = deidentify_csv(&text, &cfg).unwrap_or_else(|e| die(&e.to_string()));
    println!("\nApplied rules:");
    for (c, r) in &cfg.rules {
        println!("- {c}: {r:?} ({} cells changed)", rep.cells_changed.get(c).copied().unwrap_or(0));
    }
    let after = profile_csv(&clean, table).unwrap_or_else(|e| die(&e.to_string()));
    let qi_after: Vec<&str> = qi.iter().filter(|q| after.column(q).is_some()).map(String::as_str).collect();
    let mut final_csv = clean.clone();
    if !qi_after.is_empty() {
        let before = k_anonymity(&clean, &qi_after).unwrap_or_else(|e| die(&e.to_string()));
        println!("\nQuasi-identifiers {qi_after:?}: {} records unique ({:.1}%), smallest group {}", before.unique_rows, 100.0 * before.fraction_unique(), before.min_k);
        let (s, touched) = suppress_small_classes(&clean, &qi_after, k).unwrap_or_else(|e| die(&e.to_string()));
        let fin = k_anonymity(&s, &qi_after).unwrap_or_else(|e| die(&e.to_string()));
        println!("After suppressing groups smaller than {k}: {touched} records touched, smallest group {}", fin.min_k);
        final_csv = s;
    }
    std::fs::write(out, &final_csv).unwrap_or_else(|e| die(&format!("cannot write {out}: {e}")));
    let resid = residual_findings(&final_csv, table).unwrap_or_default();
    // pseudonym columns are expected to look like ids; anything else High is a leftover
    let leftover: Vec<&String> = resid.iter().filter(|m| !m.contains("PatientId") && !m.contains("pseudonym")).collect();
    if leftover.is_empty() {
        println!("\nNo name/contact/address/SSN-shaped columns or values remain.");
    } else {
        println!("\nSTILL IDENTIFYING:");
        for m in leftover {
            println!("- {m}");
        }
    }
    println!("Wrote {out}. This is a technical aid, not a determination that the file meets HIPAA or any other standard.");
    if let Some(pf) = o.get("--policy-out") {
        let label = o.get("--label").copied().unwrap_or("Record");
        std::fs::write(pf, suggest_policy_json(&p, label, &"00".repeat(32))).unwrap_or_else(|e| die(&format!("cannot write policy: {e}")));
        println!("Wrote {pf}: replace pseudonym_key_hex with your own secret before use.");
    }
}
