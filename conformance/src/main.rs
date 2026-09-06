//! `sumer-conformance`: black-box CLI for the conformance suite.
//!
//! ```text
//! cargo run -p sumer-conformance -- --adapter "<argv>" --cases <dir>
//! ```
//!
//! `<argv>` is the adapter's command line, split on whitespace (no shell
//! quoting is performed -- see `conformance/README.md` if your adapter's
//! path or arguments contain spaces). `<dir>` is a directory of `*.json`
//! fixtures in the `conformance/cases/` schema.

use sumer_conformance::runner;

fn parse_args() -> Result<(Vec<String>, std::path::PathBuf), String> {
    let mut adapter: Option<String> = None;
    let mut cases: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--adapter" => {
                adapter = Some(
                    args.next()
                        .ok_or_else(|| "--adapter needs a value".to_owned())?,
                );
            }
            "--cases" => {
                cases = Some(
                    args.next()
                        .ok_or_else(|| "--cases needs a value".to_owned())?,
                );
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    let adapter = adapter.ok_or_else(|| "missing --adapter \"<argv>\"".to_owned())?;
    let cases = cases.ok_or_else(|| "missing --cases <dir>".to_owned())?;
    let argv: Vec<String> = adapter.split_whitespace().map(str::to_owned).collect();
    if argv.is_empty() {
        return Err("--adapter argv must not be empty".to_owned());
    }
    Ok((argv, std::path::PathBuf::from(cases)))
}

#[tokio::main]
async fn main() {
    let (argv, cases_dir) = match parse_args() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("sumer-conformance: {msg}");
            eprintln!(
                "usage: cargo run -p sumer-conformance -- --adapter \"<argv>\" --cases <dir>"
            );
            std::process::exit(2);
        }
    };

    let entries = match std::fs::read_dir(&cases_dir) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("sumer-conformance: could not read --cases dir {cases_dir:?}: {e}");
            std::process::exit(2);
        }
    };
    let mut case_paths: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(std::ffi::OsStr::to_str) == Some("json"))
        .collect();
    case_paths.sort();

    if case_paths.is_empty() {
        eprintln!("sumer-conformance: no *.json cases found under {cases_dir:?}");
        std::process::exit(2);
    }

    let mut any_failed = false;
    for path in &case_paths {
        let outcome = runner::run_case(&argv, path).await;
        if outcome.failures.is_empty() {
            println!("PASS  {}", outcome.case);
        } else {
            any_failed = true;
            println!(
                "FAIL  {}  ({} assertion(s) failed)",
                outcome.case,
                outcome.failures.len()
            );
            for f in &outcome.failures {
                println!("      [{}] {}", f.assertion, f.message);
            }
        }
    }

    if any_failed {
        std::process::exit(1);
    }
}
