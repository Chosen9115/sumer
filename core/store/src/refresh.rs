//! `refresh`: spawn each connected adapter once, read its balances, and
//! sweep each of its resources.
//!
//! One connection per adapter per refresh -- condition (7) is about a
//! connection, and every resource of one adapter is swept over the same
//! one.
//!
//! **Freshness is derived, not marked.** A balance line is `live` iff the
//! read that wrote it is still its adapter's current one. The read is
//! opened once per adapter per refresh, before the spawn -- before
//! anything that can fail -- so a resource that was not read has no new
//! row and is stale *by construction*: a `balances.read` that failed, a
//! reply that left a category out, a resource the adapter stopped listing,
//! a `status.read` that never returned, a process that would not start,
//! and every failure nobody has thought of yet all produce the same
//! nothing, and nothing is exactly the right answer.
//!
//! This replaces a marker row written on each failing path. Four
//! adversarial rounds found four such paths that had been missed, which is
//! what a rule that has to be re-applied by hand at every exit looks like.
//! There is now no code to omit.

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
        // The read opens BEFORE the spawn, because the spawn is one of the
        // things that can fail. Nothing else in this function has to know
        // that: a refresh that never reaches an adapter writes no balance
        // row carrying this read, and every figure it did not refresh is
        // stale by construction rather than by remembering.
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
    // Same reason as in `refresh`, for the same cost: one line, on the one
    // path that always runs. Opening a second read for a refresh that
    // already opened one changes nothing -- only the current value is ever
    // compared against -- so this is not a duplicate rule, it is the rule
    // holding for the callers that spawn their own connection.
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
                let outcome = read
                    .statuses
                    .iter()
                    .find(|s| s.resource_id == balance.resource_id)
                    .map_or_else(|| "unknown".to_owned(), |s| outcome_label(&s.outcome));
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
