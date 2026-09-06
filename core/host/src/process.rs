//! Spawning an adapter subprocess: env allowlist, stdio wiring, the
//! stderr drain, and exit detection.
//!
//! **Honest boundary** (spec/wire.md §9): this is a CRASH boundary, not a
//! security boundary. The adapter runs as the same uid, on the same
//! filesystem, with inherited file descriptors. The env allowlist below is
//! hygiene -- it reduces what an adapter accidentally sees by default -- not
//! a defense against a hostile one; a same-uid process can read the host's
//! environment by other means (`/proc`, etc.) regardless of what we choose
//! to set here.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::mux::{Mux, Terminal};

/// Environment variables copied from the host's own environment into the
/// adapter's, if present. Deliberately small: an adapter that never needed
/// `AWS_SECRET_ACCESS_KEY` (or any other ambient secret) should not receive
/// it by default. `PATH` is required to resolve an interpreter (`python3`,
/// `bash`, ...); the rest are common runtime-behavior knobs a well-behaved
/// interpreter or script may expect.
const ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR"];

/// How long a natural exit waits for the reader loop to reach end of
/// stream. Normally instant: the exit closed stdout, so the reader is one
/// EOF from returning. It is not instant when something that inherited the
/// write end outlives the process -- and that case is now reported
/// ([`ProtocolViolationKind::StdoutHeldOpen`]) rather than waited out and
/// then papered over as an ordinary exit.
///
/// ponytail: a fixed bound, not the connection's deadline. An honest drain
/// finishes in microseconds; raising this only buys patience for a stream
/// that is already unjudgeable. Make it a parameter if a real adapter ever
/// needs more than a second to close a pipe it already stopped writing to.
const READER_DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

/// A freshly spawned adapter process with its stdio handles already split
/// out. `stdin`/`stdout` are owned by the caller (the mux's writer and
/// reader loops); `child` remains for waiting on exit.
pub struct SpawnedAdapter {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
}

/// Spawns `argv[0]` with `argv[1..]` as arguments. `extra_env` is layered on
/// top of the fixed allowlist -- e.g. a conformance runner forwarding
/// `SUMER_FIXTURE`/`SUMER_FIXTURE_RUN` to a fake adapter. This crate does not
/// know or care what those variables mean; it only decides which of the
/// *host's own* variables leak through by default.
///
/// # Errors
/// Returns the `std::io::Error` from the underlying `spawn()` (e.g. the
/// program does not exist or is not executable).
pub fn spawn(
    argv: &[String],
    extra_env: impl IntoIterator<Item = (String, String)>,
) -> std::io::Result<SpawnedAdapter> {
    let (program, args) = argv.split_first().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty adapter argv")
    })?;

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    cmd.env_clear();
    for key in ENV_ALLOWLIST {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // A dropped `AdapterHandle` must not leave an orphaned adapter process
    // behind (crash isolation, not resource isolation, but still: no
    // auto-restart also means no accidental immortality).
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("adapter child had no stdin pipe"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("adapter child had no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("adapter child had no stderr pipe"))?;

    tokio::spawn(drain_stderr(stderr));

    Ok(SpawnedAdapter {
        child,
        stdin,
        stdout,
    })
}

/// Reads stderr to completion and discards it. Nothing here parses adapter
/// log output (spec/wire.md §3); the drain exists so a chatty adapter can
/// never block on a full stderr pipe, and nothing retains the bytes, so it
/// cannot grow host memory either.
async fn drain_stderr(mut stderr: tokio::process::ChildStderr) {
    let mut chunk = [0_u8; 4096];
    while let Ok(n) = stderr.read(&mut chunk).await {
        if n == 0 {
            return;
        }
    }
}

/// The sole owner of the `Child` handle for its whole life: races a natural
/// exit against a kill request from elsewhere (the reader loop, on a fatal
/// protocol violation; or [`crate::AdapterHandle`]'s `Drop`, for an ordinary
/// shutdown), so nothing else ever needs `&mut Child` concurrently.
///
/// - Natural exit -> [`Terminal::Crashed`], but only after the reader loop
///   has finished. The child's exit closed its stdout, so `reader` is
///   about to return, and whatever it read out of the last bytes -- a
///   malformed frame, a duplicate id -- is published before it does.
///   Latching `Crashed` without waiting for that would hand the caller
///   "gone, and nothing established why" while the host in fact killed the
///   adapter for a violation it can name, and `Mux::finish` keeps the
///   first reason, so the truer one would arrive too late to matter.
///   There is no auto-restart (frozen contract, section (f)): a crashed
///   adapter stays crashed for the life of the handle.
/// - Natural exit whose stdout does **not** reach end of stream within
///   [`READER_DRAIN`] -> [`ProtocolViolationKind::StdoutHeldOpen`]. The
///   process exiting is not the stream ending: a process that inherited
///   the write end keeps it open, and what it writes next arrives after
///   any verdict latched here. Reporting `Crashed(code)` in that case
///   would certify a complete drain that did not happen -- which is
///   exactly how a forked writer's garbage used to reach the wire after a
///   passing verdict.
/// - `Some(Some(kind))` on `kill_tx` -> the child is killed and the mux is
///   finished with [`Terminal::Violation`].
/// - Channel closed, or `Some(None)` -> a deliberate, non-violation
///   shutdown (hello failed, or the handle was dropped): the child is
///   killed, but nothing is reported -- the caller already knows why.
///
/// [`Mux::finish`] is idempotent (only the first reason sticks), so a race
/// between these two paths -- the process happens to exit right as a kill
/// is requested -- can never produce two conflicting terminal reasons.
///
/// The three facts this function keeps apart, because collapsing them is
/// how an honest adapter gets blamed for the wrong thing: **the process
/// exited** (`exited`, published the instant it is reaped), **the stream
/// validated to its end** (the reader join completing), and **the drain
/// ran out of time** (the [`READER_DRAIN`] timeout). `exited` is what lets
/// [`crate::AdapterHandle::close`] tell "still running past stdin EOF"
/// from "exited, stream outlived it" instead of labelling both
/// `StdinEofIgnored`.
pub async fn supervise(
    mut child: Child,
    mut kill_rx: tokio::sync::mpsc::Receiver<Option<sumer_wire::ProtocolViolationKind>>,
    mux: Arc<Mux>,
    reader: tokio::task::JoinHandle<()>,
    exited: Arc<AtomicBool>,
) {
    tokio::select! {
        status = child.wait() => {
            let code = status.ok().and_then(|s| s.code());
            exited.store(true, Ordering::SeqCst);
            let drained = tokio::time::timeout(READER_DRAIN, reader).await.is_ok();
            match kill_rx.try_recv() {
                Ok(Some(kind)) => mux.finish(Terminal::Violation(kind)),
                // The reader is still reading a stdout the exit did not
                // close. Nothing establishes what else may arrive, so
                // nothing here may say the connection ended cleanly.
                _ if !drained => mux.finish(Terminal::Violation(
                    sumer_wire::ProtocolViolationKind::StdoutHeldOpen,
                )),
                _ => mux.finish(Terminal::Crashed(code)),
            };
        }
        msg = kill_rx.recv() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            exited.store(true, Ordering::SeqCst);
            if let Some(Some(kind)) = msg {
                mux.finish(Terminal::Violation(kind));
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_established_violation_outranks_a_process_that_already_exited() {
        // No sleep: this is deliberately a race, not a fixed schedule, and
        // it is deterministic anyway. `supervise`'s `child.wait()` branch
        // awaits the reader task's `JoinHandle` before it ever calls
        // `kill_rx.try_recv()` -- so if that branch wins the race, the
        // reader (below) has *already* published its violation by the
        // time `try_recv` runs, by construction of the join, not by
        // timing. If the `kill_rx.recv()` branch wins instead, it reads
        // the same `Some(kind)` message directly. Both branches publish
        // `Terminal::Violation` either way, so which one actually wins
        // this particular run cannot change the outcome -- which is
        // exactly what makes a wall-clock sleep unnecessary here: an
        // adapter that emits a malformed frame and exits in the same
        // breath must not have `AdapterCrashed` latch first and reject the
        // truer reason -- the host killed it for a violation it can name
        // -- as a late second opinion, regardless of which branch the
        // scheduler happens to pick.
        let spawned = spawn(&["true".to_owned()], std::iter::empty()).unwrap();
        let (kill_tx, kill_rx) = tokio::sync::mpsc::channel(1);
        let mux = Mux::spawn(spawned.stdin, kill_tx.clone());
        let reader = tokio::spawn(async move {
            let _ = kill_tx
                .send(Some(sumer_wire::ProtocolViolationKind::NotJson))
                .await;
        });

        supervise(
            spawned.child,
            kill_rx,
            mux.clone(),
            reader,
            Arc::new(AtomicBool::new(false)),
        )
        .await;

        match mux.terminal() {
            Some(Terminal::Violation(sumer_wire::ProtocolViolationKind::NotJson)) => {}
            other => panic!("expected the violation to win, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_rejects_empty_argv() {
        match spawn(&[], std::iter::empty()) {
            Err(err) => assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput),
            Ok(_) => panic!("expected empty argv to be rejected"),
        }
    }

    #[tokio::test]
    async fn env_allowlist_hides_unlisted_variables() {
        // ponytail: a self-check that the allowlist actually restricts the
        // child's environment, using `env` itself as the "adapter" so no
        // fixture script is needed for this one assertion.
        std::env::set_var("SUMER_HOST_TEST_SECRET", "leak-if-broken");
        let mut spawned =
            spawn(&["env".to_owned()], std::iter::empty::<(String, String)>()).unwrap();
        let mut out = Vec::new();
        spawned.stdout.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("SUMER_HOST_TEST_SECRET"));
        let _ = spawned.child.wait().await;
    }

    #[tokio::test]
    async fn extra_env_is_forwarded() {
        let mut spawned = spawn(
            &["env".to_owned()],
            [("SUMER_HOST_TEST_EXTRA".to_owned(), "yes".to_owned())],
        )
        .unwrap();
        let mut out = Vec::new();
        spawned.stdout.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("SUMER_HOST_TEST_EXTRA=yes"));
        let _ = spawned.child.wait().await;
    }
}
