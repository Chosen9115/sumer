//! Black-box supervisor tests: real `python3` subprocesses speaking the
//! wire protocol (or deliberately breaking it), driven through the public
//! `AdapterHandle` API only.
//!
//! Every adapter script is self-contained stdlib-only Python, inline via
//! `python3 -c`, mirroring `adapters/fake/fake_adapter.py`'s spirit at a
//! much smaller scale (this crate doesn't own fixtures -- that is the
//! conformance runner's job; these are just enough misbehavior to exercise
//! `sumer-host`'s own state machine).

// This whole file is test code (an integration test binary): unwrap/expect
// here panic the *test*, which is exactly the desired failure mode, not
// the "return an error instead of panicking" the workspace lint enforces
// for library code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use sumer_host::{AdapterHandle, HostError};
use sumer_wire::ProtocolViolationKind;

fn py(script: &str) -> Vec<String> {
    vec!["python3".to_owned(), "-c".to_owned(), script.to_owned()]
}

async fn spawn(script: &str, deadline: Duration) -> Result<AdapterHandle, HostError> {
    AdapterHandle::spawn_with_deadline(py(script), std::iter::empty(), deadline).await
}

/// Every script below defines `hello()`/`read()`/`send()` the same way;
/// this constant is prepended so each test only writes its own behavior.
const PRELUDE: &str = r#"
import sys, json
def send(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()
def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None
def hello_ok(req, **extra):
    body = {"protocol": "1", "adapter_id": "a", "adapter_version": "0.1",
            "capabilities": [], "local_id_derivation": "none@1"}
    body.update(extra)
    send({"id": req["id"], "ok": body})
"#;

// ---------------------------------------------------------------------
// Handshake and normal operation
// ---------------------------------------------------------------------

#[tokio::test]
async fn hello_succeeds_and_negotiates_capabilities() {
    let script = format!("{PRELUDE}\nhello_ok(read(), capabilities=['resources.list'])\n");
    let handle = spawn(&script, Duration::from_secs(2))
        .await
        .expect("handshake should succeed");
    assert_eq!(handle.hello().protocol, "1");
    assert_eq!(
        handle.hello().max_in_flight,
        1,
        "omitted max_in_flight defaults to 1"
    );
}

#[tokio::test]
async fn adapter_refusing_the_protocol_surfaces_as_a_wire_error() {
    let script = format!(
        "{PRELUDE}\nreq = read()\nsend({{'id': req['id'], 'err': {{'code': 'unsupported_protocol', 'message': 'nope'}}}})\n"
    );
    match spawn(&script, Duration::from_secs(2)).await {
        Err(HostError::Wire(err)) => {
            assert_eq!(err.code, sumer_wire::WireErrorCode::UnsupportedProtocol)
        }
        Ok(_) => panic!("expected the handshake to fail"),
        Err(other) => panic!("expected a wire err, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Pre-hello output: kill without resync, before the handshake even
// completes.
// ---------------------------------------------------------------------

#[tokio::test]
async fn pre_hello_stdout_output_kills_the_handshake() {
    let script = format!(
        "import sys\nsys.stdout.write('Starting up...\\n')\nsys.stdout.flush()\n{PRELUDE}\nhello_ok(read())\n"
    );
    match spawn(&script, Duration::from_secs(2)).await {
        Err(HostError::ProtocolViolation(ProtocolViolationKind::PreHelloOutput)) => {}
        Ok(_) => panic!("expected the handshake to fail"),
        Err(other) => panic!("expected PreHelloOutput, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Fatal, connection-ending violations (kill without resync)
// ---------------------------------------------------------------------

#[tokio::test]
async fn garbage_on_stdout_is_a_fatal_violation() {
    let script = format!(
        "{PRELUDE}\nhello_ok(read())\nread()\nsys.stdout.write('not json at all\\n')\nsys.stdout.flush()\n"
    );
    let handle = spawn(&script, Duration::from_secs(2))
        .await
        .expect("handshake");
    let result = handle.resources_list().await;
    assert!(
        matches!(
            result,
            Err(HostError::ProtocolViolation(ProtocolViolationKind::NotJson))
        ),
        "expected NotJson, got {result:?}"
    );
}

#[tokio::test]
async fn oversize_frame_is_a_fatal_violation() {
    let script = format!(
        "{PRELUDE}\nhello_ok(read())\nreq = read()\nsend({{'id': req['id'], 'ok': {{'pad': 'x' * 1_100_000}}}})\n"
    );
    let handle = spawn(&script, Duration::from_secs(2))
        .await
        .expect("handshake");
    let result = handle.resources_list().await;
    assert!(
        matches!(
            result,
            Err(HostError::ProtocolViolation(
                ProtocolViolationKind::OversizeFrame
            ))
        ),
        "expected OversizeFrame, got {result:?}"
    );
}

#[tokio::test]
async fn never_issued_id_is_a_fatal_violation() {
    let script = format!(
        "{PRELUDE}\nhello_ok(read())\nread()\nsend({{'id': 9999, 'ok': {{'resources': []}}}})\n"
    );
    let handle = spawn(&script, Duration::from_secs(2))
        .await
        .expect("handshake");
    let result = handle.resources_list().await;
    assert!(
        matches!(
            result,
            Err(HostError::ProtocolViolation(
                ProtocolViolationKind::UnknownId
            ))
        ),
        "expected UnknownId, got {result:?}"
    );
}

#[tokio::test]
async fn duplicate_reply_is_a_fatal_violation() {
    let script = format!(
        "{PRELUDE}\nhello_ok(read())\nreq = read()\nsend({{'id': req['id'], 'ok': {{'resources': []}}}})\nsend({{'id': req['id'], 'ok': {{'resources': []}}}})\n"
    );
    let handle = spawn(&script, Duration::from_secs(2))
        .await
        .expect("handshake");
    let r1 = handle.resources_list().await;
    assert!(
        r1.is_ok(),
        "the first, legitimate reply must still be delivered: {r1:?}"
    );
    // Give the reader loop time to process the duplicate and finish the mux.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let r2 = handle.resources_list().await;
    assert!(
        matches!(
            r2,
            Err(HostError::ProtocolViolation(
                ProtocolViolationKind::DuplicateId
            ))
        ),
        "expected DuplicateId, got {r2:?}"
    );
}

// ---------------------------------------------------------------------
// Crash and hang
// ---------------------------------------------------------------------

#[tokio::test]
async fn crash_mid_request_resolves_as_adapter_crashed() {
    let script = format!("{PRELUDE}\nhello_ok(read())\nread()\nsys.exit(3)\n");
    let handle = spawn(&script, Duration::from_secs(2))
        .await
        .expect("handshake");
    let result = handle.resources_list().await;
    match result {
        Err(HostError::AdapterCrashed { status }) => assert_eq!(status, Some(3)),
        other => panic!("expected AdapterCrashed{{status: Some(3)}}, got {other:?}"),
    }
}

#[tokio::test]
async fn hang_past_deadline_times_out() {
    let script = format!("{PRELUDE}\nhello_ok(read())\nread()\nimport time\ntime.sleep(60)\n");
    let handle = spawn(&script, Duration::from_millis(200))
        .await
        .expect("handshake");
    let result = handle.resources_list().await;
    assert!(
        matches!(result, Err(HostError::Timeout)),
        "expected Timeout, got {result:?}"
    );
}

// ---------------------------------------------------------------------
// Id lifecycle: a tombstoned id's late reply is discarded, and the
// connection survives to serve further requests correctly.
// ---------------------------------------------------------------------

#[tokio::test]
async fn tombstoned_reply_is_discarded_and_the_connection_survives() {
    // Delays the reply to the *first* request it ever sees by 1s (on a
    // background thread, so it keeps answering everything else
    // immediately) -- long enough to blow a 250ms deadline, but the
    // connection must still be usable afterward, and that late reply must
    // not be mistaken for anything else once it finally arrives.
    let script = format!(
        "{PRELUDE}\nimport threading, time\nhello_ok(read())\nfirst = {{}}\nlock = threading.Lock()\ndef late(rid):\n    time.sleep(1.0)\n    send({{'id': rid, 'ok': {{'resources': []}}}})\nwhile True:\n    req = read()\n    if req is None:\n        break\n    with lock:\n        is_first = 'id' not in first\n        if is_first:\n            first['id'] = req['id']\n    if is_first:\n        threading.Thread(target=late, args=(req['id'],), daemon=True).start()\n    else:\n        send({{'id': req['id'], 'ok': {{'resources': []}}}})\n"
    );
    let handle = spawn(&script, Duration::from_millis(250))
        .await
        .expect("handshake");

    let r1 = handle.resources_list().await;
    assert!(
        matches!(r1, Err(HostError::Timeout)),
        "expected the first call to time out: {r1:?}"
    );

    let r2 = handle.resources_list().await;
    assert!(
        r2.is_ok(),
        "the connection must still serve requests right after a timeout: {r2:?}"
    );

    // Let the deferred (now-tombstoned) reply actually arrive.
    tokio::time::sleep(Duration::from_millis(900)).await;

    let r3 = handle.resources_list().await;
    assert!(
        r3.is_ok(),
        "a late reply to a tombstoned id must be discarded, not kill the connection: {r3:?}"
    );
}

// ---------------------------------------------------------------------
// The deadline starts at send, not enqueue.
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_queued_requests_fresh_deadline_starts_only_once_it_is_sent() {
    // Serial (max_in_flight defaults to 1) and every reply is delayed
    // 200ms *from when the adapter reads that request*. With a 300ms
    // deadline: call1 is sent at ~t=0 and answered at ~t=200ms (fine).
    // call2 sits queued behind call1's sole permit and is only actually
    // written at ~t=200ms, then answered ~200ms later at ~t=400ms. That is
    // within budget *only* if call2's deadline started at send (window
    // [200,500)); if it had wrongly started at enqueue (window [0,300)),
    // call2 would time out.
    let script = format!(
        "{PRELUDE}\nimport time\nhello_ok(read())\nwhile True:\n    req = read()\n    if req is None:\n        break\n    time.sleep(0.2)\n    send({{'id': req['id'], 'ok': {{'resources': []}}}})\n"
    );
    let handle = spawn(&script, Duration::from_millis(300))
        .await
        .expect("handshake");
    let (r1, r2) = tokio::join!(handle.resources_list(), handle.resources_list());
    assert!(r1.is_ok(), "call1: {r1:?}");
    assert!(
        r2.is_ok(),
        "call2 must get a fresh deadline starting at send, not at enqueue: {r2:?}"
    );
}

// ---------------------------------------------------------------------
// max_in_flight: serial vs concurrent
// ---------------------------------------------------------------------

/// Both scripts record, for each incoming request, how many replies the
/// adapter had already *sent* at the moment that request *arrived* --
/// embedded in the reply as `resource_id` so the test can read it back.
/// Recording has to happen off the main read loop (a worker thread per
/// request, replying only after a delay) precisely so the *reading* of
/// request 2 is never artificially delayed by however long request 1 takes
/// to answer -- otherwise a purely sequential adapter would look "serial"
/// by construction regardless of whether the host actually dispatched both
/// concurrently. Under a serial host this must still come out strictly
/// alternating (0, then 1); under a concurrent one, both can read 0.
const ARRIVAL_COUNTER_LOOP: &str = r#"
lock = threading.Lock()
replied = [0]
def worker(rid, arrived_after):
    time.sleep(0.15)
    send({"id": rid, "ok": {"resources": [
        {"resource_id": str(arrived_after), "provider_id": "p", "kind": "k", "label": "l"}
    ]}})
    with lock:
        replied[0] += 1
while True:
    req = read()
    if req is None:
        break
    with lock:
        arrived_after = replied[0]
    threading.Thread(target=worker, args=(req["id"], arrived_after), daemon=True).start()
"#;

fn arrival_marker(reply: &sumer_wire::ResourcesListReply) -> String {
    reply.resources[0].resource_id.clone()
}

#[tokio::test]
async fn serial_adapter_never_receives_a_second_request_before_answering_the_first() {
    let script =
        format!("{PRELUDE}\nimport time, threading\nhello_ok(read())\n{ARRIVAL_COUNTER_LOOP}");
    let handle = spawn(&script, Duration::from_secs(2))
        .await
        .expect("handshake");
    assert_eq!(handle.hello().max_in_flight, 1);

    let (r1, r2) = tokio::join!(handle.resources_list(), handle.resources_list());
    let mut markers = vec![
        arrival_marker(&r1.expect("r1")),
        arrival_marker(&r2.expect("r2")),
    ];
    markers.sort();
    assert_eq!(
        markers,
        vec!["0".to_owned(), "1".to_owned()],
        "serial: the second request must arrive only after the first was answered"
    );
}

#[tokio::test]
async fn concurrent_adapter_can_receive_both_requests_before_either_is_answered() {
    let script = format!(
        "{PRELUDE}\nimport time, threading\nhello_ok(read(), max_in_flight=2)\n{ARRIVAL_COUNTER_LOOP}"
    );
    let handle = spawn(&script, Duration::from_secs(2))
        .await
        .expect("handshake");
    assert_eq!(handle.hello().max_in_flight, 2);

    let (r1, r2) = tokio::join!(handle.resources_list(), handle.resources_list());
    let markers = [
        arrival_marker(&r1.expect("r1")),
        arrival_marker(&r2.expect("r2")),
    ];
    assert!(
        markers.iter().all(|m| m == "0"),
        "concurrent: both requests should reach the adapter before either reply, got {markers:?}"
    );
}

// ---------------------------------------------------------------------
// Host-stamped provenance: received_at/staleness
// ---------------------------------------------------------------------

#[tokio::test]
async fn adapter_supplied_received_at_is_rejected_as_invalid_request() {
    let script = format!(
        "{PRELUDE}\nhello_ok(read(), capabilities=['balances.read'])\nreq = read()\nsend({{'id': req['id'], 'ok': {{\n    'observations': [{{\n        'resource_id': 'acct1', 'category': 'available', 'amount': None,\n        'provenance': {{'adapter_id': 'a', 'provider_id': 'p', 'surface': 's',\n                       'observed_at': '2026-01-01T00:00:00Z',\n                       'received_at': '2026-01-01T00:00:01Z',\n                       'completeness': 'complete'}}\n    }}],\n    'statuses': [{{'resource_id': 'acct1', 'outcome': {{'fetched': {{'page_empty': False}}}}}}]\n}}}})\n"
    );
    let handle = spawn(&script, Duration::from_secs(2))
        .await
        .expect("handshake");
    let result = handle.balances_read(vec!["acct1".to_owned()]).await;
    match result {
        Err(HostError::Wire(err)) => {
            assert_eq!(err.code, sumer_wire::WireErrorCode::InvalidRequest)
        }
        other => panic!("expected invalid_request, got {other:?}"),
    }
}

#[tokio::test]
async fn host_stamps_received_at_and_computes_live_staleness() {
    let script = format!(
        "{PRELUDE}\nhello_ok(read(), capabilities=['balances.read'])\nreq = read()\nsend({{'id': req['id'], 'ok': {{\n    'observations': [{{\n        'resource_id': 'acct1', 'category': 'available', 'amount': None,\n        'provenance': {{'adapter_id': 'a', 'provider_id': 'p', 'surface': 's',\n                       'observed_at': '2026-01-01T00:00:00Z',\n                       'completeness': 'complete'}}\n    }}],\n    'statuses': [{{'resource_id': 'acct1', 'outcome': {{'fetched': {{'page_empty': False}}}}}}]\n}}}})\n"
    );
    let handle = spawn(&script, Duration::from_secs(2))
        .await
        .expect("handshake");
    let result = handle
        .balances_read(vec!["acct1".to_owned()])
        .await
        .expect("balances_read");
    assert_eq!(result.observations.len(), 1);
    let provenance = &result.observations[0].provenance;
    assert_eq!(provenance.staleness, sumer_wire::Staleness::Live);
    assert!(provenance.received_at.as_str().ends_with('Z'));
}

// ---------------------------------------------------------------------
// Backpressure: the deadline bounds the write too, not just the wait
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_write_the_adapter_never_drains_expires_with_its_deadline() {
    // Hello, then the adapter stops reading stdin while staying alive. The
    // next request is larger than the pipe buffer, so the host blocks
    // inside `write_all` -- which must not outlive the request's deadline.
    let script = format!("{PRELUDE}\nhello_ok(read())\nimport time\ntime.sleep(10)\n");
    let handle = spawn(&script, Duration::from_millis(500))
        .await
        .expect("handshake");

    let started = std::time::Instant::now();
    let oversized_id = "x".repeat(1_000_000);
    let result = handle.status_read(vec![oversized_id]).await;
    let elapsed = started.elapsed();

    assert!(
        matches!(result, Err(HostError::Timeout)),
        "expected Timeout, got {result:?} after {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the deadline must bound the write itself; blocked for {elapsed:?}"
    );

    // The frame was cut mid-write, so the stream is unsynchronized: the
    // connection is torn down rather than resumed, and a later call says so
    // instead of writing the rest of an abandoned frame.
    let after = handle.resources_list().await;
    assert!(
        matches!(after, Err(HostError::AdapterCrashed { .. })),
        "a partial frame must end the connection, got {after:?}"
    );
}

// ---------------------------------------------------------------------
// Handshake: the adapter must pick a version the host actually offered
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_protocol_version_that_was_never_offered_is_rejected() {
    // The host offers ["1"]. "999" is not a negotiation, it is an adapter
    // answering a question nobody asked -- and every later frame would be
    // read under a contract neither side agreed to.
    let script = format!("{PRELUDE}\nhello_ok(read(), protocol='999')\nread()\n");
    match spawn(&script, Duration::from_secs(2)).await {
        Err(HostError::Wire(err)) => assert_eq!(
            err.code,
            sumer_wire::WireErrorCode::UnsupportedProtocol,
            "expected unsupported_protocol, got {err:?}"
        ),
        Err(other) => panic!("expected a wire error, got {other:?}"),
        Ok(handle) => panic!(
            "an unoffered protocol {:?} was accepted",
            handle.hello().protocol
        ),
    }
}

// ---------------------------------------------------------------------
// MAX_OBSERVATION_BYTES, enforced on real bytes
// ---------------------------------------------------------------------

#[tokio::test]
async fn an_observation_over_the_cap_is_dropped_and_reported() {
    // A genuinely oversized observation (a ~100 KiB description, which
    // truncating `provider_extra` cannot fix), alongside a normal sibling.
    // The page must survive: the sibling is delivered, the oversized record
    // is omitted, and the resource's status says why.
    let script = format!(
        "{PRELUDE}\nhello_ok(read(), capabilities=['history.read'])\n\
         def obs(local_id, description):\n\
        \x20   return {{'resource_id': 'acct1', 'local_id': local_id, 'state': 'active',\n\
        \x20           'surface': 'checking', 'posting': 'posted',\n\
        \x20           'amount': {{'asset': 'USD', 'amount': '1.00'}},\n\
        \x20           'raw_sign': 'provider_positive', 'description': description,\n\
        \x20           'provenance': {{'adapter_id': 'a', 'provider_id': 'p', 'surface': 'checking',\n\
        \x20                          'observed_at': '2026-09-06T12:00:00Z', 'completeness': 'complete'}}}}\n\
         req = read()\n\
         send({{'id': req['id'], 'ok': {{\n\
        \x20   'observations': [obs('small', 'rent'), obs('huge', 'D' * 100000)],\n\
        \x20   'statuses': [{{'resource_id': 'acct1', 'outcome': {{'fetched': {{'page_empty': False}}}},\n\
        \x20                 'page': {{'cursor_resumable': 'exact', 'next': None}}}}]}}}})\n"
    );
    let handle = spawn(&script, Duration::from_secs(5))
        .await
        .expect("handshake");
    let reply = handle
        .history_read(vec![sumer_wire::ResourceQuery {
            resource_id: "acct1".to_owned(),
            page: None,
        }])
        .await
        .expect("history.read");

    let local_ids: Vec<&str> = reply
        .observations
        .iter()
        .map(|o| o.local_id.as_str())
        .collect();
    assert_eq!(
        local_ids,
        vec!["small"],
        "the oversized record is omitted and the rest of the page continues"
    );

    assert_eq!(reply.statuses.len(), 1, "one status per requested resource");
    match &reply.statuses[0].outcome {
        sumer_wire::ReadOutcome::OversizedObservation { local_id, bytes } => {
            assert_eq!(local_id.as_deref(), Some("huge"));
            assert!(
                *bytes > 65_536,
                "the reported size is the real one, got {bytes}"
            );
        }
        other => panic!("expected oversized_observation, got {other:?}"),
    }
    assert!(
        reply.statuses[0].page.is_some(),
        "the resource stays resumable"
    );
}

#[tokio::test]
async fn staleness_comes_from_each_resources_own_status_outcome() {
    // One reply, three resources: `stale{as_of}` says the adapter is
    // serving data it already knows is old, `unavailable` says it has
    // nothing current at all, and `fetched` is a live read. Staleness is
    // still host-stamped -- the adapter cannot send it -- but the host
    // derives it from what the adapter did say, per resource.
    let script = format!(
        "{PRELUDE}\nhello_ok(read(), capabilities=['balances.read'])\n\
         def bal(resource_id):\n\
        \x20   return {{'resource_id': resource_id, 'category': 'available', 'amount': None,\n\
        \x20           'provenance': {{'adapter_id': 'a', 'provider_id': 'p', 'surface': 's',\n\
        \x20                          'observed_at': '2026-01-01T00:00:00Z',\n\
        \x20                          'completeness': 'complete'}}}}\n\
         req = read()\n\
         send({{'id': req['id'], 'ok': {{\n\
        \x20   'observations': [bal('old'), bal('live'), bal('dark')],\n\
        \x20   'statuses': [\n\
        \x20       {{'resource_id': 'old', 'outcome': {{'stale': {{'as_of': '2026-01-01T00:00:00Z'}}}}}},\n\
        \x20       {{'resource_id': 'live', 'outcome': {{'fetched': {{'page_empty': False}}}}}},\n\
        \x20       {{'resource_id': 'dark', 'outcome': 'unavailable'}}]}}}})\n"
    );
    let handle = spawn(&script, Duration::from_secs(5))
        .await
        .expect("handshake");
    let result = handle
        .balances_read(vec!["old".to_owned(), "live".to_owned(), "dark".to_owned()])
        .await
        .expect("balances_read");

    let staleness_of = |resource_id: &str| {
        result
            .observations
            .iter()
            .find(|b| b.resource_id == resource_id)
            .map(|b| b.provenance.staleness)
    };
    assert_eq!(
        staleness_of("old"),
        Some(sumer_wire::Staleness::Cached),
        "a `stale` status means the observations it covers are not live"
    );
    assert_eq!(
        staleness_of("live"),
        Some(sumer_wire::Staleness::Live),
        "a live resource in the same reply is unaffected"
    );
    assert_eq!(
        staleness_of("dark"),
        Some(sumer_wire::Staleness::Unavailable),
        "`unavailable` is not `cached`"
    );
}

#[tokio::test]
async fn a_failed_write_reports_the_real_terminal_reason_not_a_guess() {
    // The window this pins open deterministically: the adapter's stdin is
    // already unwritable (it closed fd 0), but the fatal duplicate reply
    // that ends the connection has not been sent yet -- so no terminal
    // reason is latched at the moment the host's write fails. A host that
    // answers that write failure by *guessing* `AdapterCrashed{None}`
    // resolves the caller before the truth is known, and the caller is told
    // the adapter died of unknown causes when in fact the host killed it
    // for a protocol violation it can name.
    let script = format!(
        "{PRELUDE}\nimport os, threading, time\n\
         hello_ok(read(), max_in_flight=2)\n\
         req = read()\n\
         os.close(0)\n\
         def late():\n\
        \x20   time.sleep(0.15)\n\
        \x20   send({{'id': req['id'], 'ok': {{'resources': []}}}})\n\
        \x20   send({{'id': req['id'], 'ok': {{'resources': []}}}})\n\
         threading.Thread(target=late, daemon=True).start()\n\
         time.sleep(3)\n"
    );
    let handle = spawn(&script, Duration::from_secs(3))
        .await
        .expect("handshake");

    // The second call is issued once the adapter has closed its stdin, so
    // its frame cannot be written at all, and it is issued before the
    // duplicate arrives, so nothing is latched yet when that write fails.
    let (first, second) = tokio::join!(handle.resources_list(), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.resources_list().await
    });

    assert!(
        matches!(
            second,
            Err(HostError::ProtocolViolation(
                ProtocolViolationKind::DuplicateId
            ))
        ),
        "a request whose write failed must report why the connection died, got {second:?}"
    );
    assert!(
        first.is_ok(),
        "the first, legitimate copy of that reply is still delivered: {first:?}"
    );
}
