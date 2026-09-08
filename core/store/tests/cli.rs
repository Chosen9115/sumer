//! The CLI, as a user meets it: separate processes, a real adapter, and a
//! real profile on disk.
//!
//! These are the cases an in-process test cannot honestly make. `history`
//! printing N rows proves nothing if the store it reads is the same
//! in-memory object the refresh just filled; the lock proves nothing
//! unless a second process actually asks for it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use support::{repo_root, Scratch};

/// A workspace binary, found beside this test binary: an integration test
/// lives in `<target>/<profile>/deps/` and binaries in
/// `<target>/<profile>/`. `CARGO_BIN_EXE_*` only works inside the binary's
/// own package, and hardcoding `target/debug` is wrong under
/// `CARGO_TARGET_DIR` or `--release`.
fn binary(name: &str) -> PathBuf {
    let mut dir = std::env::current_exe().expect("a test binary has a path");
    dir.pop();
    dir.pop();
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// Builds a package rather than demanding it. Cargo cannot express a
/// dependency on another crate's BINARY on stable, so a test that only
/// asserted the file exists would pass off a stale artifact and fail on a
/// clean checkout.
fn build(package: &str) {
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "-p", package])
        .current_dir(repo_root())
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("cargo build -p {package} failed ({status})"),
        Err(e) => panic!("could not run cargo to build {package}: {e}"),
    }
}

struct Cli {
    binary: PathBuf,
    profile: PathBuf,
}

impl Cli {
    fn run(&self, args: &[&str]) -> (String, String, i32) {
        let output = Command::new(&self.binary)
            .arg("--profile")
            .arg(&self.profile)
            .args(args)
            .output()
            .expect("the sumer binary runs");
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.status.code().unwrap_or(-1),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (stdout, stderr, code) = self.run(args);
        assert_eq!(code, 0, "sumer {args:?} exited {code}\n{stdout}\n{stderr}");
        stdout
    }
}

fn wallets(path: &Path, addresses: &[&str]) {
    let document = serde_json::json!({
        "wallets": [{
            "resource_id": "vault",
            "label": "Vault",
            "addresses": addresses
        }]
    });
    std::fs::write(path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
}

const ADDRESS_A: &str = "17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ";
const ADDRESS_B: &str = "bc1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3qccfmv3";

/// **L4**, **L6** and **L8** against the real `sumer-bitcoin-adapter`,
/// replaying `adapters/bitcoin/corpus/basic` -- one scenario, because they
/// are one story: read a wallet, look at it from a fresh process, then
/// remove an address from it.
#[test]
fn the_bitcoin_adapter_end_to_end() {
    if !support::python3_available() {
        eprintln!("python3 not found -- the Bitcoin corpus needs no python, continuing");
    }
    build("sumer-store");
    build("sumer-bitcoin-adapter");
    let sumer = binary("sumer");
    let adapter = binary("sumer-bitcoin-adapter");
    assert!(
        sumer.is_file() && adapter.is_file(),
        "both binaries must exist after a successful build"
    );

    let scratch = Scratch::new("btc");
    let wallets_path = scratch.path().join("wallets.json");
    wallets(&wallets_path, &[ADDRESS_A, ADDRESS_B]);
    let corpus = repo_root().join("adapters/bitcoin/corpus/basic");

    let cli = Cli {
        binary: sumer,
        profile: scratch.path().join("profile"),
    };
    cli.ok(&["init"]);
    cli.ok(&[
        "connect",
        &adapter.to_string_lossy(),
        "--wallets",
        &wallets_path.to_string_lossy(),
        "--source",
        &format!("file:{}", corpus.display()),
    ]);

    let refreshed = cli.ok(&["refresh"]);
    assert!(
        refreshed.contains("44 new") && refreshed.contains("sweep: complete"),
        "first refresh: {refreshed}"
    );

    // --- L4: a FRESH PROCESS must print the N rows the refresh read. An
    // in-memory store would pass a same-process check and lose everything
    // on exit.
    let history = cli.ok(&["history"]);
    assert_eq!(
        history.lines().filter(|l| l.contains("  vault:")).count(),
        44,
        "history, run as its own process, prints every record the first refresh read"
    );

    // --- L6: the adapter's amount text, byte for byte. The corpus's
    // confirmed balance is 379983 satoshi; nothing anywhere may reformat,
    // scale, or round it.
    let balances = cli.ok(&["balances"]);
    assert!(
        balances.contains("379983 sat"),
        "the adapter's exact amount string must survive to the screen: {balances}"
    );
    assert!(
        balances.contains("live ·") && balances.contains(&format!("file:{}", corpus.display())),
        "rule 3: every figure carries its source and freshness on its line: {balances}"
    );

    // --- L3 again, through the CLI: nothing changed, nothing appended.
    let unchanged = cli.ok(&["refresh"]);
    assert!(
        unchanged.contains("0 new · 0 revised · 0 retracted · 44 unchanged"),
        "a second refresh against an unchanged provider appends nothing: {unchanged}"
    );

    // --- L8: remove an address. The wallet's `address_set_sha256` moves,
    // so the resource DEFINITION moved -- and the records that address
    // carried can never be re-emitted under it.
    wallets(&wallets_path, &[ADDRESS_A]);
    let shrunk = cli.ok(&["refresh"]);
    assert!(
        shrunk.contains("27 retracted") && shrunk.contains("sweep: complete"),
        "removing an address retracts what only that address carried: {shrunk}"
    );
    assert!(
        shrunk.contains("resource_definition_changed"),
        "the reason names the SOFTWARE change, not the provider: {shrunk}"
    );

    let after = cli.ok(&["history"]);
    assert_eq!(
        after.lines().filter(|l| l.contains("  vault:")).count(),
        17,
        "every record still present is re-activated; only the absent ones go"
    );

    // Nothing was deleted: `show` still explains a retracted record, and
    // names the crawl that caused it.
    let retracted_id = {
        let store = sumer_store::Store::open(&sumer_store::Profile::new(&cli.profile)).unwrap();
        let mut stmt = store
            .conn()
            .prepare("SELECT local_id FROM retraction LIMIT 1")
            .unwrap();
        stmt.query_row([], |row| row.get::<_, String>(0)).unwrap()
    };
    let shown = cli.ok(&["show", &retracted_id]);
    assert!(
        shown.contains("RETRACTED") && shown.contains("resource_definition_changed"),
        "show explains the absence and names its crawl: {shown}"
    );
    assert!(
        shown.contains("observation"),
        "the observation chain is still there -- nothing is ever deleted: {shown}"
    );
}

/// The single writer. A second writing command exits **3** and names the
/// profile; it does not wait, and it certainly does not proceed.
///
/// Two refreshes would each load a live set at the start and each derive
/// retractions from it at the end, so the one that finished second would
/// write an older crawl's conclusions over the newer one's -- re-activating
/// exactly what the other correctly retracted.
#[test]
fn a_second_writer_exits_three_and_a_reader_still_works() {
    build("sumer-store");
    let scratch = Scratch::new("lock");
    let cli = Cli {
        binary: binary("sumer"),
        profile: scratch.path().join("profile"),
    };
    cli.ok(&["init"]);

    let profile = sumer_store::Profile::new(&cli.profile);
    let held = profile.lock().expect("the first writer takes the lock");

    let (_, stderr, code) = cli.run(&["refresh"]);
    assert_eq!(code, 3, "a held profile lock is exit 3, not a merge");
    assert!(
        stderr.contains(&cli.profile.to_string_lossy().to_string()),
        "the refusal names the profile: {stderr}"
    );

    // Read-only commands do NOT take the lock: a report must still answer
    // while a refresh is running.
    let (_, _, status) = cli.run(&["status"]);
    assert_eq!(status, 0, "a read-only command is not blocked by a writer");

    drop(held);
    let (_, _, code) = cli.run(&["refresh"]);
    assert_ne!(code, 3, "the lock is released when its holder goes away");
}

/// **A `connect` whose adapter breaks the contract on the way out writes
/// nothing.**
///
/// `connect` is the one command whose result is durable. It records an argv
/// that every later `refresh` spawns and trusts, so an adapter that answers
/// `hello` and `resources.list` correctly and then breaks the wire contract
/// must not be written down: the next refresh would meet it as an
/// established adapter rather than as a candidate that failed its audition.
/// Gate condition (9) refuses to let such a connection license a retraction;
/// this refuses to let it become one we re-spawn.
///
/// The adapter writes an unterminated frame and exits. Nothing will ever
/// terminate it, so it is detectable only at end of stream -- after both
/// replies were delivered and judged good, which is exactly why discarding
/// `close()` hid it.
#[test]
fn connect_refuses_an_adapter_that_breaks_the_contract_on_exit() {
    if !support::python3_available() {
        eprintln!("python3 not found on PATH -- skipping");
        return;
    }
    build("sumer-store");
    let cli = Cli {
        binary: binary("sumer"),
        profile: Scratch::new("connect-violation").path().join("profile"),
    };
    cli.ok(&["init"]);

    let (stdout, stderr, code) = cli.run(&["connect", "python3", "-c", HONEST_THEN_TRUNCATED]);

    assert_ne!(
        code, 0,
        "connect must fail on a contract violation\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("broke the wire contract"),
        "the operator is told what happened, not just that it failed: {stderr}"
    );
    // The real assertion: the profile is untouched. A message the user can
    // read is worth little if the argv landed anyway.
    let listed = cli.ok(&["status"]);
    assert!(
        !listed.contains("fake-adapter"),
        "nothing was written to the profile:\n{listed}"
    );
}

/// Answers `hello` and `resources.list`, then writes a frame with no
/// terminating newline and exits.
const HONEST_THEN_TRUNCATED: &str = r#"
import sys, json
def send(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()
while True:
    line = sys.stdin.readline()
    if not line:
        break
    req = json.loads(line)
    i, op = req["id"], req["op"]
    if op == "hello":
        send({"id": i, "ok": {"protocol": "1", "adapter_id": "fake-adapter",
              "adapter_version": "0.1.0", "local_id_derivation": "fixture-literal@1",
              "capabilities": ["resources.list", "balances.read", "history.read", "status.read"],
              "max_in_flight": 1}})
    elif op == "resources.list":
        send({"id": i, "ok": {"resources": [{"resource_id": "acct", "provider_id": "p1",
              "kind": "bank_checking", "label": "Checking"}]}})
        sys.stdout.write('{"id": 99, "ok": {}')
        sys.stdout.flush()
        break
"#;
