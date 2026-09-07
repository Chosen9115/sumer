//! The SQLite schema (ADR 0002), versioned by `PRAGMA user_version`.
//!
//! Two rules run through every table here.
//!
//! **Money is the adapter's exact string.** Every money column is declared
//! `ANY` with `CHECK(typeof(...) = 'text')`. SQLite is dynamically typed: a
//! column declared `TEXT` will happily store the *number* `130000` if a
//! caller binds one, and the value that comes back out is then a float or
//! an integer that has already lost the adapter's scale (`"1.50"` becomes
//! `1.5`, `"0.1"` becomes a binary float).
//!
//! The column is `ANY` and not `TEXT` for a reason that is easy to get
//! wrong, and this schema did get it wrong: **a `TEXT` column applies TEXT
//! affinity before the `CHECK` runs.** Binding the float `1.50` to a
//! `TEXT` column in a STRICT table converts it to the string `'1.5'`
//! first, so `typeof()` sees `'text'`, the `CHECK` passes, and the scale
//! is gone -- the guard reports success on precisely the input it names.
//! `ANY` is the one STRICT column type that applies no affinity, so the
//! bound value reaches the `CHECK` as the type the caller actually passed
//! and a number is rejected. `spec/money.md` §2 forbids a JSON number on
//! the wire for the same reason; this is the same rule one layer down, and
//! it is now enforced rather than merely declared.
//!
//! **Absence is host-authored and lives in its own table.** `retraction`
//! has no `provider_id`, no `surface`, no `amount` and no `posting` --
//! there is nothing for a host to put in them, and inventing values would
//! be fabricating provider evidence (frozen contract). It records the
//! revision it retracts, the reason, and the crawl that caused it.

use rusqlite::Connection;

/// The schema version this build writes and expects. A database stamped
/// with anything else is refused rather than guessed at.
pub const USER_VERSION: i64 = 1;

const DDL: &str = r"
CREATE TABLE adapter (
    adapter_id          TEXT PRIMARY KEY,
    argv                TEXT NOT NULL,
    -- The derivation the LAST hello reported. Metadata for `status`; the
    -- retraction gate never reads it (frozen contract: condition (7) gates
    -- on the per-sweep hello value only).
    local_id_derivation TEXT,
    needs_reauth        INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE resource (
    adapter_id       TEXT NOT NULL REFERENCES adapter(adapter_id),
    resource_id      TEXT NOT NULL,
    kind             TEXT NOT NULL,
    label            TEXT NOT NULL,
    -- SHA-256 over the resource DEFINITION as the adapter describes it
    -- (kind + provider_extra). A Bitcoin wallet's address set moves this.
    fingerprint      TEXT,
    -- The vantage the last sweep read from (`ResourceDescriptor.provider_id`).
    last_provider_id TEXT,
    PRIMARY KEY (adapter_id, resource_id)
) STRICT;

CREATE TABLE crawl (
    crawl_id            INTEGER PRIMARY KEY,
    adapter_id          TEXT NOT NULL,
    resource_id         TEXT NOT NULL,
    started_at          TEXT NOT NULL,
    -- NULL = this crawl began `page: None`. Gate condition (1) is exactly
    -- `start_page IS NULL`.
    start_page          TEXT,
    -- The resume point: the `next` of the last page that committed. NULL
    -- once drained. `refresh --resume` reads this and nothing else.
    next_page           TEXT,
    -- The per-sweep HELLO value and the fingerprint this crawl ran under.
    -- Both are recorded so `--resume` can DROP a cursor whose derivation or
    -- resource definition has moved underneath it.
    local_id_derivation TEXT NOT NULL,
    fingerprint         TEXT,
    drained             INTEGER NOT NULL DEFAULT 0,
    complete            INTEGER NOT NULL DEFAULT 0,
    disqualified_reason TEXT
) STRICT;

-- `resumable_crawl` selects the newest crawl of one resource that still
-- holds a cursor. Without this index that is a SCAN of every crawl ever
-- run, so `--resume` degrades as crawl history accumulates.
CREATE INDEX crawl_by_resource ON crawl (adapter_id, resource_id, crawl_id);

CREATE TABLE observation (
    observation_id         INTEGER PRIMARY KEY,
    adapter_id             TEXT NOT NULL,
    resource_id            TEXT NOT NULL,
    local_id               TEXT NOT NULL,
    revision               INTEGER NOT NULL,
    crawl_id               INTEGER NOT NULL REFERENCES crawl(crawl_id),
    last_seen_crawl        INTEGER NOT NULL REFERENCES crawl(crawl_id),
    derivation             TEXT NOT NULL,
    fingerprint            TEXT,
    content_hash           TEXT NOT NULL,
    provider_id            TEXT,
    supersedes_provider_id TEXT,
    state                  TEXT NOT NULL,
    tombstone_reason       TEXT,
    surface                TEXT NOT NULL,
    posting                TEXT NOT NULL,
    amount_asset           TEXT NOT NULL,
    amount                 ANY NOT NULL CHECK (typeof(amount) = 'text'),
    fees_asset             TEXT,
    fees                   ANY CHECK (fees IS NULL OR typeof(fees) = 'text'),
    raw_sign               TEXT NOT NULL,
    description            TEXT NOT NULL,
    provider_extra         TEXT NOT NULL,
    prov_adapter_id        TEXT NOT NULL,
    prov_provider_id       TEXT NOT NULL,
    prov_surface           TEXT NOT NULL,
    observed_at            TEXT NOT NULL,
    effective_at           TEXT,
    completeness           TEXT NOT NULL,
    received_at            TEXT NOT NULL,
    staleness              TEXT NOT NULL,
    UNIQUE (adapter_id, local_id, revision)
) STRICT;

CREATE INDEX observation_by_chain ON observation (adapter_id, local_id, observation_id);
CREATE INDEX observation_by_resource ON observation (adapter_id, resource_id, observation_id);

CREATE TABLE balance (
    balance_id       INTEGER PRIMARY KEY,
    adapter_id       TEXT NOT NULL,
    resource_id      TEXT NOT NULL,
    category         TEXT NOT NULL,
    canonical_hint   TEXT,
    -- NULL = the adapter looked and does not know. NEVER zero.
    amount_asset     TEXT,
    amount           ANY CHECK (amount IS NULL OR typeof(amount) = 'text'),
    prov_provider_id TEXT NOT NULL,
    prov_surface     TEXT NOT NULL,
    observed_at      TEXT NOT NULL,
    effective_at     TEXT,
    completeness     TEXT NOT NULL,
    received_at      TEXT NOT NULL,
    staleness        TEXT NOT NULL,
    outcome          TEXT NOT NULL
) STRICT;

CREATE INDEX balance_by_line ON balance (adapter_id, resource_id, category, balance_id);

CREATE TABLE retraction (
    retraction_id INTEGER PRIMARY KEY,
    adapter_id    TEXT NOT NULL,
    local_id      TEXT NOT NULL,
    -- The chain-head revision this retracts. A record is live iff its
    -- chain head's revision EXCEEDS every retraction revision for its key,
    -- so a later sweep appending revision N+1 revives it with no special
    -- case anywhere.
    revision      INTEGER NOT NULL,
    reason        TEXT NOT NULL,
    crawl_id      INTEGER NOT NULL REFERENCES crawl(crawl_id),
    retracted_at  TEXT NOT NULL
) STRICT;

CREATE INDEX retraction_by_key ON retraction (adapter_id, local_id, revision);

CREATE TABLE discrepancy (
    discrepancy_id INTEGER PRIMARY KEY,
    adapter_id     TEXT NOT NULL,
    resource_id    TEXT NOT NULL,
    kind           TEXT NOT NULL,
    crawl_id       INTEGER NOT NULL REFERENCES crawl(crawl_id),
    detail         TEXT NOT NULL,
    noted_at       TEXT NOT NULL
) STRICT;
";

/// Creates the schema in an empty database and stamps `user_version`.
pub fn create(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(DDL)?;
    conn.pragma_update(None, "user_version", USER_VERSION)?;
    Ok(())
}

/// The `user_version` stamped on this database (`0` for a fresh file).
pub fn version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The money `CHECK` must reject a bound *number*, not merely record
    /// that a number was converted to text on its way in.
    ///
    /// This is a regression test for a guard that did not work. With the
    /// columns declared `TEXT`, SQLite applied TEXT affinity BEFORE the
    /// `CHECK` ran: binding the float `1.50` stored `'1.5'`, `typeof()`
    /// answered `'text'`, and the constraint passed while the adapter's
    /// scale was destroyed -- the one outcome the module doc says is
    /// unrepresentable. Declaring the columns `ANY` removes the affinity
    /// so the value reaches the `CHECK` as the caller's own type.
    #[test]
    fn a_bound_number_cannot_reach_a_money_column() {
        let conn = Connection::open_in_memory().unwrap();
        create(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO crawl (crawl_id, adapter_id, resource_id, started_at,
                                local_id_derivation)
             VALUES (1, 'a', 'r', '2026-01-01T00:00:00Z', 'd')",
        )
        .unwrap();

        let insert = "INSERT INTO observation (
                adapter_id, resource_id, local_id, revision, crawl_id,
                last_seen_crawl, derivation, content_hash, state, surface,
                posting, amount_asset, amount, raw_sign, description,
                provider_extra, prov_adapter_id, prov_provider_id,
                prov_surface, observed_at, completeness, received_at,
                staleness)
             VALUES ('a','r','tx',?1,1,1,'d','h','active','s','p','USD',?2,
                     '+','','{}','a','p','s','2026-01-01T00:00:00Z',
                     'exact','2026-01-01T00:00:00Z','live')";

        // The float that used to slip through, and the integer beside it.
        for (revision, bad) in [(1, 1.50_f64.into()), (2, 150_i64.into())] {
            let bad: rusqlite::types::Value = bad;
            let err = conn.execute(insert, rusqlite::params![revision, bad]);
            assert!(
                err.is_err(),
                "a bound number reached the amount column as {bad:?} -- the \
                 CHECK is decorative and money has lost its scale"
            );
        }

        // The exact string still goes in, and comes back with its scale.
        conn.execute(insert, rusqlite::params![3, "1.50"]).unwrap();
        let (amount, kind): (String, String) = conn
            .query_row(
                "SELECT amount, typeof(amount) FROM observation",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((amount.as_str(), kind.as_str()), ("1.50", "text"));
    }
}
