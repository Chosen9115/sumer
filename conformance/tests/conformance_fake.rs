//! The CI gate for Milestone 0: the conformance suite versus the reference
//! Python adapter (`adapters/fake/fake_adapter.py`). Every one of the
//! twelve fixtures under `conformance/cases/` must pass with zero
//! assertion failures, or this milestone's most valuable artifact -- the
//! suite itself -- has nothing behind it.
//!
//! Requires `python3` on `PATH`; skips (rather than failing red) when it is
//! not found, since that is an environment gap, not a suite regression.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

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
    assert_eq!(
        case_paths.len(),
        12,
        "expected all twelve conformance cases under {cases_dir:?}, found {case_paths:?}"
    );

    let mut failed_cases = Vec::new();
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
            failed_cases.push(outcome.case);
        }
    }
    assert!(
        failed_cases.is_empty(),
        "conformance cases failed: {failed_cases:?} (see stderr above for the concrete diffs)"
    );
}
