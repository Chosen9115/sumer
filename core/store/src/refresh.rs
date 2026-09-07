//! `refresh`: spawn each connected adapter once, read its balances, and
//! sweep each of its resources.
//!
//! One connection per adapter per refresh -- condition (7) is about a
//! connection, and every resource of one adapter is swept over the same
//! one.
//!
//! **The read is opened before the spawn** -- before anything here that can
//! fail. That ordering is the whole of balance freshness: a resource this
//! refresh did not read has no row carrying the current read, so it is
//! stale by construction, on every failure path including the ones nobody
//! enumerated. `spec/observation.md` §2 states the rule;
//! `adr/0006-host-side-retraction.md` decision 8 records the marker scheme
//! it replaced and why that shape could not work.

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
        // Before the spawn, which is one of the things that can fail
        // (`spec/observation.md` §2).
        store::open_balance_read(store.conn(), &adapter.adapter_id)?;
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
    // The same rule holding for the callers that spawn their own
    // connection. A second open in one refresh changes nothing: §2
    // constrains the ordering, not the count.
    let read_id = store::open_balance_read(store.conn(), adapter_id)?;
    let hello = handle.hello();
    // **The connection must be the adapter we opened it for.**
    //
    // Everything below keys rows, chains and gate condition (8) by
    // `adapter_id` -- the id read out of SQLite. The connection announces
    // its own in HELLO. When those disagree the host is holding two notions
    // of one chain before a single observation has arrived, which is the
    // exact condition (8) exists to refuse; judging observations against
    // the id we REMEMBERED would let the impostor's page look native and
    // retract records it never spoke for.
    //
    // Refused as a whole rather than observation by observation: this is
    // not a bad record inside a good connection, it is a connection that
    // never claimed to be this adapter. There is nowhere honest to put
    // anything it says -- under the stored id the host would be recording a
    // provenance the connection denies, and under the announced id one
    // adapter would be writing another's history (`spec/wire.md` §10).
    if hello.adapter_id != adapter_id {
        return Err(StoreError::Host(format!(
            "hello announced adapter_id {:?} on the connection opened for {adapter_id:?}",
            hello.adapter_id
        )));
    }
    let hello_derivation = hello.local_id_derivation.clone();
    let listed = match handle.resources_list().await {
        Ok(listed) => listed,
        Err(e) => return Err(StoreError::Host(format!("resources.list failed: {e}"))),
    };
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
    let statuses = match handle.status_read(resource_ids.clone()).await {
        Ok(reply) => reply.statuses,
        Err(e) => return Err(StoreError::Host(format!("status.read failed: {e}"))),
    };
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
            for balance in &read.observations {
                // **A stored outcome is always the one the adapter
                // reported** (`spec/observation.md` §2). The host refuses a
                // reply that observes a resource it did not request, and
                // every requested resource carries exactly one status (§6),
                // so this lookup finds one; if it ever did not, the honest
                // answer is to say so rather than to invent an outcome and
                // stamp a figure with a freshness nobody gave it.
                let status = read
                    .statuses
                    .iter()
                    .find(|s| s.resource_id == balance.resource_id)
                    .ok_or_else(|| {
                        StoreError::Host(format!(
                            "balances.read reply carried a balance for resource {:?} with no \
                             status of its own",
                            balance.resource_id
                        ))
                    })?;
                let outcome = outcome_label(&status.outcome);
                store::append_balance(store.conn(), adapter_id, read_id, balance, &outcome)?;
            }
        }
        Err(e) => errors.push(format!("balances.read failed: {e}")),
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
