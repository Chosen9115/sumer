//! Where Esplora JSON comes from: a real HTTP deployment, or a recorded
//! corpus on disk. Two real implementations, not an interface with one.
//!
//! The corpus layout the `Replay` arm reads is documented in `README.md`
//! and is the same layout the `Http` arm writes under `--record`, so a
//! recording is replayable without editing.

use crate::map::{AddressStats, MapError, Tx};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use sumer_wire::ProviderDetail;

/// The `User-Agent` every outbound request carries.
///
/// Honest on purpose. Forging a browser string would be a lie told to a
/// free public service, and it would buy nothing: the disclosure that
/// matters is the set of addresses being queried, not the client name.
/// See `PRIVACY.md`.
pub const USER_AGENT: &str = "sumer-bitcoin/0.1";

/// Backoff used when a 429 carries no usable `Retry-After`.
const DEFAULT_RETRY_AFTER_MS: u64 = 60_000;

/// One request's whole budget. A wallet sync makes one request per address
/// per listing page, so a slow provider can still outrun the host's own
/// 30s deadline (`spec/wire.md` 7) on a large wallet -- see README.
const REQUEST_TIMEOUT_SECS: u64 = 15;

/// Esplora returns at most 25 confirmed transactions per page.
const CHAIN_PAGE: usize = 25;

/// A read that did not happen. Both variants suppress the whole sync:
/// there are no partial diffs.
#[derive(Debug)]
pub enum FetchError {
    /// HTTP 429. Distinct from `Unavailable` because `retry_after_ms` is
    /// the one fact the host can act on.
    RateLimited {
        retry_after_ms: u64,
        detail: ProviderDetail,
    },
    /// A connect error, a DNS failure, a 5xx, an unparseable body, or any
    /// other status this adapter cannot interpret.
    Unavailable { detail: ProviderDetail },
}

impl FetchError {
    #[must_use]
    pub fn detail(&self) -> &ProviderDetail {
        match self {
            FetchError::RateLimited { detail, .. } | FetchError::Unavailable { detail } => detail,
        }
    }

    fn unavailable(code: &str, message: String, raw: serde_json::Value) -> FetchError {
        FetchError::Unavailable {
            detail: ProviderDetail {
                code: code.to_owned(),
                message,
                raw,
            },
        }
    }
}

/// A 200 body, or the provider positively stating the resource is not
/// there. A 404 where a body was required is reported as `unavailable`
/// with an `http_404` detail, never guessed at -- see [`Source::get_json`].
pub enum Fetched {
    Body(String),
    NotFound,
}

pub enum Source {
    Http {
        base_url: String,
        agent: ureq::Agent,
        record: Option<PathBuf>,
    },
    Replay {
        dir: PathBuf,
        /// The pinned clock, from the corpus's `now` file. Absent means
        /// the wall clock, and a corpus without it is not reproducible.
        now: Option<i64>,
    },
}

impl Source {
    #[must_use]
    pub fn http(base_url: String, record: Option<PathBuf>) -> Source {
        let config = ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_global(Some(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS)))
            // A 404 and a 429 are answers this adapter reads, not errors
            // it aborts on: one is a resource that is not there, the other
            // carries a retry budget.
            .http_status_as_error(false)
            .build();
        Source::Http {
            base_url: base_url.trim_end_matches('/').to_owned(),
            agent: config.into(),
            record,
        }
    }

    /// `dir` is the corpus root; the run subdirectory is resolved here.
    #[must_use]
    pub fn replay(dir: &Path, run: u64) -> Source {
        let dir = dir.join(format!("run{run}"));
        let now = std::fs::read_to_string(dir.join("now"))
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok());
        if now.is_none() {
            eprintln!(
                "sumer-bitcoin-adapter: {}/now is missing or unreadable; \
                 falling back to the wall clock, so this corpus's observed_at \
                 timestamps are NOT reproducible",
                dir.display()
            );
        }
        Source::Replay { dir, now }
    }

    /// What goes in `provenance.provider_id` and every resource
    /// descriptor: which Esplora deployment answered.
    #[must_use]
    pub fn provider_id(&self) -> String {
        match self {
            Source::Http { base_url, .. } => base_url.clone(),
            Source::Replay { dir, .. } => format!("file:{}", dir.display()),
        }
    }

    /// Unix seconds. Pinned by a corpus's `now` file when replaying, so a
    /// replayed run is byte-reproducible; the wall clock otherwise.
    #[must_use]
    pub fn now(&self) -> i64 {
        match self {
            Source::Replay { now: Some(t), .. } => *t,
            _ => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|d| i64::try_from(d.as_secs()).ok())
                .unwrap_or(0),
        }
    }

    fn get(&self, path: &str) -> Result<Fetched, FetchError> {
        match self {
            Source::Http {
                base_url,
                agent,
                record,
            } => {
                let out = http_get(agent, &format!("{base_url}{path}"), path);
                if let Some(dir) = record {
                    record_response(dir, path, &out);
                }
                out
            }
            Source::Replay { dir, .. } => replay_get(dir, path),
        }
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, FetchError> {
        match self.get(path)? {
            Fetched::NotFound => Err(FetchError::unavailable(
                "http_404",
                format!("{path}: not found, where a body was required"),
                serde_json::Value::Null,
            )),
            Fetched::Body(body) => serde_json::from_str(&body).map_err(|e| {
                FetchError::unavailable(
                    "unparseable_body",
                    format!("{path}: {e}"),
                    serde_json::Value::String(body.chars().take(512).collect()),
                )
            }),
        }
    }

    /// `GET /address/:address` -- the funded/spent counters both balances
    /// are computed from.
    pub fn address_stats(&self, address: &str) -> Result<AddressStats, FetchError> {
        self.get_json(&format!("/address/{address}"))
    }

    /// `GET /address/:address/txs/mempool` -- up to 50 unconfirmed
    /// transactions. Esplora does not paginate this endpoint.
    pub fn address_mempool(&self, address: &str) -> Result<Vec<Tx>, FetchError> {
        self.get_json(&format!("/address/{address}/txs/mempool"))
    }

    /// Every confirmed transaction for one address, oldest page last.
    ///
    /// Esplora pages this endpoint newest-first, 25 at a time, continuing
    /// after a `last_seen_txid`; there is no "from height H" entry point,
    /// so a full crawl is the only way to reach old history.
    ///
    /// Called ONCE per crawl, not once per page: the result is held as a
    /// snapshot for the crawl's lifetime (`Adapter::crawls`). Both reasons
    /// matter -- the round trips would otherwise be repeated per page
    /// against the host's 30s deadline, and paginating over live data
    /// would let a transaction arriving mid-crawl shift every later page.
    ///
    /// ponytail: the FIRST page of a crawl still pays for the whole
    /// history -- one round trip per 25 transactions per address, serially
    /// -- so a very large wallet on a slow provider can still miss the
    /// host's deadline on page one. Slow is acceptable where dead was not.
    /// The upgrade path is PR 4's persistence: a durable crawl only has to
    /// fetch what arrived since the last one.
    pub fn address_chain(&self, address: &str) -> Result<Vec<Tx>, FetchError> {
        let mut out: Vec<Tx> = Vec::new();
        let mut last_seen: Option<String> = None;
        loop {
            let path = match &last_seen {
                None => format!("/address/{address}/txs/chain"),
                Some(t) => format!("/address/{address}/txs/chain/{t}"),
            };
            let page: Vec<Tx> = self.get_json(&path)?;
            let Some(last) = page.last().map(|t| t.txid.clone()) else {
                return Ok(out);
            };
            let short = page.len() < CHAIN_PAGE;
            out.extend(page);
            if short {
                return Ok(out);
            }
            if last_seen.as_deref() == Some(last.as_str()) {
                // The provider is not advancing. Refuse to loop forever,
                // and refuse to call a truncated crawl a complete one.
                return Err(FetchError::unavailable(
                    "pagination_stalled",
                    format!("{path}: the listing did not advance past {last}"),
                    serde_json::Value::Null,
                ));
            }
            last_seen = Some(last);
        }
    }
}

// ---------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------

fn http_get(agent: &ureq::Agent, url: &str, path: &str) -> Result<Fetched, FetchError> {
    let mut response = match agent.get(url).call() {
        Ok(r) => r,
        Err(e) => {
            return Err(FetchError::unavailable(
                "transport",
                format!("{path}: {e}"),
                serde_json::Value::Null,
            ))
        }
    };
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        // Seconds only. The HTTP-date form is legal and rare; reading it
        // would need a date parser, and the fixed backoff is a correct,
        // if blunter, answer.
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1_000));
    let body = response
        .body_mut()
        .read_to_string()
        .unwrap_or_else(|e| format!("<body unreadable: {e}>"));
    classify(status, body, retry_after, path)
}

/// How much of a provider's error body rides in `provider_detail.raw`.
///
/// The body is evidence and goes verbatim (`spec/observation.md` 7) -- but
/// *verbatim* is not *unbounded*. A status entry rides inside a frame with
/// a hard ceiling, and one provider answering 503 with a megabyte of HTML
/// would otherwise make the whole reply unwritable -- an oversized frame
/// being a fatal kill with no resync (`spec/wire.md` 2). 4 KiB is more of
/// an error page than any human reads, and what was dropped is stated in
/// the marker rather than hidden.
const MAX_DETAIL_BODY: usize = 4_096;

/// Truncates an error body to [`MAX_DETAIL_BODY`], on a UTF-8 boundary,
/// and says so. A `--record`ed corpus keeps the capped body: it is what the
/// adapter kept, and a recording that claimed more than the adapter ever
/// carried would replay differently from the run it recorded.
fn cap_body(body: String) -> String {
    if body.len() <= MAX_DETAIL_BODY {
        return body;
    }
    let mut end = MAX_DETAIL_BODY;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [truncated: {} bytes]", &body[..end], body.len())
}

/// Status to outcome. `provider_detail` carries the status and the body:
/// `spec/observation.md` 7 makes it evidence for a human, never something
/// this adapter or the host branches on. Capped at [`MAX_DETAIL_BODY`].
fn classify(
    status: u16,
    body: String,
    retry_after_ms: Option<u64>,
    path: &str,
) -> Result<Fetched, FetchError> {
    match status {
        200 => return Ok(Fetched::Body(body)),
        404 => return Ok(Fetched::NotFound),
        _ => {}
    }
    let capped = cap_body(body);
    let detail = |code: &str| ProviderDetail {
        code: code.to_owned(),
        message: format!("{path}: HTTP {status}"),
        raw: serde_json::json!({"status": status, "body": capped}),
    };
    if status == 429 {
        return Err(FetchError::RateLimited {
            retry_after_ms: retry_after_ms.unwrap_or(DEFAULT_RETRY_AFTER_MS),
            detail: detail("http_429"),
        });
    }
    Err(FetchError::Unavailable {
        detail: detail(&format!("http_{status}")),
    })
}

// ---------------------------------------------------------------------
// Corpus on disk
// ---------------------------------------------------------------------

/// The corpus file name for a request path: leading `/` dropped, every
/// remaining `/` replaced by `_`. `/address/bc1q.../txs/chain` becomes
/// `address_bc1q..._txs_chain`. Documented in README.md; the recorder and
/// the replayer share this one function so they cannot disagree.
#[must_use]
pub fn corpus_name(path: &str) -> String {
    path.trim_start_matches('/').replace('/', "_")
}

fn replay_get(dir: &Path, path: &str) -> Result<Fetched, FetchError> {
    let name = corpus_name(path);
    let json = dir.join(format!("{name}.json"));
    if let Ok(body) = std::fs::read_to_string(&json) {
        return Ok(Fetched::Body(body));
    }
    let status_file = dir.join(format!("{name}.status"));
    if let Ok(raw) = std::fs::read_to_string(&status_file) {
        let (head, body) = raw.split_once('\n').unwrap_or((raw.as_str(), ""));
        let Ok(status) = head.trim().parse::<u16>() else {
            return Err(FetchError::unavailable(
                "corpus_malformed",
                format!(
                    "{}: first line is not an HTTP status",
                    status_file.display()
                ),
                serde_json::Value::Null,
            ));
        };
        return classify(status, body.to_owned(), None, path);
    }
    // A corpus that does not answer a request the adapter made is a
    // corpus bug. Reporting it as unavailable suppresses the sync (never
    // a partial history out of a missing file) and the message names the
    // file to add.
    Err(FetchError::unavailable(
        "corpus_missing",
        format!(
            "{}: no recorded response ({} or {})",
            path,
            json.display(),
            status_file.display()
        ),
        serde_json::Value::Null,
    ))
}

fn record_response(dir: &Path, path: &str, out: &Result<Fetched, FetchError>) {
    let name = corpus_name(path);
    let (file, contents) = match out {
        Ok(Fetched::Body(body)) => (dir.join(format!("{name}.json")), body.clone()),
        Ok(Fetched::NotFound) => (dir.join(format!("{name}.status")), "404\n".to_owned()),
        Err(e) => {
            let d = e.detail();
            let status = d.raw.get("status").and_then(serde_json::Value::as_u64);
            let body = d
                .raw
                .get("body")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            match status {
                Some(s) => (dir.join(format!("{name}.status")), format!("{s}\n{body}")),
                // A transport failure has no status to record; there is
                // nothing faithful to write, so nothing is written.
                None => return,
            }
        }
    };
    if let Err(e) = std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&file, contents)) {
        eprintln!(
            "sumer-bitcoin-adapter: could not record {}: {e}",
            file.display()
        );
    }
}

impl From<MapError> for FetchError {
    fn from(e: MapError) -> FetchError {
        FetchError::unavailable("mapping", e.to_string(), serde_json::Value::Null)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn corpus_names_are_flat_and_unambiguous() {
        assert_eq!(corpus_name("/address/bc1q"), "address_bc1q");
        assert_eq!(
            corpus_name("/address/bc1q/txs/chain"),
            "address_bc1q_txs_chain"
        );
        assert_eq!(
            corpus_name("/address/bc1q/txs/chain/ab"),
            "address_bc1q_txs_chain_ab"
        );
        assert_eq!(corpus_name("/tx/ab"), "tx_ab");
    }

    #[test]
    fn a_404_is_not_found_and_a_429_is_rate_limited() {
        assert!(matches!(
            classify(404, String::new(), None, "/address/x"),
            Ok(Fetched::NotFound)
        ));
        assert!(matches!(
            classify(429, String::new(), Some(2_000), "/address/x"),
            Err(FetchError::RateLimited {
                retry_after_ms: 2_000,
                ..
            })
        ));
        assert!(matches!(
            classify(429, String::new(), None, "/address/x"),
            Err(FetchError::RateLimited {
                retry_after_ms: DEFAULT_RETRY_AFTER_MS,
                ..
            })
        ));
        assert!(matches!(
            classify(503, "down".to_owned(), None, "/address/x"),
            Err(FetchError::Unavailable { .. })
        ));
    }

    #[test]
    fn a_huge_error_body_is_capped_and_says_so() {
        let Err(e) = classify(503, "x".repeat(1_100_000), None, "/address/a") else {
            panic!("503 must not be a success");
        };
        let body = e.detail().raw["body"].as_str().unwrap();
        assert!(
            body.len() < MAX_DETAIL_BODY + 64,
            "{} bytes of evidence would make the reply unwritable",
            body.len()
        );
        assert!(body.starts_with("xxxx"), "the head of the body survives");
        assert!(
            body.ends_with("[truncated: 1100000 bytes]"),
            "what was dropped is stated, not hidden: {body:?}"
        );
    }

    #[test]
    fn a_capped_body_is_cut_on_a_character_boundary() {
        // A multi-byte character straddling the cap must not panic and
        // must not produce invalid UTF-8.
        let body = format!(
            "{}\u{00e9}{}",
            "a".repeat(MAX_DETAIL_BODY - 1),
            "b".repeat(10)
        );
        let capped = cap_body(body);
        assert!(capped.starts_with('a'));
        assert!(capped.contains("[truncated"));
    }

    #[test]
    fn provider_detail_carries_the_status_and_body_verbatim() {
        let Err(e) = classify(503, "maintenance".to_owned(), None, "/address/a") else {
            panic!("503 must not be a success");
        };
        assert_eq!(e.detail().code, "http_503");
        assert_eq!(e.detail().raw["body"], "maintenance");
        assert_eq!(e.detail().raw["status"], 503);
    }
}
