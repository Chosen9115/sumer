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

use sumer_host::{AdapterHandle, HostError, Terminal};
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
    //
    // The resource is `stale` in the same reply, which is the case the
    // degrade must not destroy: dropping a record says nothing about how
    // fresh the ones that remain are, so the freshness outcome stays put
    // and the degradation is reported beside it.
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
        \x20   'statuses': [{{'resource_id': 'acct1', 'outcome': {{'stale': {{'as_of': '2026-09-06T11:00:00Z'}}}},\n\
        \x20                 'page': {{'cursor_resumable': 'exact',\n\
        \x20                          'next': {{'kind': 'cursor', 'cursor': 'block-841000'}}}}}}]}}}})\n"
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
    let status = &reply.statuses[0];
    match &status.outcome {
        sumer_wire::ReadOutcome::Stale { as_of } => {
            assert_eq!(as_of.as_str(), "2026-09-06T11:00:00Z")
        }
        other => panic!("the degrade must not overwrite the freshness outcome, got {other:?}"),
    }
    match status.degraded.as_slice() {
        [degraded] => {
            assert_eq!(degraded.local_id.as_deref(), Some("huge"));
            assert!(
                degraded.bytes > 65_536,
                "the reported size is the real one, got {}",
                degraded.bytes
            );
        }
        other => panic!("exactly one dropped record must be reported, got {other:?}"),
    }
    assert_eq!(
        reply.observations[0].provenance.staleness,
        sumer_wire::Staleness::Cached,
        "a sibling of a dropped record keeps the freshness its own resource reported"
    );

    // Resumable means the cursor the adapter sent is still there, not
    // merely that some page object survived.
    let page = status.page.as_ref().expect("the resource stays resumable");
    match &page.next {
        Some(sumer_wire::PageRequest::Cursor { cursor }) => assert_eq!(cursor, "block-841000"),
        other => panic!("the resume cursor must survive the degrade, got {other:?}"),
    }
    assert_eq!(page.cursor_resumable, sumer_wire::CursorResumable::Exact);
}

#[tokio::test]
async fn an_adapter_side_degrade_does_not_destroy_the_staleness_it_reported() {
    // The adapter did the omitting itself (spec/observation.md §6 step 2),
    // so the host has nothing to measure -- and no chance to snapshot the
    // freshness first. `degraded` is a field of its own precisely so this
    // resource can say both things at once: what it is serving is old,
    // *and* one record was too large to serve at all.
    let script = format!(
        "{PRELUDE}\nhello_ok(read(), capabilities=['history.read'])\n\
         req = read()\n\
         send({{'id': req['id'], 'ok': {{\n\
        \x20   'observations': [{{'resource_id': 'acct1', 'local_id': 'small', 'state': 'active',\n\
        \x20                     'surface': 'checking', 'posting': 'posted',\n\
        \x20                     'amount': {{'asset': 'USD', 'amount': '1.00'}},\n\
        \x20                     'raw_sign': 'provider_positive', 'description': 'rent',\n\
        \x20                     'provenance': {{'adapter_id': 'a', 'provider_id': 'p', 'surface': 'checking',\n\
        \x20                                    'observed_at': '2026-09-06T12:00:00Z',\n\
        \x20                                    'completeness': 'complete'}}}}],\n\
        \x20   'statuses': [{{'resource_id': 'acct1',\n\
        \x20                 'outcome': {{'stale': {{'as_of': '2026-09-06T11:00:00Z'}}}},\n\
        \x20                 'degraded': [{{'local_id': 'huge', 'bytes': 260000}}]}}]}}}})\n"
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

    assert_eq!(
        reply.observations[0].provenance.staleness,
        sumer_wire::Staleness::Cached,
        "an adapter-side degrade must not make a cached sibling look live"
    );
    let status = &reply.statuses[0];
    assert!(matches!(
        status.outcome,
        sumer_wire::ReadOutcome::Stale { .. }
    ));
    assert_eq!(status.degraded.len(), 1, "the degrade is reported");
    assert_eq!(status.degraded[0].local_id.as_deref(), Some("huge"));
    assert_eq!(status.degraded[0].bytes, 260_000);
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

// ---------------------------------------------------------------------
// Duplicate keys, on the real transport
// ---------------------------------------------------------------------
//
// `serde_json::Value` collapses duplicate object keys (last one wins), so
// any decode that goes through `Value` before the typed decode silently
// erases them. These two cases pin both layers -- the envelope and the
// `ok` payload -- to the bytes the adapter actually wrote, through the
// real framer, the real reader loop and the real public API.

#[tokio::test]
async fn a_duplicate_ok_field_is_rejected_on_the_real_transport() {
    // Written raw: `json.dumps` cannot emit a duplicate key.
    let script = format!(
        "{PRELUDE}\nhello_ok(read())\nreq = read()\n\
         sys.stdout.write('{{\"id\": %d, \"ok\": {{\"wrong\": true}}, \"ok\": {{\"resources\": []}}}}\\n' % req['id'])\n\
         sys.stdout.flush()\nread()\n"
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
        "a repeated `ok` must not be collapsed into a well-formed reply, got {result:?}"
    );
}

#[tokio::test]
async fn a_duplicate_field_inside_the_ok_payload_is_rejected() {
    let script = format!(
        "{PRELUDE}\nhello_ok(read(), capabilities=['balances.read'])\nreq = read()\n\
         sys.stdout.write('{{\"id\": %d, \"ok\": {{\"observations\": [], \"statuses\": [], \"statuses\": [{{\"resource_id\": \"acct1\", \"outcome\": \"unavailable\"}}]}}}}\\n' % req['id'])\n\
         sys.stdout.flush()\nread()\n"
    );
    let handle = spawn(&script, Duration::from_secs(2))
        .await
        .expect("handshake");
    let result = handle.balances_read(vec!["acct1".to_owned()]).await;
    match result {
        Err(HostError::Wire(err)) => assert_eq!(
            err.code,
            sumer_wire::WireErrorCode::InvalidRequest,
            "a repeated key inside `ok` is a malformed reply, got {err:?}"
        ),
        other => panic!("a repeated `statuses` must not be collapsed, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// A named violation outranks an unexplained exit
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_violation_wins_over_an_immediately_following_exit() {
    // The schedule the previous fix left open: the adapter emits a
    // malformed frame and exits in the same breath, so `child.wait()` can
    // resolve before the reader loop's kill request is ever looked at. The
    // caller must still be told the truth -- the host killed this adapter
    // for a violation it can name -- not `AdapterCrashed`, which means
    // "gone, and nothing established why".
    let script = format!(
        "{PRELUDE}\nhello_ok(read())\nread()\n\
         sys.stdout.write('not json at all\\n')\nsys.stdout.flush()\nsys.exit(3)\n"
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
        "an established violation must outrank the exit that followed it, got {result:?}"
    );
}

// ---------------------------------------------------------------------
// The close boundary: stdin EOF is the END (spec/wire.md §7)
// ---------------------------------------------------------------------

#[tokio::test]
async fn an_adapter_that_exits_at_stdin_eof_closes_cleanly() {
    // `for line in sys.stdin` ends at EOF, so dropping the child's stdin
    // is enough to end this process. Its exit code is the terminal
    // reason, and no violation is held against it.
    let script = format!("{PRELUDE}\nhello_ok(read())\nfor line in sys.stdin:\n    pass\n");
    let handle = spawn(&script, Duration::from_secs(5))
        .await
        .expect("handshake");
    match handle.close().await {
        Some(Terminal::Crashed(Some(0))) => {}
        other => panic!("expected a clean exit at stdin EOF, got {other:?}"),
    }
}

#[tokio::test]
async fn an_adapter_that_ignores_stdin_eof_is_a_protocol_violation() {
    // The exact mistake spec/wire.md §7 names for Python: at EOF
    // `readline()` returns `""` forever, and a loop that reads that as
    // "nothing to read *yet*" never leaves. Nothing this process writes
    // from here answers a request or reaches a caller, and the host has
    // no lever left but the kill -- so the connection did not end, and
    // saying it ended for no known reason would be a lie.
    let script = format!(
        "{PRELUDE}\nimport time\nhello_ok(read())\nwhile True:\n    \
         line = sys.stdin.readline()\n    if not line:\n        time.sleep(0.05)\n        continue\n"
    );
    let handle = spawn(&script, Duration::from_millis(500))
        .await
        .expect("handshake");
    match handle.close().await {
        Some(Terminal::Violation(ProtocolViolationKind::StdinEofIgnored)) => {}
        other => panic!("expected StdinEofIgnored, got {other:?}"),
    }
}

#[tokio::test]
async fn trailing_garbage_with_a_newline_is_caught_at_the_close() {
    // The control for the test below: the same bytes, LF-terminated. The
    // decoder frames them, `NotJson` is reported, and the close boundary
    // sees a violation.
    let script = format!(
        "{PRELUDE}\nhello_ok(read())\nfor line in sys.stdin:\n    pass\n\
         sys.stdout.write('}} not a frame, and not the answer to anything\\n')\nsys.stdout.flush()\n"
    );
    let handle = spawn(&script, Duration::from_secs(5))
        .await
        .expect("handshake");
    match handle.close().await {
        Some(Terminal::Violation(ProtocolViolationKind::NotJson)) => {}
        other => panic!("expected NotJson, got {other:?}"),
    }
}

#[tokio::test]
async fn trailing_garbage_without_a_newline_is_caught_at_the_close() {
    // The same bytes with the LF removed. Nothing frames them, so the
    // decoder holds them as an in-progress frame and the stream ends
    // mid-frame -- output the host received, never judged, and drained by
    // the boundary that is supposed to be the end of the evidence.
    let script = format!(
        "{PRELUDE}\nhello_ok(read())\nfor line in sys.stdin:\n    pass\n\
         sys.stdout.write('}} not a frame, and not the answer to anything')\nsys.stdout.flush()\n"
    );
    let handle = spawn(&script, Duration::from_secs(5))
        .await
        .expect("handshake");
    match handle.close().await {
        Some(Terminal::Violation(ProtocolViolationKind::UnterminatedFrame)) => {}
        other => panic!("expected UnterminatedFrame, got {other:?}"),
    }
}

#[tokio::test]
async fn a_writer_that_outlives_the_adapter_is_not_a_clean_close() {
    // The adapter exits at stdin EOF, but a process it forked inherited
    // the stdout write end and writes long after. Awaiting the process
    // does not establish end of stream: the host cannot certify it read
    // everything, so it must not report an ordinary exit.
    let script = format!(
        "{PRELUDE}\nimport os, time\nhello_ok(read())\nfor line in sys.stdin:\n    pass\n\
         if os.fork() == 0:\n    time.sleep(1.5)\n    \
         sys.stdout.write('not JSON\\n')\n    sys.stdout.flush()\n    os._exit(0)\n"
    );
    let handle = spawn(&script, Duration::from_secs(5))
        .await
        .expect("handshake");
    match handle.close().await {
        Some(Terminal::Violation(ProtocolViolationKind::StdoutHeldOpen)) => {}
        other => panic!("expected StdoutHeldOpen, got {other:?}"),
    }
}

#[tokio::test]
async fn an_exited_adapter_is_never_blamed_for_ignoring_stdin_eof() {
    // Same shape, but with a deadline shorter than the drain bound, so the
    // close gives up while the drain is still running. The process exited
    // -- promptly and cooperatively -- and blaming it for staying alive
    // past stdin EOF would be an honest adapter failing for a lie. The
    // three facts are distinct: the process exited, the stream never
    // ended, the drain ran out of time.
    let script = format!(
        "{PRELUDE}\nimport os, time\nhello_ok(read())\nfor line in sys.stdin:\n    pass\n\
         if os.fork() == 0:\n    time.sleep(2)\n    os._exit(0)\n"
    );
    let handle = spawn(&script, Duration::from_millis(300))
        .await
        .expect("handshake");
    match handle.close().await {
        Some(Terminal::Violation(ProtocolViolationKind::StdoutHeldOpen)) => {}
        other => panic!("expected StdoutHeldOpen, got {other:?}"),
    }
}

/// **Every requested `resource_id` appears in `statuses` exactly once**
/// (spec/observation.md §6), on every read that carries statuses -- not
/// only on the one whose downstream reader happened to be audited.
///
/// A reply naming a resource twice is refused where it is decoded. Every
/// reader of a `statuses` array takes the first entry that matches, so an
/// adapter answering cleanly and then contradicting itself in the same
/// array gets judged on the clean half: `sumer-store`'s sweep reads a
/// complete page and retracts, and a balance line is labelled with an
/// outcome the resource also denied. One guard at the decode boundary is
/// what keeps every one of those readers from having to remember.
#[tokio::test]
async fn a_reply_naming_one_resource_twice_is_refused_on_every_read() {
    let statuses = "[{'resource_id': 'acct1', 'outcome': {'fetched': {'page_empty': True}}},\
                     {'resource_id': 'acct1', 'outcome': 'unavailable'}]";
    let script = format!(
        "{PRELUDE}\nhello_ok(read(), capabilities=['balances.read', 'status.read'])\n\
         for _ in range(2):\n\
        \x20   req = read()\n\
        \x20   send({{'id': req['id'], 'ok': {{'observations': [], 'statuses': {statuses}}}\n\
        \x20         if req['op'] == 'balances.read' else {{'statuses': {statuses}}}}})\n"
    );
    let handle = spawn(&script, Duration::from_secs(5))
        .await
        .expect("handshake");

    match handle.balances_read(vec!["acct1".to_owned()]).await {
        Err(HostError::Wire(err)) => assert!(
            err.message.contains("more than once"),
            "the refusal names the malformation, got {err:?}"
        ),
        other => panic!("a duplicate resource must be refused, got {other:?}"),
    }
    match handle.status_read(vec!["acct1".to_owned()]).await {
        Err(HostError::Wire(err)) => assert!(err.message.contains("more than once")),
        other => panic!("a duplicate resource must be refused, got {other:?}"),
    }
}
