//! Profile every CSV and emit one JSON line per column / finding summary.
use nexgraph::profile;
fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let mut fs: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().map_or(false, |x| x == "csv")).collect();
    fs.sort();
    for p in fs {
        let name = p.file_stem().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&p).unwrap();
        let t = std::time::Instant::now();
        let pr = profile::profile_csv(&text, &name).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1e3;
        for c in &pr.columns { println!("COL\t{}\t{}\t{:?}\t{:?}\t{}\t{}", name, c.name, c.kind, c.risk, c.distinct, c.missing); }
        let mut by: std::collections::BTreeMap<String, usize> = Default::default();
        for f in &pr.findings { let k = f.message.split(": ").nth(1).unwrap_or(&f.message).chars().take(40).collect::<String>(); *by.entry(format!("{:?}|{}", f.severity, k)).or_default() += 1; }
        println!("FILE\t{}\t{}\t{}\t{}\t{:.0}\t{:?}", name, pr.rows, pr.duplicate_rows, pr.findings.len(), ms, by);
    }
}
