//! The CI gate for Milestone 0: the conformance suite versus the reference
//! Python adapter (`adapters/fake/fake_adapter.py`).
//!
//! Two things are asserted, and the second is why the list below exists at
//! all: every fixture under `conformance/cases/` passes with **zero**
//! assertion failures, and the set of files on disk equals [`CASES`]. A
//! fixture dropped into the directory without being named here is a red
//! build rather than a case nobody notices is unrun.
//!
//! Requires `python3` on `PATH`; skips (rather than failing red) when it is
//! not found, since that is an environment gap, not a suite regression.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Every fixture this gate knows about. Must equal the `*.json` files in
/// `conformance/cases/`, exactly.
const CASES: &[&str] = &[
    "duplicate_events",
    "fdx_lossless",
    "interrupted_pagination",
    "large_amounts",
    "null_category",
    "oversized_observation",
    "pending_to_posted",
    "protocol_violations",
    "provider_json_number",
    "reorg_vanish",
    "stale_balance",
    "unsupported_op",
];

/// `CARGO_MANIFEST_DIR` is `<repo>/conformance`; the fixtures and the
/// Python adapter both live one level up, at the repo root.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("conformance/ has a parent directory")
        .to_path_buf()
}

#[tokio::test]
async fn fake_adapter_passes_every_case() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 not found on PATH -- skipping the fake-adapter conformance gate");
        return;
    }

    let root = repo_root();
    let argv = vec![
        "python3".to_owned(),
        root.join("adapters/fake/fake_adapter.py")
            .to_string_lossy()
            .into_owned(),
    ];
    let cases_dir = root.join("conformance/cases");

    let mut case_paths: Vec<PathBuf> = std::fs::read_dir(&cases_dir)
        .unwrap_or_else(|e| panic!("could not read {cases_dir:?}: {e}"))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    case_paths.sort();
    let on_disk: BTreeSet<String> = case_paths
        .iter()
        .filter_map(|p| Some(p.file_stem()?.to_string_lossy().into_owned()))
        .collect();
    assert_eq!(
        on_disk,
        CASES.iter().map(|s| (*s).to_owned()).collect(),
        "the fixtures in {cases_dir:?} and the CASES list have drifted apart"
    );

    let mut broken = Vec::new();
    for path in &case_paths {
        let outcome = sumer_conformance::runner::run_case(&argv, path).await;
        if !outcome.failures.is_empty() {
            for f in &outcome.failures {
                eprintln!(
                    "FAIL [{}] {}: [{}] {}",
                    outcome.case,
                    path.display(),
                    f.assertion,
                    f.message
                );
            }
            broken.push(outcome.case);
        }
    }
    assert!(
        broken.is_empty(),
        "conformance cases failed: {broken:?} (see stderr above for the concrete diffs)"
    );
}
