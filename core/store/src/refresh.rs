//! `refresh`: spawn each connected adapter once, read its balances, and
//! sweep each of its resources.
//!
//! One connection per adapter per refresh -- condition (7) is about a
//! connection, and every resource of one adapter is swept over the same
//! one.

use std::collections::{BTreeSet, HashSet};

use sumer_host::AdapterHandle;
use sumer_wire::ReadOutcome;

use crate::error::{Result, StoreError};
use crate::store::{self, Store};
use crate::sweep::{self, SweepOptions, SweepReport};

#[derive(Debug, Clone, Default)]
pub struct RefreshOptions {
    pub resume: bool,
    pub confirm_empty: bool,
    /// Refresh only this adapter. `None` refreshes every connected one.
    pub adapter_id: Option<String>,
}

/// Everything one connected adapter contributed to a refresh.
#[derive(Debug, Clone, Default)]
pub struct AdapterRefresh {
    pub sweeps: Vec<SweepReport>,
    /// Reads that failed for the adapter as a whole rather than for one
    /// resource -- a `balances.read` that never landed, say. Reported
    /// rather than logged and forgotten: a cron job that exits 0 after a
    /// failed read is a cron job nobody ever hears from.
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RefreshReport {
    pub sweeps: Vec<SweepReport>,
    /// Adapters that could not be reached at all. Reported, never silent.
    pub adapter_errors: Vec<(String, String)>,
}

impl RefreshReport {
    /// Whether anything failed -- the CLI's exit-1 condition.
    #[must_use]
    pub fn failed(&self) -> bool {
        !self.adapter_errors.is_empty() || self.sweeps.iter().any(|s| s.error.is_some())
    }
}

/// Runs a refresh over every connected adapter.
pub async fn refresh(store: &mut Store, options: &RefreshOptions) -> Result<RefreshReport> {
    let mut report = RefreshReport::default();
    let adapters = store::adapters(store.conn())?;
    if adapters.is_empty() {
        return Err(StoreError::Usage(
            "no adapters connected -- run `sumer connect <argv...>` first".to_owned(),
        ));
    }
    for adapter in adapters {
        if options
            .adapter_id
            .as_ref()
            .is_some_and(|wanted| *wanted != adapter.adapter_id)
        {
            continue;
        }
        let handle = match AdapterHandle::spawn(adapter.argv.clone(), []).await {
            Ok(handle) => handle,
            Err(e) => {
                report
                    .adapter_errors
                    .push((adapter.adapter_id.clone(), e.to_string()));
                continue;
            }
        };
        let sweeps = refresh_adapter(
            store,
            &handle,
            &adapter.adapter_id,
            SweepOptions {
                resume: options.resume,
                confirm_empty: options.confirm_empty,
            },
        )
        .await;
        // The connection is ended either way: `close` is what makes "the
        // adapter answered everything and then broke the protocol"
        // observable at all.
        let _ = handle.close().await;
        match sweeps {
            Ok(adapter_refresh) => {
                report.sweeps.extend(adapter_refresh.sweeps);
                for error in adapter_refresh.errors {
                    report
                        .adapter_errors
                        .push((adapter.adapter_id.clone(), error));
                }
            }
            Err(e) => report
                .adapter_errors
                .push((adapter.adapter_id.clone(), e.to_string())),
        }
    }
    Ok(report)
}

/// Everything one already-connected adapter contributes to a refresh.
/// Public so a test can drive it over a **recorded** connection and assert
/// on the wire itself.
pub async fn refresh_adapter(
    store: &mut Store,
    handle: &AdapterHandle,
    adapter_id: &str,
    options: SweepOptions,
) -> Result<AdapterRefresh> {
    let mut errors = Vec::new();
    let hello_derivation = handle.hello().local_id_derivation.clone();
    let listed = handle.resources_list().await?;
    let resource_ids: Vec<String> = listed
        .resources
        .iter()
        .map(|r| r.resource_id.clone())
        .collect();
    for descriptor in &listed.resources {
        store::upsert_resource(
            store.conn(),
            adapter_id,
            &descriptor.resource_id,
            &descriptor.kind,
            &descriptor.label,
        )?;
    }

    // `status.read` is what the history_start exemption reads. It is also
    // where `needs_reauth` comes from -- a credential the user must renew
    // is not a discrepancy, it is a fact about the connection.
    let statuses = handle.status_read(resource_ids.clone()).await?.statuses;
    let needs_reauth = statuses.iter().any(|s| {
        matches!(
            s.outcome,
            ReadOutcome::ReauthRequired | ReadOutcome::ScaRequired | ReadOutcome::Revoked
        )
    });
    store::set_needs_reauth(store.conn(), adapter_id, needs_reauth)?;

    // Balances are batched, never paginated, and are appended verbatim --
    // the adapter's exact amount string, or NULL for "looked, don't know".
    match handle.balances_read(resource_ids.clone()).await {
        Ok(read) => {
            // Coverage is keyed per (resource, CATEGORY). Per resource is
            // one level too coarse: `spec/observation.md` §2 documents
            // Teller as guaranteeing only that *at least one* of two
            // categories appears in any given response, so a wholly
            // successful read routinely drops a category the last one
            // carried -- and that category's last row goes on reading
            // `live` forever if the resource merely being mentioned counts
            // as covering it.
            let mut covered = HashSet::new();
            for balance in &read.observations {
                covered.insert((balance.resource_id.as_str(), balance.category.as_str()));
                let outcome = read
                    .statuses
                    .iter()
                    .find(|s| s.resource_id == balance.resource_id)
                    .map_or_else(|| "unknown".to_owned(), |s| outcome_label(&s.outcome));
                store::append_balance(store.conn(), adapter_id, balance, &outcome)?;
            }
            // Every category this reply said nothing about -- because the
            // resource carried an `unavailable`/`gone`/etc. status with
            // nothing attached, or because the reply simply left that one
            // category out. Whatever it last reported must not go on
            // reading `live` forever just because nothing rewrote it.
            for resource_id in &resource_ids {
                let reason = read
                    .statuses
                    .iter()
                    .find(|s| s.resource_id == *resource_id)
                    .map_or_else(
                        || "no_observation".to_owned(),
                        |s| outcome_label(&s.outcome),
                    );
                mark_unread(store.conn(), adapter_id, resource_id, &reason, &covered)?;
            }
        }
        Err(e) => {
            // A failed balances read never erases the last figure: every
            // category the resource has ever reported gets a fresh row
            // with the amount withheld and staleness downgraded, so
            // rendering's fallback (render.rs's rule 2) finds a non-live
            // row on top and shows the last observed figure marked stale
            // instead of replaying whatever staleness the last SUCCESSFUL
            // read happened to leave behind. It is still a failed read, so
            // it is reported and the process exits 1.
            for resource_id in &resource_ids {
                mark_unread(
                    store.conn(),
                    adapter_id,
                    resource_id,
                    &format!("balances.read failed: {e}"),
                    &HashSet::new(),
                )?;
            }
            errors.push(format!("balances.read failed: {e}"));
        }
    }

    let mut reports = Vec::new();
    for descriptor in &listed.resources {
        let fingerprint = sweep::resource_fingerprint(descriptor);
        let input = sweep::SweepInput {
            adapter_id,
            resource_id: &descriptor.resource_id,
            hello_derivation: &hello_derivation,
            fingerprint: &fingerprint,
            provider_id: &descriptor.provider_id,
            history_start: sweep::history_start(&statuses, &descriptor.resource_id),
            options,
        };
        reports.push(sweep::sweep_resource(store, handle, &input).await?);
    }
    Ok(AdapterRefresh {
        sweeps: reports,
        errors,
    })
}

/// A `balances.read` that produced nothing for a category of
/// `resource_id` -- the call failed outright, this reply had no
/// observation for the resource, or it named the resource and left this
/// one category out -- must not leave the LAST successful read's
/// staleness sitting there looking current. This re-records every
/// category `covered` does not name, figure withheld (never a zero, never
/// a guess), so the next render finds a non-live row on top and falls back
/// to the last known amount marked stale (or `unavailable`, if there never
/// was one) instead of reprinting a `live` line for a read that did not
/// happen this time.
///
/// **The row invents nothing.** It is written as a COPY of the row it
/// marks unread, so every adapter-authored column -- `canonical_hint`,
/// `provider_id`, `surface`, `observed_at`, `effective_at` -- carries the
/// adapter's own last value rather than something the host made up. That
/// is structural here, not a promise: a host that fills in provenance on
/// its own behalf is fabricating provider evidence, which is the failure
/// §8.4 gave retractions their own table to avoid, and this stream has no
/// separate table to move to.
///
/// The four columns that are NOT copied are the host's own: `amount` is
/// NULL because nothing was read, `received_at` is host-stamped by
/// definition (§1), `staleness` is host-computed and never on the wire
/// (§1), and `completeness` is set to the enum's explicit "no claim"
/// variant -- carrying a `complete` forward onto a read that never
/// happened would be the fabrication this function is avoiding.
///
/// A consumer tells a marker from an adapter row by `outcome`, the one
/// column the host authors even on the success path: a marker's always
/// begins `unread:`, and no adapter row's ever does.
///
/// A category with no balance history is left alone: there is no figure to
/// protect from looking falsely current.
fn mark_unread(
    conn: &rusqlite::Connection,
    adapter_id: &str,
    resource_id: &str,
    reason: &str,
    covered: &HashSet<(&str, &str)>,
) -> Result<()> {
    let history = store::balance_history(conn, adapter_id, resource_id)?;
    let categories: BTreeSet<&str> = history
        .iter()
        .map(|row| row.category.as_str())
        .filter(|category| !covered.contains(&(resource_id, *category)))
        .collect();
    let received_at = crate::now_rfc3339();
    for category in categories {
        // `'unknown'` and `'unavailable'` are the serde spellings of
        // `Completeness::Unknown` and `Staleness::Unavailable`; a rename
        // shows up at once as `from_spelling` refusing the row on its way
        // back out.
        //
        // The SQL lives here rather than in `store` because the copy is
        // the whole point: naming the adapter-authored columns in a
        // `SELECT` is what makes it impossible for this write to author
        // one. Round-tripping them through a `Balance` would mean the host
        // re-typing every value it must not choose.
        conn.execute(
            "INSERT INTO balance (
                adapter_id, resource_id, category, canonical_hint, amount_asset, amount,
                prov_provider_id, prov_surface, observed_at, effective_at, completeness,
                received_at, staleness, outcome
             )
             SELECT adapter_id, resource_id, category, canonical_hint, NULL, NULL,
                    prov_provider_id, prov_surface, observed_at, effective_at, 'unknown',
                    ?4, 'unavailable', ?5
             FROM balance
             WHERE adapter_id = ?1 AND resource_id = ?2 AND category = ?3
             ORDER BY balance_id DESC LIMIT 1",
            rusqlite::params![
                adapter_id,
                resource_id,
                category,
                received_at.as_str(),
                format!("unread:{reason}"),
            ],
        )?;
    }
    Ok(())
}

/// The wire name of an outcome, stored beside a balance line so rendering
/// can say whether the figure came from a read that succeeded.
fn outcome_label(outcome: &ReadOutcome) -> String {
    match outcome {
        ReadOutcome::Fetched { .. } => "fetched".to_owned(),
        ReadOutcome::NotFetched => "not_fetched".to_owned(),
        ReadOutcome::Stale { as_of } => format!("stale:{}", as_of.as_str()),
        ReadOutcome::RateLimited { .. } => "rate_limited".to_owned(),
        ReadOutcome::Unavailable => "unavailable".to_owned(),
        ReadOutcome::ReauthRequired => "reauth_required".to_owned(),
        ReadOutcome::Revoked => "revoked".to_owned(),
        ReadOutcome::Gone => "gone".to_owned(),
        ReadOutcome::ScaRequired => "sca_required".to_owned(),
    }
}
