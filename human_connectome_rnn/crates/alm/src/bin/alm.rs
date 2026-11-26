use alm::optimizer::{apply, rebalance_dv01};
use alm::report::{run_alm, AlmInput};
use alm::sample::sample_input;
use std::{env, fs, process};

fn usage() -> ! {
    eprintln!(
        "alm: asset-liability management\n\n\
         USAGE:\n  alm sample [--out FILE]                    write an illustrative input file (JSON)\n  \
         alm report FILE [--json]                   run the full ALM analysis\n  \
         alm rebalance FILE [--target-dv01 X] [--apply OUT]\n                                             \
         minimum-cost trades to reach an EVE DV01 target (default 0)"
    );
    process::exit(2)
}

fn load(path: &str) -> AlmInput {
    let s = fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        process::exit(1)
    });
    AlmInput::from_json(&s).unwrap_or_else(|e| {
        eprintln!("{e}");
        process::exit(1)
    })
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1))
}

fn fail<T>(r: alm::Result<T>) -> T {
    r.unwrap_or_else(|e| {
        eprintln!("error: {e}");
        process::exit(1)
    })
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("sample") => {
            let json = fail(sample_input().to_json());
            match flag(&args, "--out") {
                Some(p) => fs::write(p, json).unwrap_or_else(|e| {
                    eprintln!("cannot write {p}: {e}");
                    process::exit(1)
                }),
                None => println!("{json}"),
            }
        }
        Some("report") => {
            let path = args.get(1).unwrap_or_else(|| usage());
            let rep = fail(run_alm(&load(path)));
            if args.iter().any(|a| a == "--json") {
                println!("{}", fail(rep.to_json()));
            } else {
                print!("{rep}");
            }
            if !rep.all_limits_ok() {
                process::exit(3);
            }
        }
        Some("rebalance") => {
            let path = args.get(1).unwrap_or_else(|| usage());
            let mut input = load(path);
            let target: f64 = flag(&args, "--target-dv01").map(|s| s.parse().unwrap_or_else(|_| usage())).unwrap_or(0.0);
            let h = input.config.horizon_months;
            let plan = fail(rebalance_dv01(&input.balance_sheet, &input.curve, h, input.config.calibrate_to_book, target));
            println!("EVE DV01 {:+.4} -> {:+.4} (target {:+.4}), gross turnover {:.2}", plan.dv01_before, plan.dv01_after, plan.target_dv01, plan.gross_turnover);
            for t in &plan.trades {
                println!("  {:<20} {:+9.2}  -> {:9.2}", t.id, t.delta, t.new_balance);
            }
            if let Some(out) = flag(&args, "--apply") {
                input.balance_sheet = apply(&input.balance_sheet, &plan);
                fs::write(out, fail(input.to_json())).unwrap_or_else(|e| {
                    eprintln!("cannot write {out}: {e}");
                    process::exit(1)
                });
                println!("rebalanced input written to {out}");
            }
        }
        _ => usage(),
    }
}
