//! The conformance suite versus the real `sumer-bitcoin-adapter`, replaying
//! the recorded corpora under `adapters/bitcoin/corpus/`.
//!
//! This is the same [`sumer_conformance::runner::run_case`] the Python
//! reference adapter is driven through, with nothing added for Bitcoin: the
//! fixtures under `conformance/cases/bitcoin/` carry an `expect` block and
//! **no `script`**, because there is no script to write -- the adapter's
//! replies come from a provider recording, not from a fixture.
//!
//! One of the two cases is two adapter lifetimes over one `--state-dir`
//! (record a balance, then meet a provider that has gone dark), which the
//! conformance runner has no notion of: it spawns one argv per execution.
//! That is what
//! `conformance/cases/bitcoin/two_phase.py` is for -- it makes "the phase-2
//! adapter, with a state file phase 1 actually wrote" a single command
//! line, and gives every launch a fresh state directory so the two
//! executions A9 compares are the same scenario twice.
//!
//! Requires `python3` on `PATH` (for that wrapper) and a built
//! `sumer-bitcoin-adapter` binary; `cargo test --workspace` builds it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Every Bitcoin fixture, and the corpus run indices it drives: all but the
/// last are priming lifetimes. Must equal the `*.json` files in
/// `conformance/cases/bitcoin/`, exactly.
const CASES: &[(&str, &str, &str)] = &[
    // (fixture, corpus, runs)
    ("btc_basic", "basic", "0"),
    ("btc_fetch_fail", "fetch_fail", "0,1"),
];

/// `CARGO_MANIFEST_DIR` is `<repo>/conformance`.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("conformance/ has a parent directory")
        .to_path_buf()
}

/// The adapter binary, found beside this test binary: an integration test
/// lives in `<target>/<profile>/deps/`, and workspace binaries in
/// `<target>/<profile>/`. `CARGO_BIN_EXE_*` only works within the binary's
/// own package, and hardcoding `target/debug` would be wrong under
/// `CARGO_TARGET_DIR` or `--release`.
fn adapter_binary() -> PathBuf {
    let mut dir = std::env::current_exe().expect("a test binary has a path");
    dir.pop(); // deps/
    dir.pop(); // <profile>/
    dir.join(format!(
        "sumer-bitcoin-adapter{}",
        std::env::consts::EXE_SUFFIX
    ))
}

/// The argv for one case: the two-phase wrapper, the adapter, and the
/// corpus. `--state-dir` is deliberately absent -- the wrapper appends a
/// fresh one per launch.
fn argv(root: &Path, binary: &Path, corpus: &str, runs: &str) -> Vec<String> {
    let corpus_dir = root.join("adapters/bitcoin/corpus").join(corpus);
    vec![
        "python3".to_owned(),
        root.join("conformance/cases/bitcoin/two_phase.py")
            .to_string_lossy()
            .into_owned(),
        runs.to_owned(),
        binary.to_string_lossy().into_owned(),
        "--wallets".to_owned(),
        corpus_dir
            .join("wallets.json")
            .to_string_lossy()
            .into_owned(),
        "--source".to_owned(),
        format!("file:{}", corpus_dir.display()),
    ]
}

#[tokio::test]
async fn the_bitcoin_adapter_passes_every_bitcoin_case() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 not found on PATH -- skipping the Bitcoin conformance gate");
        return;
    }
    let root = repo_root();
    let binary = adapter_binary();
    assert!(
        binary.is_file(),
        "{} is not built. Run `cargo test --workspace` (or `cargo build -p \
         sumer-bitcoin-adapter`): this gate drives the real adapter, and silently \
         skipping it would leave two cases unrun.",
        binary.display()
    );

    let cases_dir = root.join("conformance/cases/bitcoin");
    let on_disk: BTreeSet<String> = std::fs::read_dir(&cases_dir)
        .unwrap_or_else(|e| panic!("could not read {cases_dir:?}: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .filter_map(|p| Some(p.file_stem()?.to_string_lossy().into_owned()))
        .collect();
    assert_eq!(
        on_disk,
        CASES.iter().map(|(c, _, _)| (*c).to_owned()).collect(),
        "the fixtures in {cases_dir:?} and the CASES list have drifted apart"
    );

    let mut broken = Vec::new();
    for (case, corpus, runs) in CASES {
        let path = cases_dir.join(format!("{case}.json"));
        let outcome =
            sumer_conformance::runner::run_case(&argv(&root, &binary, corpus, runs), &path).await;
        if !outcome.failures.is_empty() {
            for f in &outcome.failures {
                eprintln!("FAIL [{case}] [{}] {}", f.assertion, f.message);
            }
            broken.push(*case);
        }
    }
    assert!(
        broken.is_empty(),
        "Bitcoin conformance cases failed: {broken:?} (see stderr above for the concrete diffs)"
    );
}
